//! OpenTelemetry OTLP JSON-over-HTTP batch exporter (`OMNI_OTLP_ENDPOINT`).

use crate::data_plane::HttpDownstreamClient;
use crate::json::JsonValue;
use crate::models::TraceSpan;
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct OtlpExportReport {
    pub endpoint: String,
    pub spans_exported: usize,
    pub ok: bool,
    pub error: Option<String>,
}

pub struct OtlpExporter {
    pub endpoint: String,
    pub service_name: String,
    client: HttpDownstreamClient,
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

        let metric_items: Vec<JsonValue> = metrics_counters
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
        let payload = self.build_otlp_payload(spans, metrics_counters);
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
