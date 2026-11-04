pub mod entities;
pub mod rpc;
pub mod schema_validator;

pub use entities::{
    constant_time_eq, continuation_key, format_traceparent, generate_span_id, generate_trace_id,
    generate_ulid, parse_traceparent, ActorType, ChangeOrigin, CharterPolicy,
    CompletedMutationRecord, ConfigDiff, HistoryEntry, MutationCompensationSpec, ProtocolType,
    SagaCompensationRecord, SchemaSource, ServerRecord, SessionMode, SuspendedExecutionFrame,
    ToolRecord, ToolStatus, TraceSpan, UpstreamTransportConfig,
};
pub use rpc::{
    from_legacy_error_code, negotiate_protocol_version, protocol_capabilities,
    to_legacy_error_code, FeatureStatus, ProtocolCapabilities, RPCBatchOrSingle, RPCError,
    RPCRequest, RPCResponse, APPROVAL_INVALID, APPROVAL_INVALID_LEGACY, APPROVAL_REQUIRED,
    APPROVAL_REQUIRED_LEGACY, CIRCUIT_OPEN, CIRCUIT_OPEN_LEGACY, DOWNSTREAM_ERROR, FUEL_EXHAUSTED,
    FUEL_EXHAUSTED_LEGACY, HEADER_MISMATCH, INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST,
    JSONRPC_VERSION, LATEST_MCP_PROTOCOL_VERSION, LATEST_SESSION_MCP_PROTOCOL_VERSION,
    LEGACY_MCP_PROTOCOL_VERSION, MEMORY_EXHAUSTED, MEMORY_EXHAUSTED_LEGACY, METHOD_NOT_FOUND,
    MISSING_REQUIRED_CLIENT_CAPABILITY, OUTBOUND_BLOCKED, OUTBOUND_BLOCKED_LEGACY, PARSE_ERROR,
    PROTOCOL_CAPABILITY_TABLE, RATE_LIMITED, RATE_LIMITED_LEGACY, SAGA_COMPENSATED,
    SAGA_COMPENSATED_LEGACY, SCHEMA_PREFLIGHT_FAILED, SCHEMA_PREFLIGHT_FAILED_LEGACY,
    SUPPORTED_MCP_PROTOCOL_VERSIONS, TIMEOUT, TIMEOUT_LEGACY, TOOL_DISABLED, TOOL_DISABLED_LEGACY,
    UNAUTHORIZED, UNSUPPORTED_PROTOCOL_VERSION, UPSTREAM_TLS_UNSUPPORTED,
};
pub use schema_validator::{validate_mcp_response, validate_tool_schema, SchemaViolation};
