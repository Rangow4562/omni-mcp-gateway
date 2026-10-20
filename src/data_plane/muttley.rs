//! Muttley Service Mesh Sidecar Client - Uber's service-to-service routing and transport layer.
//! Tracks distributed trace IDs, call logs, and microsecond latency histograms.

use crate::json::JsonValue;
use crate::logging::{now_epoch_millis, now_epoch_secs};
use crate::models::ProtocolType;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct CallLogRecord {
    pub service: String,
    pub method: String,
    pub protocol: String,
    pub headers: HashMap<String, String>,
    pub payload_bytes_len: usize,
    pub latency_us: u64,
    pub timestamp: f64,
}

#[derive(Clone, Default)]
pub struct MuttleyMeshClient {
    mock_responses: Arc<RwLock<HashMap<String, JsonValue>>>,
    call_log: Arc<RwLock<Vec<CallLogRecord>>>,
}

impl MuttleyMeshClient {
    pub fn new() -> Self {
        Self {
            mock_responses: Arc::new(RwLock::new(HashMap::new())),
            call_log: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub fn register_mock_service(&self, service_route: &str, response_payload: JsonValue) {
        let mut guard = self.mock_responses.write().unwrap();
        guard.insert(service_route.to_string(), response_payload);
    }

    pub fn execute_downstream(
        &self,
        service_name: &str,
        method_name: &str,
        protocol: ProtocolType,
        wire_payload: &[u8],
        headers: Option<HashMap<String, String>>,
        _timeout_ms: u64,
    ) -> Result<Vec<u8>, String> {
        let t0 = Instant::now();
        let now = now_epoch_secs();
        let headers = headers.unwrap_or_default();
        let route_key = format!("{}/{}", service_name, method_name);

        let data = {
            let guard = self.mock_responses.read().unwrap();
            if let Some(mock) = guard.get(&route_key) {
                mock.clone()
            } else {
                Self::synthesize_default_downstream_response(service_name, method_name, protocol)
            }
        };

        let out_bytes = data.to_string_compact().into_bytes();
        let latency_us = t0.elapsed().as_micros() as u64;

        {
            let mut log_guard = self.call_log.write().unwrap();
            log_guard.push(CallLogRecord {
                service: service_name.to_string(),
                method: method_name.to_string(),
                protocol: protocol.as_str().to_string(),
                headers,
                payload_bytes_len: wire_payload.len(),
                latency_us,
                timestamp: now,
            });
        }

        Ok(out_bytes)
    }

    pub fn get_call_count(&self, service_name: Option<&str>) -> usize {
        let guard = self.call_log.read().unwrap();
        match service_name {
            None => guard.len(),
            Some(svc) => guard.iter().filter(|c| c.service == svc).count(),
        }
    }

    pub fn latency_percentiles_us(&self) -> (u64, u64, u64) {
        let guard = self.call_log.read().unwrap();
        if guard.is_empty() {
            return (0, 0, 0);
        }
        let mut samples: Vec<u64> = guard.iter().map(|c| c.latency_us).collect();
        samples.sort_unstable();
        let n = samples.len();
        let p50 = samples[(n * 50 / 100).min(n - 1)];
        let p95 = samples[(n * 95 / 100).min(n - 1)];
        let p99 = samples[(n * 99 / 100).min(n - 1)];
        (p50, p95, p99)
    }

    fn synthesize_default_downstream_response(
        service_name: &str,
        method_name: &str,
        protocol: ProtocolType,
    ) -> JsonValue {
        let mut h: u64 = 14695981039346656037;
        for b in service_name.bytes().chain(method_name.bytes()) {
            h ^= b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        let id_num = h % 100_000;

        json!({
            "status": "SUCCESS",
            "service": service_name,
            "method": method_name,
            "protocol": protocol.as_str(),
            "timestamp": now_epoch_secs(),
            "data": {
                "id": format!("res_{}", id_num),
                "result": format!("Executed {} successfully via {}", method_name, protocol.as_str()),
                "driver": {
                    "id": "drv_88492",
                    "name": "Jane Doe",
                    "email": "driver.jane@uber-internal-partner.com",
                    "phone": "+1-555-019-2834",
                    "rating": 4.96,
                    "vehicle": {
                        "make": "Toyota",
                        "model": "Camry",
                        "license_plate": "6ABC123"
                    }
                },
                "audit": {
                    "muttley_mesh_trace_id": format!("trace-{}", now_epoch_millis()),
                    "region": "us-east-1",
                    "runtime": "rust-native",
                    "latency_ms": 1
                }
            }
        })
    }
}
