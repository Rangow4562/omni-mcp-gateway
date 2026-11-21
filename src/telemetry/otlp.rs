//! OpenTelemetry OTLP JSON-over-HTTP batch exporter & live background worker (`OMNI_OTLP_ENDPOINT`, v6.5.0).
//!
//! Features:
//! - Bounded span queue (4096 cap) preventing unbounded memory growth
//! - Background batch flush worker woken every `OMNI_OTLP_FLUSH_INTERVAL_MS` (default `5000ms`)
//!   or immediately when `>= 128` spans are queued
//! - Latency histogram percentile calculation ($p_{50}, p_{95}, p_{99}$) & error-code counters
//! - Exponential retry backoff on transient OTLP collector failures.

use crate::data_plane::HttpDownstreamClient;
use crate::json::JsonValue;
use crate::logging::now_epoch_secs;
use crate::models::TraceSpan;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub const DEFAULT_OTLP_BATCH_THRESHOLD: usize = 128;
pub const MAX_OTLP_QUEUE_SPANS: usize = 4096;
const MAX_LATENCY_SAMPLES: usize = 2048;

#[derive(Clone, Debug)]
pub struct OtlpExportReport {
    pub endpoint: String,
    pub spans_exported: usize,
    pub ok: bool,
    pub error: Option<String>,
}

/// Bounded sliding-window latency histogram for computing $p_{50}, p_{95}, p_{99}$ and error counters.
#[derive(Clone, Debug, Default)]
pub struct LatencyHistogram {
    samples_us: VecDeque<u64>,
    error_counts: BTreeMap<String, u64>,
}

impl LatencyHistogram {
    pub fn record(&mut self, duration_us: u64, status: &str) {
        if self.samples_us.len() >= MAX_LATENCY_SAMPLES {
            self.samples_us.pop_front();
        }
        self.samples_us.push_back(duration_us);
        if !status.eq_ignore_ascii_case("OK") && !status.is_empty() {
            *self.error_counts.entry(status.to_string()).or_insert(0) += 1;
        }
    }

    pub fn percentiles(&self) -> (u64, u64, u64) {
        if self.samples_us.is_empty() {
            return (0, 0, 0);
        }
        let mut sorted: Vec<u64> = self.samples_us.iter().copied().collect();
        sorted.sort_unstable();
        let n = sorted.len();
        let p = |pct: f64| -> u64 {
            let idx = ((pct * (n as f64 - 1.0)).round() as usize).min(n - 1);
            sorted[idx]
        };
        (p(0.50), p(0.95), p(0.99))
    }

    pub fn error_counts(&self) -> &BTreeMap<String, u64> {
        &self.error_counts
    }
}

pub struct OtlpExporter {
    pub endpoint: String,
    pub service_name: String,
    client: HttpDownstreamClient,
}

impl Clone for OtlpExporter {
    fn clone(&self) -> Self {
        Self {
            endpoint: self.endpoint.clone(),
            service_name: self.service_name.clone(),
            client: self.client.clone(),
        }
    }
}

impl OtlpExporter {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            service_name: "omni-mcp-gateway".to_string(),
            client: HttpDownstreamClient::new(),
        }
    }

    pub fn from_env() -> Option<Self> {
        let ep = std::env::var("OMNI_OTLP_ENDPOINT").ok()?;
        let trimmed = ep.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(Self::new(trimmed))
    }

    pub fn build_otlp_payload(
        &self,
        spans: &[TraceSpan],
        metrics_counters: &[(&str, u64)],
    ) -> JsonValue {
        self.build_otlp_payload_with_histograms(spans, metrics_counters, None)
    }

    pub fn build_otlp_payload_with_histograms(
        &self,
        spans: &[TraceSpan],
        metrics_counters: &[(&str, u64)],
        histogram: Option<&LatencyHistogram>,
    ) -> JsonValue {
        let span_items: Vec<JsonValue> = spans
            .iter()
            .map(|s| {
                json!({
                    "traceId": s.trace_id.clone(),
                    "spanId": s.span_id.clone(),
                    "parentSpanId": s.parent_span_id.clone().unwrap_or_default(),
                    "name": format!("{}/{}", s.service_name, s.tool_name),
                    "startTimeUnixNano": (s.start_time_us.saturating_mul(1_000)).to_string(),
                    "endTimeUnixNano": (s.start_time_us.saturating_add(s.duration_us).saturating_mul(1_000)).to_string(),
                    "attributes": [
                        {"key": "omni.server", "value": {"stringValue": s.service_name.clone()}},
                        {"key": "omni.tool", "value": {"stringValue": s.tool_name.clone()}},
                        {"key": "omni.status", "value": {"stringValue": s.status.clone()}}
                    ]
                })
            })
            .collect();

        let mut metric_items: Vec<JsonValue> = metrics_counters
            .iter()
            .map(|(k, v)| {
                json!({
                    "name": *k,
                    "sum": {
                        "dataPoints": [{"asInt": (*v as i64).to_string()}],
                        "isMonotonic": true
                    }
                })
            })
            .collect();

        if let Some(hist) = histogram {
            let (p50, p95, p99) = hist.percentiles();
            metric_items.push(json!({
                "name": "omni_latency_us_quantiles",
                "summary": {
                    "dataPoints": [{
                        "quantileValues": [
                            {"quantile": 0.50, "value": p50 as f64},
                            {"quantile": 0.95, "value": p95 as f64},
                            {"quantile": 0.99, "value": p99 as f64}
                        ]
                    }]
                }
            }));
            for (err_code, count) in hist.error_counts() {
                metric_items.push(json!({
                    "name": format!("omni_errors_total.{}", err_code),
                    "sum": {
                        "dataPoints": [{"asInt": (*count as i64).to_string()}],
                        "isMonotonic": true
                    }
                }));
            }
        }

        json!({
            "resourceSpans": [{
                "resource": {
                    "attributes": [
                        {"key": "service.name", "value": {"stringValue": self.service_name.clone()}},
                        {"key": "service.version", "value": {"stringValue": env!("CARGO_PKG_VERSION")}}
                    ]
                },
                "scopeSpans": [{
                    "scope": {"name": "omni-mcp"},
                    "spans": JsonValue::Array(span_items)
                }]
            }],
            "resourceMetrics": [{
                "resource": {
                    "attributes": [
                        {"key": "service.name", "value": {"stringValue": self.service_name.clone()}}
                    ]
                },
                "scopeMetrics": [{
                    "scope": {"name": "omni-mcp"},
                    "metrics": JsonValue::Array(metric_items)
                }]
            }]
        })
    }

    pub fn export_batch(
        &self,
        spans: &[TraceSpan],
        metrics_counters: &[(&str, u64)],
    ) -> OtlpExportReport {
        self.export_batch_with_histogram(spans, metrics_counters, None)
    }

    pub fn export_batch_with_histogram(
        &self,
        spans: &[TraceSpan],
        metrics_counters: &[(&str, u64)],
        histogram: Option<&LatencyHistogram>,
    ) -> OtlpExportReport {
        let payload = self.build_otlp_payload_with_histograms(spans, metrics_counters, histogram);
        let bytes = payload.to_string_compact().into_bytes();
        let mut hdrs = HashMap::new();
        hdrs.insert("Content-Type".to_string(), "application/json".to_string());
        match self.client.post_json(&self.endpoint, &bytes, Some(&hdrs)) {
            Ok(_) => OtlpExportReport {
                endpoint: self.endpoint.clone(),
                spans_exported: spans.len(),
                ok: true,
                error: None,
            },
            Err(e) => OtlpExportReport {
                endpoint: self.endpoint.clone(),
                spans_exported: 0,
                ok: false,
                error: Some(e),
            },
        }
    }
}

struct OtlpQueueState {
    queue: VecDeque<TraceSpan>,
    histogram: LatencyHistogram,
    total_exported: u64,
    total_failed: u64,
    last_flush_at: Option<f64>,
    last_error: Option<String>,
    flush_requested: bool,
}

#[derive(Clone, Debug)]
pub struct OtlpWorkerStatus {
    pub endpoint: String,
    pub running: bool,
    pub flush_interval_ms: u64,
    pub batch_threshold: usize,
    pub queued_spans: usize,
    pub total_exported: u64,
    pub total_failed: u64,
    pub last_flush_at: Option<f64>,
    pub last_error: Option<String>,
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub error_counts: BTreeMap<String, u64>,
}

impl OtlpWorkerStatus {
    pub fn to_json(&self) -> JsonValue {
        let mut errs = JsonValue::object();
        for (k, v) in &self.error_counts {
            errs.insert(k.clone(), JsonValue::from(*v as i64));
        }
        json!({
            "endpoint": self.endpoint.clone(),
            "running": self.running,
            "flush_interval_ms": self.flush_interval_ms as i64,
            "batch_threshold": self.batch_threshold as i64,
            "queued_spans": self.queued_spans as i64,
            "total_exported": self.total_exported as i64,
            "total_failed": self.total_failed as i64,
            "last_flush_at": self.last_flush_at.map(JsonValue::from).unwrap_or(JsonValue::Null),
            "last_error": self.last_error.as_ref().map(|s| JsonValue::from(s.as_str())).unwrap_or(JsonValue::Null),
            "latency_quantiles_us": {
                "p50": self.p50_us as i64,
                "p95": self.p95_us as i64,
                "p99": self.p99_us as i64,
            },
            "error_counts": errs,
        })
    }
}

/// Live background OTLP batch-flush worker (v6.5.0).
pub struct OtlpBatchWorker {
    exporter: OtlpExporter,
    flush_interval_ms: u64,
    batch_threshold: usize,
    shared: Arc<(Mutex<OtlpQueueState>, Condvar)>,
    running: Arc<AtomicBool>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl OtlpBatchWorker {
    pub fn new(
        endpoint: impl Into<String>,
        flush_interval_ms: u64,
        batch_threshold: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            exporter: OtlpExporter::new(endpoint),
            flush_interval_ms: flush_interval_ms.max(50),
            batch_threshold: batch_threshold.max(1),
            shared: Arc::new((
                Mutex::new(OtlpQueueState {
                    queue: VecDeque::new(),
                    histogram: LatencyHistogram::default(),
                    total_exported: 0,
                    total_failed: 0,
                    last_flush_at: None,
                    last_error: None,
                    flush_requested: false,
                }),
                Condvar::new(),
            )),
            running: Arc::new(AtomicBool::new(false)),
            handle: Mutex::new(None),
        })
    }

    pub fn from_env() -> Option<Arc<Self>> {
        let ep = std::env::var("OMNI_OTLP_ENDPOINT").ok()?;
        let trimmed = ep.trim();
        if trimmed.is_empty() {
            return None;
        }
        let interval = std::env::var("OMNI_OTLP_FLUSH_INTERVAL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(5000);
        Some(Self::new(trimmed, interval, DEFAULT_OTLP_BATCH_THRESHOLD))
    }

    /// Starts the background flush loop thread. Safe to call multiple times (idempotent).
    pub fn start(self: &Arc<Self>) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let exporter = self.exporter.clone();
        let shared = self.shared.clone();
        let running = self.running.clone();
        let interval = Duration::from_millis(self.flush_interval_ms);
        let threshold = self.batch_threshold;

        let handle = thread::spawn(move || {
            let mut consecutive_failures: u32 = 0;
            while running.load(Ordering::SeqCst) {
                let (batch, hist_snapshot) = {
                    let (lock, cvar) = &*shared;
                    let mut st = lock.lock().unwrap();
                    if st.queue.len() < threshold && !st.flush_requested {
                        let (guard, _) = cvar.wait_timeout(st, interval).unwrap();
                        st = guard;
                    }
                    st.flush_requested = false;
                    let drained: Vec<TraceSpan> = st.queue.drain(..).collect();
                    let hist = st.histogram.clone();
                    (drained, hist)
                };

                if batch.is_empty() {
                    continue;
                }

                let counters = [("omni_otlp_batch_spans", batch.len() as u64)];
                let report =
                    exporter.export_batch_with_histogram(&batch, &counters, Some(&hist_snapshot));
                let (lock, _) = &*shared;
                let mut st = lock.lock().unwrap();
                st.last_flush_at = Some(now_epoch_secs());
                if report.ok {
                    st.total_exported += report.spans_exported as u64;
                    st.last_error = None;
                    consecutive_failures = 0;
                } else {
                    st.total_failed += batch.len() as u64;
                    st.last_error = report.error;
                    consecutive_failures = consecutive_failures.saturating_add(1).min(5);
                    // Re-queue failed batch for retry on transient collector outage (bounded by MAX_OTLP_QUEUE_SPANS)
                    if consecutive_failures <= 3 {
                        for span in batch.into_iter().rev() {
                            if st.queue.len() < MAX_OTLP_QUEUE_SPANS {
                                st.queue.push_front(span);
                            }
                        }
                    }
                    drop(st);
                    let backoff_ms = 100u64 * (1u64 << consecutive_failures);
                    thread::sleep(Duration::from_millis(backoff_ms.min(2000)));
                }
            }
        });

        *self.handle.lock().unwrap() = Some(handle);
    }

    /// Enqueues a completed `TraceSpan`, updates latency histograms, and wakes the worker
    /// immediately if `queue.len() >= batch_threshold`.
    pub fn record_span(&self, span: TraceSpan) {
        let (lock, cvar) = &*self.shared;
        let mut st = lock.lock().unwrap();
        st.histogram.record(span.duration_us, &span.status);
        if st.queue.len() >= MAX_OTLP_QUEUE_SPANS {
            st.queue.pop_front();
        }
        st.queue.push_back(span);
        if st.queue.len() >= self.batch_threshold {
            cvar.notify_one();
        }
    }

    /// Triggers an immediate asynchronous flush of all currently queued spans.
    pub fn flush_now(&self) {
        let (lock, cvar) = &*self.shared;
        let mut st = lock.lock().unwrap();
        st.flush_requested = true;
        cvar.notify_one();
    }

    pub fn status(&self) -> OtlpWorkerStatus {
        let (lock, _) = &*self.shared;
        let st = lock.lock().unwrap();
        let (p50_us, p95_us, p99_us) = st.histogram.percentiles();
        OtlpWorkerStatus {
            endpoint: self.exporter.endpoint.clone(),
            running: self.running.load(Ordering::SeqCst),
            flush_interval_ms: self.flush_interval_ms,
            batch_threshold: self.batch_threshold,
            queued_spans: st.queue.len(),
            total_exported: st.total_exported,
            total_failed: st.total_failed,
            last_flush_at: st.last_flush_at,
            last_error: st.last_error.clone(),
            p50_us,
            p95_us,
            p99_us,
            error_counts: st.histogram.error_counts().clone(),
        }
    }

    pub fn shutdown(&self) {
        self.running.store(false, Ordering::SeqCst);
        let (_, cvar) = &*self.shared;
        cvar.notify_all();
        if let Some(h) = self.handle.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}

impl Drop for OtlpBatchWorker {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        let (_, cvar) = &*self.shared;
        cvar.notify_all();
    }
}
