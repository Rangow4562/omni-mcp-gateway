//! Advanced Resilience Layer: Per-Service Circuit Breaker & TTL Semantic Response Cache.
//! Protects downstream microservices from cascading failures and caches idempotent tool invocations.

use crate::json::JsonValue;
use crate::logging::now_epoch_secs;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

impl CircuitState {
    pub fn as_str(&self) -> &'static str {
        match self {
            CircuitState::Closed => "CLOSED",
            CircuitState::Open => "OPEN",
            CircuitState::HalfOpen => "HALF_OPEN",
        }
    }
}

#[derive(Clone, Debug)]
struct ServiceCircuit {
    state: CircuitState,
    consecutive_failures: u32,
    opened_at: f64,
}

#[derive(Clone)]
pub struct CircuitBreakerRegistry {
    pub failure_threshold: u32,
    pub recovery_timeout_sec: f64,
    circuits: Arc<RwLock<HashMap<String, ServiceCircuit>>>,
}

impl Default for CircuitBreakerRegistry {
    fn default() -> Self {
        Self::new(5, 30.0)
    }
}

impl CircuitBreakerRegistry {
    pub fn new(failure_threshold: u32, recovery_timeout_sec: f64) -> Self {
        Self {
            failure_threshold: failure_threshold.max(1),
            recovery_timeout_sec: recovery_timeout_sec.max(1.0),
            circuits: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Checks whether a downstream request to `service_name` is permitted.
    pub fn allow_request(&self, service_name: &str) -> Result<CircuitState, String> {
        let now = now_epoch_secs();
        let mut guard = self.circuits.write().unwrap();
        let entry = guard
            .entry(service_name.to_string())
            .or_insert(ServiceCircuit {
                state: CircuitState::Closed,
                consecutive_failures: 0,
                opened_at: 0.0,
            });

        match entry.state {
            CircuitState::Closed | CircuitState::HalfOpen => Ok(entry.state),
            CircuitState::Open => {
                if now - entry.opened_at >= self.recovery_timeout_sec {
                    entry.state = CircuitState::HalfOpen;
                    Ok(CircuitState::HalfOpen)
                } else {
                    Err(format!(
                        "Circuit breaker OPEN for downstream service '{}' ({} consecutive failures)",
                        service_name, entry.consecutive_failures
                    ))
                }
            }
        }
    }

    pub fn record_success(&self, service_name: &str) {
        let mut guard = self.circuits.write().unwrap();
        if let Some(c) = guard.get_mut(service_name) {
            c.consecutive_failures = 0;
            c.state = CircuitState::Closed;
        }
    }

    pub fn record_failure(&self, service_name: &str) {
        let now = now_epoch_secs();
        let mut guard = self.circuits.write().unwrap();
        let entry = guard
            .entry(service_name.to_string())
            .or_insert(ServiceCircuit {
                state: CircuitState::Closed,
                consecutive_failures: 0,
                opened_at: 0.0,
            });
        entry.consecutive_failures += 1;
        if entry.consecutive_failures >= self.failure_threshold
            || entry.state == CircuitState::HalfOpen
        {
            entry.state = CircuitState::Open;
            entry.opened_at = now;
        }
    }

    pub fn get_state(&self, service_name: &str) -> CircuitState {
        let guard = self.circuits.read().unwrap();
        guard
            .get(service_name)
            .map(|c| c.state)
            .unwrap_or(CircuitState::Closed)
    }

    pub fn open_circuits_count(&self) -> usize {
        let guard = self.circuits.read().unwrap();
        guard
            .values()
            .filter(|c| c.state == CircuitState::Open)
            .count()
    }
}

struct CachedItem {
    payload: JsonValue,
    expires_at: f64,
}

#[derive(Clone)]
pub struct SemanticResponseCache {
    pub ttl_seconds: f64,
    entries: Arc<RwLock<HashMap<String, CachedItem>>>,
    hits: Arc<AtomicU64>,
    misses: Arc<AtomicU64>,
}

impl Default for SemanticResponseCache {
    fn default() -> Self {
        Self::new(15.0)
    }
}

impl SemanticResponseCache {
    pub fn new(ttl_seconds: f64) -> Self {
        Self {
            ttl_seconds,
            entries: Arc::new(RwLock::new(HashMap::new())),
            hits: Arc::new(AtomicU64::new(0)),
            misses: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn make_key(service_name: &str, tool_name: &str, args: &JsonValue) -> String {
        format!(
            "{}::{}::{}",
            service_name,
            tool_name,
            args.to_string_compact()
        )
    }

    pub fn get(&self, key: &str) -> Option<JsonValue> {
        if self.ttl_seconds <= 0.0 {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let now = now_epoch_secs();
        let guard = self.entries.read().unwrap();
        if let Some(item) = guard.get(key) {
            if now < item.expires_at {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Some(item.payload.clone());
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    pub fn put(&self, key: String, payload: JsonValue) {
        if self.ttl_seconds <= 0.0 {
            return;
        }
        let now = now_epoch_secs();
        let mut guard = self.entries.write().unwrap();
        if guard.len() > 4096 {
            guard.retain(|_, v| now < v.expires_at);
        }
        guard.insert(
            key,
            CachedItem {
                payload,
                expires_at: now + self.ttl_seconds,
            },
        );
    }

    pub fn invalidate_service(&self, service_name: &str) {
        let prefix = format!("{}::", service_name);
        let mut guard = self.entries.write().unwrap();
        guard.retain(|k, _| !k.starts_with(&prefix));
    }

    pub fn stats(&self) -> (u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
        )
    }
}
