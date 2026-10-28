//! JSON-RPC 2.0 and Model Context Protocol (MCP) data models, batch support, and error primitives.

use crate::json::JsonValue;

pub const JSONRPC_VERSION: &str = "2.0";

// Standard JSON-RPC 2.0 Error Codes
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;

// MCP & Enterprise Gateway Specific Error Codes (-320xx Authoritative Contract)
pub const UNAUTHORIZED: i32 = -32001;
pub const TOOL_DISABLED: i32 = -32002;
pub const DOWNSTREAM_ERROR: i32 = -32003;
pub const RATE_LIMITED: i32 = -32029;
pub const CIRCUIT_OPEN: i32 = -32030;
pub const APPROVAL_REQUIRED: i32 = -32031;
pub const FUEL_EXHAUSTED: i32 = -32032;
pub const OUTBOUND_BLOCKED: i32 = -32033;
pub const SAGA_COMPENSATED: i32 = -32034;
pub const SCHEMA_PREFLIGHT_FAILED: i32 = -32035;

// Supported MCP Protocol Versions for Lifecycle Handshake Negotiation
pub const LEGACY_MCP_PROTOCOL_VERSION: &str = "2024-11-05";
pub const LATEST_MCP_PROTOCOL_VERSION: &str = "2025-03-26";
pub const SUPPORTED_MCP_PROTOCOL_VERSIONS: &[&str] =
    &[LEGACY_MCP_PROTOCOL_VERSION, LATEST_MCP_PROTOCOL_VERSION];

/// Negotiates the MCP protocol version during `initialize`.
/// Echoes the client's requested `protocolVersion` if it is in `SUPPORTED_MCP_PROTOCOL_VERSIONS`;
/// otherwise responds with `LATEST_MCP_PROTOCOL_VERSION` (`"2025-03-26"`).
pub fn negotiate_protocol_version(client_version: Option<&str>) -> &'static str {
    if let Some(req_ver) = client_version {
        for &supported in SUPPORTED_MCP_PROTOCOL_VERSIONS {
            if req_ver.trim() == supported {
                return supported;
            }
        }
    }
    LATEST_MCP_PROTOCOL_VERSION
}

/// Compiler-checked runtime feature status enum with wire-compatible string representations
/// and authoritative `-320xx` JSON-RPC 2.0 error code mappings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeatureStatus {
    Ok,
    Error,
    ApprovalRequired,
    FuelExhausted,
    OutboundBlocked,
    SagaRolledBack,
    CircuitOpen,
    SchemaPreflightFailed,
}

impl FeatureStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            FeatureStatus::Ok => "OK",
            FeatureStatus::Error => "ERROR",
            FeatureStatus::ApprovalRequired => "APPROVAL_REQUIRED",
            FeatureStatus::FuelExhausted => "FUEL_EXHAUSTED",
            FeatureStatus::OutboundBlocked => "OUTBOUND_BLOCKED",
            FeatureStatus::SagaRolledBack => "SAGA_ROLLED_BACK",
            FeatureStatus::CircuitOpen => "CIRCUIT_OPEN",
            FeatureStatus::SchemaPreflightFailed => "SCHEMA_PREFLIGHT_FAILED",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "OK" => FeatureStatus::Ok,
            "APPROVAL_REQUIRED" => FeatureStatus::ApprovalRequired,
            "FUEL_EXHAUSTED" | "FuelExhausted" => FeatureStatus::FuelExhausted,
            "OUTBOUND_BLOCKED" | "GlobalOutboundBlocked" => FeatureStatus::OutboundBlocked,
            "SAGA_ROLLED_BACK" | "SAGA_COMPENSATED" => FeatureStatus::SagaRolledBack,
            "CIRCUIT_OPEN" => FeatureStatus::CircuitOpen,
            "SCHEMA_PREFLIGHT_FAILED" => FeatureStatus::SchemaPreflightFailed,
            _ => FeatureStatus::Error,
        }
    }

    pub fn error_code(&self) -> Option<i32> {
        match self {
            FeatureStatus::Ok => None,
            FeatureStatus::Error => Some(INTERNAL_ERROR),
            FeatureStatus::ApprovalRequired => Some(APPROVAL_REQUIRED),
            FeatureStatus::FuelExhausted => Some(FUEL_EXHAUSTED),
            FeatureStatus::OutboundBlocked => Some(OUTBOUND_BLOCKED),
            FeatureStatus::SagaRolledBack => Some(SAGA_COMPENSATED),
            FeatureStatus::CircuitOpen => Some(CIRCUIT_OPEN),
            FeatureStatus::SchemaPreflightFailed => Some(SCHEMA_PREFLIGHT_FAILED),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RPCError {
    pub code: i32,
    pub message: String,
    pub data: Option<JsonValue>,
}

impl RPCError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(code: i32, message: impl Into<String>, data: JsonValue) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(data),
        }
    }

    pub fn from_status(status: FeatureStatus, detail: impl Into<String>) -> Self {
        let detail_str = detail.into();
        let code = status.error_code().unwrap_or(INTERNAL_ERROR);
        Self {
            code,
            message: format!("{}: {}", status.as_str(), detail_str),
            data: Some(json!({
                "status": status.as_str(),
                "detail": detail_str,
            })),
        }
    }

    pub fn to_json(&self) -> JsonValue {
        let mut obj = json!({
            "code": self.code,
            "message": self.message.clone(),
        });
        if let Some(d) = &self.data {
            obj.insert("data", d.clone());
        }
        obj
    }
}

#[derive(Clone, Debug)]
pub struct RPCRequest {
    pub jsonrpc: String,
    pub id: JsonValue,
    pub method: String,
    pub params: JsonValue,
}

impl RPCRequest {
    pub fn new(id: impl Into<JsonValue>, method: impl Into<String>, params: JsonValue) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: id.into(),
            method: method.into(),
            params,
        }
    }

    pub fn from_json(data: &JsonValue) -> Result<Self, String> {
        if !data.is_object() {
            return Err("Request must be a JSON object".to_string());
        }
        let method = data
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let params = data
            .get("params")
            .cloned()
            .unwrap_or_else(JsonValue::object);
        let id = data.get("id").cloned().unwrap_or(JsonValue::Null);
        let jsonrpc = data
            .get("jsonrpc")
            .and_then(|v| v.as_str())
            .unwrap_or(JSONRPC_VERSION)
            .to_string();

        Ok(Self {
            jsonrpc,
            id,
            method,
            params,
        })
    }
}

#[derive(Clone, Debug)]
pub enum RPCBatchOrSingle {
    Single(RPCRequest),
    Batch(Vec<RPCRequest>),
}

impl RPCBatchOrSingle {
    pub fn from_json(data: &JsonValue) -> Result<Self, String> {
        if let Some(arr) = data.as_array() {
            if arr.is_empty() {
                return Err("Batch JSON-RPC request array cannot be empty".to_string());
            }
            let mut reqs = Vec::with_capacity(arr.len());
            for item in arr {
                reqs.push(RPCRequest::from_json(item)?);
            }
            Ok(RPCBatchOrSingle::Batch(reqs))
        } else {
            Ok(RPCBatchOrSingle::Single(RPCRequest::from_json(data)?))
        }
    }
}

#[derive(Clone, Debug)]
pub struct RPCResponse {
    pub jsonrpc: String,
    pub id: JsonValue,
    pub result: Option<JsonValue>,
    pub error: Option<RPCError>,
}

impl RPCResponse {
    pub fn ok(id: JsonValue, result: JsonValue) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: JsonValue, error: RPCError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }

    pub fn to_json(&self) -> JsonValue {
        let mut out = json!({
            "jsonrpc": self.jsonrpc.clone(),
            "id": self.id.clone(),
        });
        if let Some(err) = &self.error {
            out.insert("error", err.to_json());
        } else {
            out.insert(
                "result",
                self.result.clone().unwrap_or_else(JsonValue::object),
            );
        }
        out
    }
}
