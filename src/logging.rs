//! Structured Production Logging and Telemetry for Omni MCP.
//! Emits structured JSON log events with nanosecond/microsecond precision for observability.

use crate::json::JsonValue;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_epoch_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn now_epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub struct LogEvent<'a> {
    pub level: &'a str,
    pub message: &'a str,
    pub trace_id: Option<&'a str>,
    pub actor: Option<&'a str>,
    pub service: Option<&'a str>,
    pub duration_us: Option<u64>,
    pub status_code: Option<u16>,
}

impl<'a> LogEvent<'a> {
    pub fn to_json(&self) -> JsonValue {
        let mut obj = json!({
            "timestamp": now_epoch_secs(),
            "level": self.level,
            "logger": "omni_mcp",
            "runtime": "rust",
            "message": self.message,
        });
        if let Some(tid) = self.trace_id {
            obj.insert("trace_id", tid);
        }
        if let Some(actor) = self.actor {
            obj.insert("actor", actor);
        }
        if let Some(svc) = self.service {
            obj.insert("service", svc);
        }
        if let Some(dur) = self.duration_us {
            obj.insert("duration_us", dur);
            obj.insert("duration_ms", (dur as f64) / 1000.0);
        }
        if let Some(code) = self.status_code {
            obj.insert("status_code", code as i64);
        }
        obj
    }

    pub fn emit(&self) {
        println!("{}", self.to_json().to_string_compact());
    }
}

pub fn log_info(message: &str) {
    let evt = LogEvent {
        level: "INFO",
        message,
        trace_id: None,
        actor: None,
        service: None,
        duration_us: None,
        status_code: None,
    };
    evt.emit();
}
