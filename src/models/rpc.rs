//! JSON-RPC 2.0 and Model Context Protocol (MCP) data models, batch support, and error primitives.

use crate::json::JsonValue;

pub const JSONRPC_VERSION: &str = "2.0";

// Standard JSON-RPC 2.0 Error Codes
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;

// MCP Specification Reserved Error Codes (-32020..-32099)
pub const HEADER_MISMATCH: i32 = -32020;
pub const MISSING_REQUIRED_CLIENT_CAPABILITY: i32 = -32021;
pub const UNSUPPORTED_PROTOCOL_VERSION: i32 = -32022;

// MCP & Enterprise Gateway Specific Error Codes (-32000..-32019 Implementation-Defined Range)
pub const UNAUTHORIZED: i32 = -32001;
pub const TOOL_DISABLED: i32 = -32002;
pub const TOOL_DISABLED_LEGACY: i32 = -32002;
pub const DOWNSTREAM_ERROR: i32 = -32003;
pub const RATE_LIMITED: i32 = -32004;
pub const CIRCUIT_OPEN: i32 = -32005;
pub const APPROVAL_REQUIRED: i32 = -32006;
pub const FUEL_EXHAUSTED: i32 = -32007;
pub const OUTBOUND_BLOCKED: i32 = -32008;
pub const SAGA_COMPENSATED: i32 = -32009;
pub const SCHEMA_PREFLIGHT_FAILED: i32 = -32010;
pub const MEMORY_EXHAUSTED: i32 = -32011;
pub const TIMEOUT: i32 = -32012;
pub const APPROVAL_INVALID: i32 = -32013;

// Legacy 4.x compatibility aliases (-32029..-32038)
pub const RATE_LIMITED_LEGACY: i32 = -32029;
pub const CIRCUIT_OPEN_LEGACY: i32 = -32030;
pub const APPROVAL_REQUIRED_LEGACY: i32 = -32031;
pub const FUEL_EXHAUSTED_LEGACY: i32 = -32032;
pub const OUTBOUND_BLOCKED_LEGACY: i32 = -32033;
pub const SAGA_COMPENSATED_LEGACY: i32 = -32034;
pub const SCHEMA_PREFLIGHT_FAILED_LEGACY: i32 = -32035;
pub const MEMORY_EXHAUSTED_LEGACY: i32 = -32036;
pub const TIMEOUT_LEGACY: i32 = -32037;
pub const APPROVAL_INVALID_LEGACY: i32 = -32038;

pub fn to_legacy_error_code(code: i32) -> i32 {
    match code {
        RATE_LIMITED => RATE_LIMITED_LEGACY,
        CIRCUIT_OPEN => CIRCUIT_OPEN_LEGACY,
        APPROVAL_REQUIRED => APPROVAL_REQUIRED_LEGACY,
        FUEL_EXHAUSTED => FUEL_EXHAUSTED_LEGACY,
        OUTBOUND_BLOCKED => OUTBOUND_BLOCKED_LEGACY,
        SAGA_COMPENSATED => SAGA_COMPENSATED_LEGACY,
        SCHEMA_PREFLIGHT_FAILED => SCHEMA_PREFLIGHT_FAILED_LEGACY,
        MEMORY_EXHAUSTED => MEMORY_EXHAUSTED_LEGACY,
        TIMEOUT => TIMEOUT_LEGACY,
        APPROVAL_INVALID => APPROVAL_INVALID_LEGACY,
        other => other,
    }
}

pub fn from_legacy_error_code(code: i32) -> i32 {
    match code {
        RATE_LIMITED_LEGACY => RATE_LIMITED,
        CIRCUIT_OPEN_LEGACY => CIRCUIT_OPEN,
        APPROVAL_REQUIRED_LEGACY => APPROVAL_REQUIRED,
        FUEL_EXHAUSTED_LEGACY => FUEL_EXHAUSTED,
        OUTBOUND_BLOCKED_LEGACY => OUTBOUND_BLOCKED,
        SAGA_COMPENSATED_LEGACY => SAGA_COMPENSATED,
        SCHEMA_PREFLIGHT_FAILED_LEGACY => SCHEMA_PREFLIGHT_FAILED,
        MEMORY_EXHAUSTED_LEGACY => MEMORY_EXHAUSTED,
        TIMEOUT_LEGACY => TIMEOUT,
        APPROVAL_INVALID_LEGACY => APPROVAL_INVALID,
        other => other,
    }
}

// Supported MCP Protocol Versions for Lifecycle Handshake Negotiation
pub const LEGACY_MCP_PROTOCOL_VERSION: &str = "2024-11-05";
pub const LATEST_MCP_PROTOCOL_VERSION: &str = "2026-07-28";
pub const LATEST_SESSION_MCP_PROTOCOL_VERSION: &str = "2025-11-25";

/// Per-revision MCP behavior. Protocol behavior is driven by this table rather than by
/// scattered version string comparisons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProtocolCapabilities {
    /// Revision identifier as sent on the wire.
    pub version: &'static str,
    /// Whether this gateway currently implements the revision. Rows with `implemented: false`
    /// are roadmap placeholders and are never offered during negotiation.
    pub implemented: bool,
    /// `true` for `initialize`-based session revisions, `false` for the stateless revision.
    pub session_based: bool,
    /// JSON-RPC array batching is permitted (removed by later revisions).
    pub allows_batching: bool,
    /// Tool `title`, `outputSchema`/`structuredContent`, and resource links in tool results.
    pub structured_tool_output: bool,
    /// Resource-not-found is reported as `-32602` rather than `-32002`.
    pub resource_not_found_is_invalid_params: bool,
}

/// Capability table for every MCP revision this gateway knows about, oldest first.
pub const PROTOCOL_CAPABILITY_TABLE: &[ProtocolCapabilities] = &[
    ProtocolCapabilities {
        version: "2024-11-05",
        implemented: true,
        session_based: true,
        allows_batching: true,
        structured_tool_output: false,
        resource_not_found_is_invalid_params: false,
    },
    ProtocolCapabilities {
        version: "2025-03-26",
        implemented: true,
        session_based: true,
        allows_batching: true,
        structured_tool_output: false,
        resource_not_found_is_invalid_params: false,
    },
    ProtocolCapabilities {
        version: "2025-06-18",
        implemented: true,
        session_based: true,
        allows_batching: false,
        structured_tool_output: true,
        resource_not_found_is_invalid_params: false,
    },
    ProtocolCapabilities {
        version: "2025-11-25",
        implemented: true,
        session_based: true,
        allows_batching: false,
        structured_tool_output: true,
        resource_not_found_is_invalid_params: false,
    },
    ProtocolCapabilities {
        version: "2026-07-28",
        implemented: true,
        session_based: false,
        allows_batching: false,
        structured_tool_output: true,
        resource_not_found_is_invalid_params: true,
    },
];

/// Revisions the gateway can actually negotiate today.
pub const SUPPORTED_MCP_PROTOCOL_VERSIONS: &[&str] = &[
    "2024-11-05",
    "2025-03-26",
    "2025-06-18",
    "2025-11-25",
    "2026-07-28",
];

/// Looks up the capability row for a revision identifier (implemented or not).
pub fn protocol_capabilities(version: &str) -> Option<&'static ProtocolCapabilities> {
    let version = version.trim();
    PROTOCOL_CAPABILITY_TABLE
        .iter()
        .find(|caps| caps.version == version)
}

/// Negotiates the MCP protocol version during `initialize`.
/// Echoes the client's requested `protocolVersion` if the gateway implements it;
/// otherwise responds with `LATEST_SESSION_MCP_PROTOCOL_VERSION` (`"2025-11-25"`),
/// because `initialize` handshakes are session-based.
pub fn negotiate_protocol_version(client_version: Option<&str>) -> &'static str {
    if let Some(req_ver) = client_version {
        if let Some(caps) = protocol_capabilities(req_ver) {
            if caps.implemented {
                return caps.version;
            }
        }
    }
    LATEST_SESSION_MCP_PROTOCOL_VERSION
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
    MemoryExhausted,
    Timeout,
    ApprovalInvalid,
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
            FeatureStatus::MemoryExhausted => "MEMORY_EXHAUSTED",
            FeatureStatus::Timeout => "TIMEOUT",
            FeatureStatus::ApprovalInvalid => "APPROVAL_INVALID",
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
            "MEMORY_EXHAUSTED" | "MemoryExhausted" => FeatureStatus::MemoryExhausted,
            "TIMEOUT" | "Timeout" => FeatureStatus::Timeout,
            "APPROVAL_INVALID" | "ApprovalInvalid" => FeatureStatus::ApprovalInvalid,
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
            FeatureStatus::MemoryExhausted => Some(MEMORY_EXHAUSTED),
            FeatureStatus::Timeout => Some(TIMEOUT),
            FeatureStatus::ApprovalInvalid => Some(APPROVAL_INVALID),
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
        let legacy = to_legacy_error_code(code);
        Self {
            code,
            message: format!("{}: {}", status.as_str(), detail_str),
            data: Some(json!({
                "status": status.as_str(),
                "detail": detail_str,
                "legacy_code": legacy,
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

#[cfg(test)]
mod protocol_table_tests {
    use super::*;

    #[test]
    fn capability_table_lists_all_five_revisions_in_order() {
        let versions: Vec<&str> = PROTOCOL_CAPABILITY_TABLE
            .iter()
            .map(|c| c.version)
            .collect();
        assert_eq!(
            versions,
            [
                "2024-11-05",
                "2025-03-26",
                "2025-06-18",
                "2025-11-25",
                "2026-07-28"
            ]
        );
    }

    #[test]
    fn supported_versions_match_implemented_rows() {
        let implemented: Vec<&str> = PROTOCOL_CAPABILITY_TABLE
            .iter()
            .filter(|c| c.implemented)
            .map(|c| c.version)
            .collect();
        assert_eq!(implemented, SUPPORTED_MCP_PROTOCOL_VERSIONS);
    }

    #[test]
    fn negotiation_never_offers_unimplemented_revisions() {
        for caps in PROTOCOL_CAPABILITY_TABLE.iter().filter(|c| !c.implemented) {
            assert_eq!(
                negotiate_protocol_version(Some(caps.version)),
                LATEST_SESSION_MCP_PROTOCOL_VERSION,
                "{} is not implemented and must fall back to the latest implemented session revision",
                caps.version
            );
        }
        assert_eq!(
            negotiate_protocol_version(Some(" 2024-11-05 ")),
            "2024-11-05"
        );
        assert_eq!(
            negotiate_protocol_version(None),
            LATEST_SESSION_MCP_PROTOCOL_VERSION
        );
    }

    #[test]
    fn only_the_stateless_revision_is_sessionless() {
        let stateless: Vec<&str> = PROTOCOL_CAPABILITY_TABLE
            .iter()
            .filter(|c| !c.session_based)
            .map(|c| c.version)
            .collect();
        assert_eq!(stateless, ["2026-07-28"]);
        assert!(protocol_capabilities("2099-01-01").is_none());
    }

    #[test]
    fn gateway_error_block_is_contiguous_and_unique() {
        let codes = [
            RATE_LIMITED,
            CIRCUIT_OPEN,
            APPROVAL_REQUIRED,
            FUEL_EXHAUSTED,
            OUTBOUND_BLOCKED,
            SAGA_COMPENSATED,
            SCHEMA_PREFLIGHT_FAILED,
            MEMORY_EXHAUSTED,
            TIMEOUT,
            APPROVAL_INVALID,
        ];
        for (i, code) in codes.iter().enumerate() {
            assert_eq!(
                *code,
                -32004 - i as i32,
                "gateway v5.0 implementation-defined block must stay contiguous in -32000..-32019"
            );
            assert_eq!(
                to_legacy_error_code(*code),
                -32029 - i as i32,
                "legacy error mapping must preserve 4.x -32029..-32038 codes"
            );
            assert_eq!(from_legacy_error_code(-32029 - i as i32), *code);
        }
    }
}
