//! Domain entities representing servers, tools, policies, diffs, and governance states.

use crate::json::JsonValue;
use crate::logging::now_epoch_secs;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

static TRACE_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Generates a valid 32-hex character W3C trace ID.
pub fn generate_trace_id() -> String {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let count = TRACE_COUNTER.fetch_add(1, Ordering::Relaxed) as u128;
    let part1 = (nanos ^ (count.wrapping_mul(0x9e3779b97f4a7c15))) as u64;
    let part2 =
        ((nanos >> 64) ^ (count.wrapping_mul(0xbf58476d1ce4e5b9))) as u64 ^ 0xa5a5a5a5a5a5a5a5;
    format!("{:016x}{:016x}", part1.max(1), part2.max(1))
}

/// Generates a valid 16-hex character W3C span ID.
pub fn generate_span_id() -> String {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let count = TRACE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let val = nanos ^ count.wrapping_mul(0x517cc1b727220a95);
    format!("{:016x}", val.max(1))
}

/// Parses an incoming W3C `traceparent` header (format: `00-{trace_id}-{span_id}-{flags}`).
pub fn parse_traceparent(header: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = header.trim().split('-').collect();
    if parts.len() == 4 && parts[0] == "00" && parts[1].len() == 32 && parts[2].len() == 16 {
        Some((parts[1].to_string(), parts[2].to_string()))
    } else {
        None
    }
}

/// Formats a W3C `traceparent` header for downstream propagation.
pub fn format_traceparent(trace_id: &str, span_id: &str) -> String {
    format!("00-{}-{}-01", trace_id, span_id)
}

/// Gathers OS/system entropy via `getrandom` (BCryptGenRandom / getrandom syscall)
/// when `os-csprng` is enabled, or falls back to system time / PID / `/dev/urandom` mix.
/// Never uses the Deterministic Simulation Testing (DST) Xoshiro256StarStar chaos generator.
pub fn gather_entropy(dest: &mut [u8]) {
    #[cfg(feature = "os-csprng")]
    {
        if getrandom::getrandom(dest).is_ok() {
            return;
        }
    }

    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let thread_id = format!("{:?}", std::thread::current().id());
    let local_var = 42usize;
    let stack_addr = &local_var as *const usize as usize;

    let mut mixed = Vec::new();
    mixed.extend_from_slice(&now.to_be_bytes());
    mixed.extend_from_slice(&pid.to_be_bytes());
    mixed.extend_from_slice(thread_id.as_bytes());
    mixed.extend_from_slice(&stack_addr.to_be_bytes());
    mixed.extend_from_slice(&TRACE_COUNTER.fetch_add(1, Ordering::Relaxed).to_be_bytes());

    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let mut os_bytes = [0u8; 32];
        if f.read_exact(&mut os_bytes).is_ok() {
            mixed.extend_from_slice(&os_bytes);
        }
    }

    let hash = crate::storage::audit_log::sha256(&mixed);
    let len = dest.len().min(32);
    dest[..len].copy_from_slice(&hash[..len]);
}

/// Returns the HMAC key used to sign and verify continuation frames.
///
/// Uses `OMNI_AUTH_SECRET` when configured. Otherwise a random per-process key
/// is generated, so no publicly known constant can be used to forge frames
/// (frames signed in one process are then not resumable after a restart).
pub fn continuation_key() -> &'static str {
    static KEY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    KEY.get_or_init(|| {
        let secret = crate::config::get_config().auth_secret;
        if !secret.is_empty() {
            return secret;
        }
        let mut buf = [0u8; 32];
        gather_entropy(&mut buf);
        buf.iter().map(|b| format!("{:02x}", b)).collect()
    })
}

/// Constant-time equality for secrets/signatures.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= (x ^ y) as usize;
    }
    diff == 0
}

/// Generates a 26-character Crockford Base32 ULID (48-bit ms timestamp + 80-bit monotonic OS entropy).
pub fn generate_ulid() -> String {
    const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    use std::time::SystemTime;
    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let mut entropy_bytes = [0u8; 16];
    gather_entropy(&mut entropy_bytes);

    let entropy_hi = u64::from_be_bytes([
        entropy_bytes[0],
        entropy_bytes[1],
        entropy_bytes[2],
        entropy_bytes[3],
        entropy_bytes[4],
        entropy_bytes[5],
        entropy_bytes[6],
        entropy_bytes[7],
    ]);
    let entropy_lo = u64::from_be_bytes([
        entropy_bytes[8],
        entropy_bytes[9],
        entropy_bytes[10],
        entropy_bytes[11],
        entropy_bytes[12],
        entropy_bytes[13],
        entropy_bytes[14],
        entropy_bytes[15],
    ]);

    let mut out = [b'0'; 26];
    // 10 chars of 48-bit timestamp
    let mut ts = now_ms & 0xFFFF_FFFF_FFFF;
    for i in (0..10).rev() {
        out[i] = CROCKFORD[(ts & 0x1F) as usize];
        ts >>= 5;
    }
    // 16 chars of 80-bit randomness
    let mut r1 = entropy_hi;
    let mut r2 = entropy_lo;
    for slot in out.iter_mut().take(18).skip(10) {
        *slot = CROCKFORD[(r1 & 0x1F) as usize];
        r1 >>= 5;
    }
    for slot in out.iter_mut().skip(18) {
        *slot = CROCKFORD[(r2 & 0x1F) as usize];
        r2 >>= 5;
    }
    String::from_utf8(out.to_vec()).unwrap_or_else(|_| "01J00000000000000000000000".to_string())
}

/// OpenTelemetry-compatible distributed trace span.
#[derive(Clone, Debug, PartialEq)]
pub struct TraceSpan {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub name: String,
    pub service_name: String,
    pub tool_name: String,
    pub start_time_us: u64,
    pub duration_us: u64,
    pub status: String,
    pub attributes: BTreeMap<String, JsonValue>,
}

impl TraceSpan {
    pub fn to_json(&self) -> JsonValue {
        let attrs: Vec<JsonValue> = self
            .attributes
            .iter()
            .map(|(k, v)| {
                json!({
                    "key": k.clone(),
                    "value": v.clone()
                })
            })
            .collect();

        json!({
            "traceId": self.trace_id.clone(),
            "spanId": self.span_id.clone(),
            "parentSpanId": self.parent_span_id.clone().unwrap_or_default(),
            "name": self.name.clone(),
            "serviceName": self.service_name.clone(),
            "toolName": self.tool_name.clone(),
            "kind": 2, // SPAN_KIND_SERVER
            "startTimeUnixNano": (self.start_time_us as u128 * 1_000) as i64,
            "endTimeUnixNano": ((self.start_time_us + self.duration_us) as u128 * 1_000) as i64,
            "durationUs": self.duration_us as i64,
            "status": {
                "code": if self.status == "OK" { 1 } else { 2 },
                "message": self.status.clone()
            },
            "attributes": JsonValue::Array(attrs)
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolType {
    Http,
    Grpc,
    TChannel,
    NativeMcp,
    ThirdParty,
    ExternalMcp,
    WasmPlugin,
}

impl ProtocolType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProtocolType::Http => "HTTP",
            ProtocolType::Grpc => "gRPC",
            ProtocolType::TChannel => "TChannel",
            ProtocolType::NativeMcp => "Native_MCP",
            ProtocolType::ThirdParty => "ThirdParty",
            ProtocolType::ExternalMcp => "External_MCP",
            ProtocolType::WasmPlugin => "WasmPlugin",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "gRPC" | "grpc" | "GRPC" => ProtocolType::Grpc,
            "TChannel" | "tchannel" | "TCHANNEL" => ProtocolType::TChannel,
            "Native_MCP" | "native_mcp" | "NATIVE_MCP" => ProtocolType::NativeMcp,
            "ThirdParty" | "third_party" | "THIRDPARTY" => ProtocolType::ThirdParty,
            "External_MCP" | "external_mcp" | "EXTERNAL_MCP" => ProtocolType::ExternalMcp,
            "WasmPlugin" | "wasm_plugin" | "WASM_PLUGIN" | "wasm" | "WASM" => {
                ProtocolType::WasmPlugin
            }
            _ => ProtocolType::Http,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Disabled,
    PendingReview,
    Enabled,
    Unavailable,
}

impl ToolStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ToolStatus::Disabled => "DISABLED",
            ToolStatus::PendingReview => "PENDING_REVIEW",
            ToolStatus::Enabled => "ENABLED",
            ToolStatus::Unavailable => "UNAVAILABLE",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s.to_uppercase().as_str() {
            "ENABLED" => ToolStatus::Enabled,
            "PENDING_REVIEW" => ToolStatus::PendingReview,
            "UNAVAILABLE" => ToolStatus::Unavailable,
            _ => ToolStatus::Disabled,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemaSource {
    Upstream,
    Human,
}

impl SchemaSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            SchemaSource::Upstream => "upstream",
            SchemaSource::Human => "human",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "human" => SchemaSource::Human,
            _ => SchemaSource::Upstream,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionMode {
    Stateless,
    Stateful,
}

impl SessionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionMode::Stateless => "stateless",
            SessionMode::Stateful => "stateful",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "stateful" => SessionMode::Stateful,
            _ => SessionMode::Stateless,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum UpstreamTransportConfig {
    Stdio {
        command: String,
        canonical_command: String,
        args: Vec<String>,
        env_refs: BTreeMap<String, String>,
        cwd: Option<String>,
    },
    StreamableHttp {
        url: String,
        bearer_ref: Option<String>,
        header_refs: BTreeMap<String, String>,
        session_mode: SessionMode,
    },
}

impl UpstreamTransportConfig {
    pub fn to_json(&self) -> JsonValue {
        match self {
            UpstreamTransportConfig::Stdio {
                command,
                canonical_command,
                args,
                env_refs,
                cwd,
            } => {
                let mut env_obj = JsonValue::object();
                for (k, v) in env_refs {
                    env_obj.insert(k.clone(), JsonValue::from(v.as_str()));
                }
                let args_arr: Vec<JsonValue> =
                    args.iter().map(|a| JsonValue::from(a.as_str())).collect();
                json!({
                    "transport": "stdio",
                    "command": command.clone(),
                    "canonical_command": canonical_command.clone(),
                    "args": JsonValue::Array(args_arr),
                    "env_refs": env_obj,
                    "cwd": cwd.as_ref().map(|c| JsonValue::from(c.as_str())).unwrap_or(JsonValue::Null),
                })
            }
            UpstreamTransportConfig::StreamableHttp {
                url,
                bearer_ref,
                header_refs,
                session_mode,
            } => {
                let mut hdr_obj = JsonValue::object();
                for (k, v) in header_refs {
                    hdr_obj.insert(k.clone(), JsonValue::from(v.as_str()));
                }
                json!({
                    "transport": "streamable_http",
                    "url": url.clone(),
                    "bearer_ref": bearer_ref.as_ref().map(|b| JsonValue::from(b.as_str())).unwrap_or(JsonValue::Null),
                    "header_refs": hdr_obj,
                    "session_mode": session_mode.as_str(),
                })
            }
        }
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let transport = v.get("transport")?.as_str()?;
        match transport.to_ascii_lowercase().as_str() {
            "stdio" => {
                let command = v.get("command")?.as_str()?.to_string();
                let canonical_command = v
                    .get("canonical_command")
                    .and_then(|x| x.as_str())
                    .unwrap_or(&command)
                    .to_string();
                let args = v
                    .get("args")
                    .and_then(|a| a.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let mut env_refs = BTreeMap::new();
                if let Some(obj) = v.get("env_refs").and_then(|o| o.as_object()) {
                    for (k, val) in obj {
                        if let Some(s) = val.as_str() {
                            env_refs.insert(k.clone(), s.to_string());
                        }
                    }
                }
                let cwd = v.get("cwd").and_then(|x| x.as_str()).map(String::from);
                Some(UpstreamTransportConfig::Stdio {
                    command,
                    canonical_command,
                    args,
                    env_refs,
                    cwd,
                })
            }
            "streamable_http" | "streamablehttp" | "http" => {
                let url = v.get("url")?.as_str()?.to_string();
                let bearer_ref = v
                    .get("bearer_ref")
                    .and_then(|b| b.as_str())
                    .map(String::from);
                let mut header_refs = BTreeMap::new();
                if let Some(obj) = v.get("header_refs").and_then(|o| o.as_object()) {
                    for (k, val) in obj {
                        if let Some(s) = val.as_str() {
                            header_refs.insert(k.clone(), s.to_string());
                        }
                    }
                }
                let session_mode = v
                    .get("session_mode")
                    .and_then(|x| x.as_str())
                    .map(SessionMode::from_str)
                    .unwrap_or(SessionMode::Stateless);
                Some(UpstreamTransportConfig::StreamableHttp {
                    url,
                    bearer_ref,
                    header_refs,
                    session_mode,
                })
            }
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeOrigin {
    Human,
    ExternalSync,
    AutoCrawler,
}

impl ChangeOrigin {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChangeOrigin::Human => "human",
            ChangeOrigin::ExternalSync => "external_sync",
            ChangeOrigin::AutoCrawler => "auto_crawler",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "external_sync" | "externalsync" => ChangeOrigin::ExternalSync,
            "auto_crawler" | "autocrawler" => ChangeOrigin::AutoCrawler,
            _ => ChangeOrigin::Human,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActorType {
    Human,
    Service,
    Agent,
}

impl ActorType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ActorType::Human => "human",
            ActorType::Service => "service",
            ActorType::Agent => "agent",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        let lower = s.to_lowercase();
        if lower == "human"
            || lower.contains("admin")
            || lower.contains("user")
            || lower.contains("approver")
            || lower.contains("operator")
        {
            Some(ActorType::Human)
        } else if lower == "agent" || lower.contains("agent") {
            Some(ActorType::Agent)
        } else if lower == "service" || lower.contains("service") {
            Some(ActorType::Service)
        } else {
            None
        }
    }
}

/// Declarative argument-level guardrail rule evaluated before tool execution (v6.5.0).
/// Example: `{ field: "/amount", op: "lte", value: 5000 }` or `{ field: "/environment", op: "neq", value: "prod" }`.
#[derive(Clone, Debug, PartialEq)]
pub struct ArgumentPolicyRule {
    pub field: String,
    pub op: String,
    pub value: JsonValue,
}

impl ArgumentPolicyRule {
    pub fn new(field: impl Into<String>, op: impl Into<String>, value: JsonValue) -> Self {
        Self {
            field: field.into(),
            op: op.into(),
            value,
        }
    }

    pub fn to_json(&self) -> JsonValue {
        json!({
            "field": self.field.clone(),
            "op": self.op.clone(),
            "value": self.value.clone(),
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let field = v.get("field")?.as_str()?.to_string();
        let op = v.get("op")?.as_str()?.to_string();
        let value = v.get("value").cloned().unwrap_or(JsonValue::Null);
        Some(Self { field, op, value })
    }
}

/// Per-tenant / per-principal hourly sliding-window resource quota policy (v6.5.0).
#[derive(Clone, Debug, PartialEq)]
pub struct TenantQuotaPolicy {
    pub max_tool_calls_per_hour: u64,
    pub max_codemode_fuel_per_hour: u64,
    pub max_egress_bytes_per_hour: u64,
}

impl Default for TenantQuotaPolicy {
    fn default() -> Self {
        Self {
            max_tool_calls_per_hour: 10_000,
            max_codemode_fuel_per_hour: 50_000_000,
            max_egress_bytes_per_hour: 500 * 1024 * 1024,
        }
    }
}

impl TenantQuotaPolicy {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "max_tool_calls_per_hour": self.max_tool_calls_per_hour as i64,
            "max_codemode_fuel_per_hour": self.max_codemode_fuel_per_hour as i64,
            "max_egress_bytes_per_hour": self.max_egress_bytes_per_hour as i64,
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        if !v.is_object() {
            return None;
        }
        let d = Self::default();
        Some(Self {
            max_tool_calls_per_hour: v
                .get("max_tool_calls_per_hour")
                .and_then(|x| x.as_u64())
                .unwrap_or(d.max_tool_calls_per_hour),
            max_codemode_fuel_per_hour: v
                .get("max_codemode_fuel_per_hour")
                .and_then(|x| x.as_u64())
                .unwrap_or(d.max_codemode_fuel_per_hour),
            max_egress_bytes_per_hour: v
                .get("max_egress_bytes_per_hour")
                .and_then(|x| x.as_u64())
                .unwrap_or(d.max_egress_bytes_per_hour),
        })
    }
}

/// Correlation frame for bidirectional upstream-initiated MCP requests (`sampling/createMessage`,
/// `elicitation/create`, `roots/list`) awaiting client relay or operator approval (v6.5.0).
#[derive(Clone, Debug, PartialEq)]
pub struct PendingUpstreamCallback {
    pub callback_id: String,
    pub server_name: String,
    pub upstream_request_id: JsonValue,
    pub method: String,
    pub params: JsonValue,
    pub mode: String,
    pub status: String,
    pub created_at: f64,
}

impl PendingUpstreamCallback {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "callback_id": self.callback_id.clone(),
            "server_name": self.server_name.clone(),
            "upstream_request_id": self.upstream_request_id.clone(),
            "method": self.method.clone(),
            "params": self.params.clone(),
            "mode": self.mode.clone(),
            "status": self.status.clone(),
            "created_at": self.created_at,
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let callback_id = v.get("callback_id")?.as_str()?.to_string();
        let server_name = v
            .get("server_name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let upstream_request_id = v
            .get("upstream_request_id")
            .cloned()
            .unwrap_or(JsonValue::Null);
        let method = v
            .get("method")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let params = v.get("params").cloned().unwrap_or_else(JsonValue::object);
        let mode = v
            .get("mode")
            .and_then(|x| x.as_str())
            .unwrap_or("approval_gate")
            .to_string();
        let status = v
            .get("status")
            .and_then(|x| x.as_str())
            .unwrap_or("pending")
            .to_string();
        let created_at = v
            .get("created_at")
            .and_then(|x| x.as_f64())
            .unwrap_or_else(now_epoch_secs);
        Some(Self {
            callback_id,
            server_name,
            upstream_request_id,
            method,
            params,
            mode,
            status,
            created_at,
        })
    }
}

/// Governance-driven Elicitation policy rule (`v6.8.0`).
/// Defines when a tool/server should emit an MCP `elicitation/create` (`form` or `url`)
/// request before executing a protected or missing-parameter action.
#[derive(Clone, Debug, PartialEq)]
pub struct ElicitationPolicyRule {
    pub mode: String, // "form" | "url"
    pub message: String,
    pub requested_schema: Option<JsonValue>,
    pub require_on_approval: bool,
}

impl ElicitationPolicyRule {
    /// Strict Secret-Safety & Primitive-Schema Validator for `form`-mode elicitations.
    /// Rejects any `requestedSchema` property whose name matches secret key patterns
    /// (`KEY`, `TOKEN`, `SECRET`, `PASSWORD`, `PASS`, `AUTH`, `CREDENTIAL`) or uses
    /// non-primitive JSON schema types (only `string`, `number`, `integer`, `boolean` are allowed).
    pub fn validate_elicitation_schema(schema: &JsonValue) -> Result<(), String> {
        if !schema.is_object() {
            return Err("requestedSchema must be a JSON object".to_string());
        }
        if let Some(t) = schema.get("type").and_then(|v| v.as_str()) {
            if t != "object" {
                return Err(format!(
                    "requestedSchema top-level type must be 'object', got '{}'",
                    t
                ));
            }
        }
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .ok_or_else(|| "requestedSchema must contain a 'properties' object".to_string())?;
        for (prop_name, prop_def) in props {
            if is_elicitation_secret_key(prop_name) {
                return Err(format!(
                    "Secret-safety violation: form elicitation property '{}' is forbidden (use url-mode OAuth instead)",
                    prop_name
                ));
            }
            if !prop_def.is_object() {
                return Err(format!(
                    "requestedSchema property '{}' must be a JSON schema object",
                    prop_name
                ));
            }
            let prop_type = prop_def.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if !matches!(prop_type, "string" | "number" | "integer" | "boolean") {
                return Err(format!(
                    "Primitive-schema violation: property '{}' has non-primitive type '{}' (allowed: string, number, integer, boolean)",
                    prop_name, prop_type
                ));
            }
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        match self.mode.as_str() {
            "form" => {
                if let Some(ref schema) = self.requested_schema {
                    Self::validate_elicitation_schema(schema)?;
                }
                Ok(())
            }
            "url" => Ok(()),
            other => Err(format!(
                "Unsupported elicitation mode '{}' (expected 'form' or 'url')",
                other
            )),
        }
    }

    pub fn to_json(&self) -> JsonValue {
        json!({
            "mode": self.mode.clone(),
            "message": self.message.clone(),
            "requested_schema": self.requested_schema.clone().unwrap_or(JsonValue::Null),
            "require_on_approval": self.require_on_approval,
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let mode = v
            .get("mode")
            .and_then(|x| x.as_str())
            .unwrap_or("form")
            .to_string();
        let message = v
            .get("message")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let requested_schema =
            v.get("requested_schema").and_then(
                |x| {
                    if x.is_null() {
                        None
                    } else {
                        Some(x.clone())
                    }
                },
            );
        let require_on_approval = v
            .get("require_on_approval")
            .and_then(|x| x.as_bool())
            .unwrap_or(true);
        Some(Self {
            mode,
            message,
            requested_schema,
            require_on_approval,
        })
    }
}

/// Returns `true` if a property/key name looks like a credential or secret that must NEVER
/// be collected via MCP `form`-mode elicitation.
pub fn is_elicitation_secret_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    [
        "KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASS",
        "AUTH",
        "CREDENTIAL",
    ]
    .iter()
    .any(|kw| upper.contains(kw))
}

#[derive(Clone, Debug, PartialEq)]
pub struct CharterPolicy {
    pub allowed_actors: Vec<ActorType>,
    pub allowed_tools: Vec<String>,
    pub rate_limit_rpm: u32,
    pub require_redaction: bool,
    pub custom_attributes: JsonValue,
    pub argument_rules: Vec<ArgumentPolicyRule>,
    pub tenant_quota: Option<TenantQuotaPolicy>,
    pub elicitation_rules: Vec<ElicitationPolicyRule>,
}

impl Default for CharterPolicy {
    fn default() -> Self {
        Self {
            allowed_actors: vec![ActorType::Human, ActorType::Service, ActorType::Agent],
            allowed_tools: vec!["*".to_string()],
            rate_limit_rpm: 1200,
            require_redaction: true,
            custom_attributes: JsonValue::object(),
            argument_rules: Vec::new(),
            tenant_quota: None,
            elicitation_rules: Vec::new(),
        }
    }
}

impl CharterPolicy {
    pub fn is_actor_allowed(&self, actor: &str) -> bool {
        if let Some(actor_enum) = ActorType::from_str(actor) {
            self.allowed_actors.contains(&actor_enum)
        } else {
            false
        }
    }

    pub fn is_tool_allowed(&self, tool_name: &str) -> bool {
        if self.allowed_tools.iter().any(|t| t == "*") {
            return true;
        }
        self.allowed_tools.iter().any(|t| t == tool_name)
    }

    pub fn to_json(&self) -> JsonValue {
        let actors: Vec<JsonValue> = self
            .allowed_actors
            .iter()
            .map(|a| JsonValue::from(a.as_str()))
            .collect();
        let tools: Vec<JsonValue> = self
            .allowed_tools
            .iter()
            .map(|t| JsonValue::from(t.as_str()))
            .collect();
        let arg_rules: Vec<JsonValue> = self.argument_rules.iter().map(|r| r.to_json()).collect();
        let elicit_rules: Vec<JsonValue> =
            self.elicitation_rules.iter().map(|r| r.to_json()).collect();
        json!({
            "allowed_actors": JsonValue::Array(actors),
            "allowed_tools": JsonValue::Array(tools),
            "rate_limit_rpm": self.rate_limit_rpm as i64,
            "require_redaction": self.require_redaction,
            "custom_attributes": self.custom_attributes.clone(),
            "argument_rules": JsonValue::Array(arg_rules),
            "tenant_quota": self.tenant_quota.as_ref().map(|q| q.to_json()).unwrap_or(JsonValue::Null),
            "elicitation_rules": JsonValue::Array(elicit_rules),
        })
    }

    pub fn from_json(v: &JsonValue) -> Self {
        let allowed_actors = v
            .get("allowed_actors")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().and_then(ActorType::from_str))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec![ActorType::Human, ActorType::Service, ActorType::Agent]);

        let allowed_tools = v
            .get("allowed_tools")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec!["*".to_string()]);

        let rate_limit_rpm = v
            .get("rate_limit_rpm")
            .and_then(|x| x.as_u64())
            .unwrap_or(1200) as u32;

        let require_redaction = v
            .get("require_redaction")
            .and_then(|x| x.as_bool())
            .unwrap_or(true);

        let custom_attributes = v
            .get("custom_attributes")
            .cloned()
            .unwrap_or_else(JsonValue::object);

        let argument_rules = v
            .get("argument_rules")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(ArgumentPolicyRule::from_json)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let tenant_quota = v.get("tenant_quota").and_then(|q| {
            if q.is_null() {
                None
            } else {
                TenantQuotaPolicy::from_json(q)
            }
        });

        let elicitation_rules = v
            .get("elicitation_rules")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(ElicitationPolicyRule::from_json)
                    .collect()
            })
            .unwrap_or_default();

        Self {
            allowed_actors,
            allowed_tools,
            rate_limit_rpm,
            require_redaction,
            custom_attributes,
            argument_rules,
            tenant_quota,
            elicitation_rules,
        }
    }
}

/// Declarative Saga compensation specification for state-mutating tools.
#[derive(Clone, Debug, PartialEq)]
pub struct MutationCompensationSpec {
    pub forward_tool: String,
    pub compensating_server: Option<String>,
    pub compensating_tool: String,
    /// Maps compensating tool argument key -> source path ("input.<field>", "output.<field>", or literal)
    pub arg_mapping: BTreeMap<String, String>,
}

impl MutationCompensationSpec {
    pub fn to_json(&self) -> JsonValue {
        let mut map_obj = JsonValue::object();
        for (k, v) in &self.arg_mapping {
            map_obj.insert(k.clone(), JsonValue::from(v.as_str()));
        }
        json!({
            "forward_tool": self.forward_tool.clone(),
            "compensating_server": self.compensating_server.as_ref().map(|s| JsonValue::from(s.as_str())).unwrap_or(JsonValue::Null),
            "compensating_tool": self.compensating_tool.clone(),
            "arg_mapping": map_obj,
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let forward_tool = v
            .get("forward_tool")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let compensating_server = v
            .get("compensating_server")
            .and_then(|x| x.as_str())
            .map(String::from);
        let compensating_tool = v.get("compensating_tool")?.as_str()?.to_string();
        let mut arg_mapping = BTreeMap::new();
        if let Some(entries) = v.get("arg_mapping").and_then(|m| m.as_object()) {
            for (k, val) in entries {
                if let Some(s) = val.as_str() {
                    arg_mapping.insert(k.clone(), s.to_string());
                }
            }
        }
        Some(Self {
            forward_tool,
            compensating_server,
            compensating_tool,
            arg_mapping,
        })
    }
}

/// Record of a completed mutation during a Code Mode script execution, used for LIFO Saga rollback.
#[derive(Clone, Debug, PartialEq)]
pub struct CompletedMutationRecord {
    pub server_name: String,
    pub tool_name: String,
    pub input_args: JsonValue,
    pub output_val: JsonValue,
    pub compensation_spec: Option<MutationCompensationSpec>,
}

impl CompletedMutationRecord {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "server_name": self.server_name.clone(),
            "tool_name": self.tool_name.clone(),
            "input_args": self.input_args.clone(),
            "output_val": self.output_val.clone(),
            "compensation_spec": self.compensation_spec.as_ref().map(|c| c.to_json()).unwrap_or(JsonValue::Null),
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let server_name = v.get("server_name")?.as_str()?.to_string();
        let tool_name = v.get("tool_name")?.as_str()?.to_string();
        let input_args = v
            .get("input_args")
            .cloned()
            .unwrap_or_else(JsonValue::object);
        let output_val = v.get("output_val").cloned().unwrap_or(JsonValue::Null);
        let compensation_spec = v.get("compensation_spec").and_then(|cs| {
            if cs.is_null() {
                None
            } else {
                MutationCompensationSpec::from_json(cs)
            }
        });
        Some(Self {
            server_name,
            tool_name,
            input_args,
            output_val,
            compensation_spec,
        })
    }
}

/// Record of a Saga compensation action executed when a multi-mutation script rolls back.
#[derive(Clone, Debug, PartialEq)]
pub struct SagaCompensationRecord {
    pub idempotency_key: String,
    pub forward_server: String,
    pub forward_tool: String,
    pub compensating_server: String,
    pub compensating_tool: String,
    pub compensating_args: JsonValue,
    pub status: String,
    pub compensation_output: JsonValue,
}

impl SagaCompensationRecord {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "idempotency_key": self.idempotency_key.clone(),
            "forward_server": self.forward_server.clone(),
            "forward_tool": self.forward_tool.clone(),
            "compensating_server": self.compensating_server.clone(),
            "compensating_tool": self.compensating_tool.clone(),
            "compensating_args": self.compensating_args.clone(),
            "status": self.status.clone(),
            "compensation_output": self.compensation_output.clone(),
        })
    }

    pub fn from_json(val: &JsonValue) -> Option<Self> {
        let forward_server = val.get("forward_server")?.as_str()?.to_string();
        let forward_tool = val.get("forward_tool")?.as_str()?.to_string();
        let compensating_server = val.get("compensating_server")?.as_str()?.to_string();
        let compensating_tool = val.get("compensating_tool")?.as_str()?.to_string();
        let compensating_args = val
            .get("compensating_args")
            .cloned()
            .unwrap_or(JsonValue::object());
        let status = val
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("PENDING")
            .to_string();
        let compensation_output = val
            .get("compensation_output")
            .cloned()
            .unwrap_or(JsonValue::Null);

        let idempotency_key = val
            .get("idempotency_key")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                format!("{}:{}:{}", forward_server, forward_tool, compensating_tool)
            });

        Some(Self {
            idempotency_key,
            forward_server,
            forward_tool,
            compensating_server,
            compensating_tool,
            compensating_args,
            status,
            compensation_output,
        })
    }
}

/// Serializable execution continuation frame persisted in `WalRegistryStore` when a script
/// hits an approval gate (`APPROVAL_REQUIRED`). Enables stateless horizontal suspend/resume.
#[derive(Clone, Debug, PartialEq)]
pub struct SuspendedExecutionFrame {
    pub execution_id: String,
    pub script_hash: String,
    pub script_source: String,
    pub instruction_ptr: usize,
    pub target_var: Option<String>,
    pub serialized_scope: JsonValue,
    pub pending_server: String,
    pub pending_tool: String,
    pub pending_arguments: JsonValue,
    pub pending_reason: String,
    pub policy_context_hash: String,
    pub caller_actor: String,
    pub logs: Vec<String>,
    pub completed_mutations: Vec<CompletedMutationRecord>,
    pub created_at: f64,
    pub pending_arguments_hash: String,
    pub expires_at: f64,
    pub approver_identity: Option<String>,
    pub is_consumed: bool,
    pub frame_hmac: String,
}

impl SuspendedExecutionFrame {
    /// Computes HMAC-SHA256 over canonical frame fields using `auth_secret`.
    pub fn compute_hmac(&self, secret: &str) -> String {
        // Length-prefixed canonical encoding covering the entire frame so no
        // field can be altered or shifted between fields without detection.
        let fields: [String; 12] = [
            self.execution_id.clone(),
            self.script_hash.clone(),
            self.script_source.clone(),
            self.instruction_ptr.to_string(),
            self.serialized_scope.to_string_compact(),
            self.pending_server.clone(),
            self.pending_tool.clone(),
            self.pending_arguments.to_string_compact(),
            self.pending_arguments_hash.clone(),
            self.policy_context_hash.clone(),
            self.caller_actor.clone(),
            (self.expires_at as u64).to_string(),
        ];
        let mut canonical = String::new();
        for f in &fields {
            canonical.push_str(&f.len().to_string());
            canonical.push(':');
            canonical.push_str(f);
        }
        crate::storage::audit_log::hmac_sha256_hex(secret.as_bytes(), canonical.as_bytes())
    }

    /// Verifies that frame HMAC matches the secret, has not expired, has not been consumed,
    /// and that resume arguments match `pending_arguments_hash`.
    pub fn verify_approval(
        &self,
        secret: &str,
        approver: &str,
        resume_args: Option<&JsonValue>,
        current_time: f64,
    ) -> Result<(), &'static str> {
        if self.is_consumed {
            return Err("Approval has already been consumed (single-use violated)");
        }
        if approver.trim().is_empty() || approver == "anonymous" {
            return Err("Unauthenticated or empty approver identity");
        }
        if approver == self.caller_actor {
            return Err("Approval denied: Requester cannot approve their own mutation (separation of duties)");
        }
        if current_time > self.expires_at {
            return Err("Approval frame has expired");
        }
        if self.frame_hmac.is_empty() {
            return Err("Frame HMAC signature missing (unsigned frames are rejected)");
        }
        let expected_hmac = self.compute_hmac(secret);
        if !constant_time_eq(self.frame_hmac.as_bytes(), expected_hmac.as_bytes()) {
            return Err("Frame HMAC signature mismatch (tampering detected)");
        }
        let actual_pending_hash = crate::storage::audit_log::sha256_hex(
            self.pending_arguments.to_string_compact().as_bytes(),
        );
        if !constant_time_eq(
            actual_pending_hash.as_bytes(),
            self.pending_arguments_hash.as_bytes(),
        ) {
            return Err("Pending arguments do not match their recorded hash");
        }
        if let Some(args) = resume_args {
            let actual_args_hash =
                crate::storage::audit_log::sha256_hex(args.to_string_compact().as_bytes());
            if actual_args_hash != self.pending_arguments_hash {
                return Err("Resume arguments do not match original pending arguments hash");
            }
        }
        Ok(())
    }

    pub fn to_json(&self) -> JsonValue {
        let logs_arr: Vec<JsonValue> = self
            .logs
            .iter()
            .map(|s| JsonValue::from(s.as_str()))
            .collect();
        let muts_arr: Vec<JsonValue> = self
            .completed_mutations
            .iter()
            .map(|m| m.to_json())
            .collect();
        json!({
            "execution_id": self.execution_id.clone(),
            "script_hash": self.script_hash.clone(),
            "script_source": self.script_source.clone(),
            "instruction_ptr": self.instruction_ptr as i64,
            "target_var": self.target_var.as_ref().map(|s| JsonValue::from(s.as_str())).unwrap_or(JsonValue::Null),
            "serialized_scope": self.serialized_scope.clone(),
            "pending_server": self.pending_server.clone(),
            "pending_tool": self.pending_tool.clone(),
            "pending_arguments": self.pending_arguments.clone(),
            "pending_reason": self.pending_reason.clone(),
            "policy_context_hash": self.policy_context_hash.clone(),
            "caller_actor": self.caller_actor.clone(),
            "logs": JsonValue::Array(logs_arr),
            "completed_mutations": JsonValue::Array(muts_arr),
            "created_at": self.created_at,
            "pending_arguments_hash": self.pending_arguments_hash.clone(),
            "expires_at": self.expires_at,
            "approver_identity": self.approver_identity.as_ref().map(|s| JsonValue::from(s.as_str())).unwrap_or(JsonValue::Null),
            "is_consumed": self.is_consumed,
            "frame_hmac": self.frame_hmac.clone(),
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let execution_id = v.get("execution_id")?.as_str()?.to_string();
        let script_hash = v.get("script_hash")?.as_str()?.to_string();
        let script_source = v.get("script_source")?.as_str()?.to_string();
        let instruction_ptr = v.get("instruction_ptr")?.as_u64()? as usize;
        let target_var = v
            .get("target_var")
            .and_then(|x| x.as_str())
            .map(String::from);
        let serialized_scope = v
            .get("serialized_scope")
            .cloned()
            .unwrap_or_else(JsonValue::object);
        let pending_server = v.get("pending_server")?.as_str()?.to_string();
        let pending_tool = v.get("pending_tool")?.as_str()?.to_string();
        let pending_arguments = v
            .get("pending_arguments")
            .cloned()
            .unwrap_or_else(JsonValue::object);
        let pending_reason = v
            .get("pending_reason")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let policy_context_hash = v
            .get("policy_context_hash")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let caller_actor = v
            .get("caller_actor")
            .and_then(|x| x.as_str())
            .unwrap_or("agent")
            .to_string();
        let logs = v
            .get("logs")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let completed_mutations = v
            .get("completed_mutations")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(CompletedMutationRecord::from_json)
                    .collect()
            })
            .unwrap_or_default();
        let created_at = v
            .get("created_at")
            .and_then(|x| x.as_f64())
            .unwrap_or_else(now_epoch_secs);

        let pending_arguments_hash = v
            .get("pending_arguments_hash")
            .and_then(|x| x.as_str())
            .map(String::from)
            .unwrap_or_else(|| {
                crate::storage::audit_log::sha256_hex(
                    pending_arguments.to_string_compact().as_bytes(),
                )
            });
        let expires_at = v
            .get("expires_at")
            .and_then(|x| x.as_f64())
            .unwrap_or(created_at + 3600.0);
        let approver_identity = v
            .get("approver_identity")
            .and_then(|x| x.as_str())
            .map(String::from);
        let is_consumed = v
            .get("is_consumed")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        let frame_hmac = v
            .get("frame_hmac")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();

        Some(Self {
            execution_id,
            script_hash,
            script_source,
            instruction_ptr,
            target_var,
            serialized_scope,
            pending_server,
            pending_tool,
            pending_arguments,
            pending_reason,
            policy_context_hash,
            caller_actor,
            logs,
            completed_mutations,
            created_at,
            pending_arguments_hash,
            expires_at,
            approver_identity,
            is_consumed,
            frame_hmac,
        })
    }
}

#[derive(Clone, Debug)]
pub struct ToolRecord {
    pub name: String,
    pub description: String,
    pub input_schema: JsonValue,
    pub output_schema: JsonValue,
    pub status: ToolStatus,
    pub version: u32,
    pub protocol: ProtocolType,
    pub downstream_endpoint: String,
    pub downstream_method: String,
    pub charter_policy: Option<CharterPolicy>,
    pub metadata: JsonValue,
    pub idempotent_cacheable: bool,
    pub canary_version: Option<u32>,
    pub canary_weight_pct: u8,
    pub fallback_tool: Option<String>,
    pub tags: Vec<String>,
    pub is_mutation: bool,
    pub compensation_spec: Option<MutationCompensationSpec>,
    pub schema_source: SchemaSource,
    pub retired_at: Option<f64>,
    pub last_seen_in_sync: Option<f64>,
    pub timeout_ms: Option<u64>,
    pub http_method: Option<String>,
    pub cache_ttl_sec: Option<u64>,
    pub wasm_module_base64: Option<String>,
}

impl ToolRecord {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: JsonValue,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            output_schema: json!({"type": "object"}),
            status: ToolStatus::Disabled,
            version: 1,
            protocol: ProtocolType::Http,
            downstream_endpoint: String::new(),
            downstream_method: String::new(),
            charter_policy: None,
            metadata: JsonValue::object(),
            idempotent_cacheable: true,
            canary_version: None,
            canary_weight_pct: 0,
            fallback_tool: None,
            tags: Vec::new(),
            is_mutation: false,
            compensation_spec: None,
            schema_source: SchemaSource::Upstream,
            retired_at: None,
            last_seen_in_sync: None,
            timeout_ms: None,
            http_method: None,
            cache_ttl_sec: None,
            wasm_module_base64: None,
        }
    }

    pub fn to_mcp_spec(&self, enable_projection: bool) -> JsonValue {
        let mut schema = if self.input_schema.is_object() {
            self.input_schema.clone()
        } else {
            json!({"type": "object", "properties": {}})
        };

        if enable_projection {
            let mut props = schema
                .get("properties")
                .cloned()
                .unwrap_or_else(JsonValue::object);
            props.insert(
                "_projection",
                json!({
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "GraphQL-like nested field projections. Specify only fields needed to conserve context tokens."
                }),
            );
            schema.insert("properties", props);
        }

        json!({
            "name": self.name.clone(),
            "description": self.description.clone(),
            "inputSchema": schema,
            "annotations": {
                "readOnlyHint": !self.is_mutation,
                "destructiveHint": self.is_mutation,
                "idempotentHint": self.idempotent_cacheable,
                "openWorldHint": false,
            },
        })
    }

    pub fn to_json(&self) -> JsonValue {
        let policy_val = match &self.charter_policy {
            Some(p) => p.to_json(),
            None => JsonValue::Null,
        };
        let comp_val = match &self.compensation_spec {
            Some(c) => c.to_json(),
            None => JsonValue::Null,
        };
        json!({
            "name": self.name.clone(),
            "description": self.description.clone(),
            "input_schema": self.input_schema.clone(),
            "output_schema": self.output_schema.clone(),
            "status": self.status.as_str(),
            "version": self.version as i64,
            "protocol": self.protocol.as_str(),
            "downstream_endpoint": self.downstream_endpoint.clone(),
            "downstream_method": self.downstream_method.clone(),
            "charter_policy": policy_val,
            "metadata": self.metadata.clone(),
            "idempotent_cacheable": self.idempotent_cacheable,
            "canary_version": self.canary_version.map(|v| v as i64).map(JsonValue::from).unwrap_or(JsonValue::Null),
            "canary_weight_pct": self.canary_weight_pct as i64,
            "fallback_tool": self.fallback_tool.as_ref().map(|s| JsonValue::from(s.as_str())).unwrap_or(JsonValue::Null),
            "tags": JsonValue::Array(self.tags.iter().map(|t| JsonValue::from(t.as_str())).collect()),
            "is_mutation": self.is_mutation,
            "compensation_spec": comp_val,
            "schema_source": self.schema_source.as_str(),
            "retired_at": self.retired_at,
            "last_seen_in_sync": self.last_seen_in_sync,
            "timeout_ms": self.timeout_ms.map(|v| v as i64).map(JsonValue::from).unwrap_or(JsonValue::Null),
            "http_method": self.http_method.as_ref().map(|s| JsonValue::from(s.as_str())).unwrap_or(JsonValue::Null),
            "cache_ttl_sec": self.cache_ttl_sec.map(|v| v as i64).map(JsonValue::from).unwrap_or(JsonValue::Null),
            "wasm_module_base64": self.wasm_module_base64.as_ref().map(|s| JsonValue::from(s.as_str())).unwrap_or(JsonValue::Null),
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let name = v.get("name")?.as_str()?.to_string();
        let description = v
            .get("description")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let input_schema = v
            .get("input_schema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
        let output_schema = v
            .get("output_schema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"}));
        let status = v
            .get("status")
            .and_then(|x| x.as_str())
            .map(ToolStatus::from_str)
            .unwrap_or(ToolStatus::Disabled);
        let version = v.get("version").and_then(|x| x.as_u64()).unwrap_or(1) as u32;
        let protocol = v
            .get("protocol")
            .and_then(|x| x.as_str())
            .map(ProtocolType::from_str)
            .unwrap_or(ProtocolType::Http);
        let downstream_endpoint = v
            .get("downstream_endpoint")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let downstream_method = v
            .get("downstream_method")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let charter_policy = v.get("charter_policy").and_then(|cp| {
            if cp.is_null() {
                None
            } else {
                Some(CharterPolicy::from_json(cp))
            }
        });
        let metadata = v.get("metadata").cloned().unwrap_or_else(JsonValue::object);
        let idempotent_cacheable = v
            .get("idempotent_cacheable")
            .and_then(|x| x.as_bool())
            .unwrap_or(true);
        let canary_version = v
            .get("canary_version")
            .and_then(|x| x.as_u64())
            .map(|u| u as u32);
        let canary_weight_pct = v
            .get("canary_weight_pct")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u8;
        let fallback_tool = v
            .get("fallback_tool")
            .and_then(|x| x.as_str())
            .map(String::from);
        let tags = v
            .get("tags")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let is_mutation = v
            .get("is_mutation")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        let compensation_spec = v.get("compensation_spec").and_then(|cs| {
            if cs.is_null() {
                None
            } else {
                MutationCompensationSpec::from_json(cs)
            }
        });
        let schema_source = v
            .get("schema_source")
            .and_then(|x| x.as_str())
            .map(SchemaSource::from_str)
            .unwrap_or(SchemaSource::Upstream);
        let retired_at = v.get("retired_at").and_then(|x| x.as_f64());
        let last_seen_in_sync = v.get("last_seen_in_sync").and_then(|x| x.as_f64());
        let timeout_ms = v.get("timeout_ms").and_then(|x| x.as_u64());
        let http_method = v
            .get("http_method")
            .and_then(|x| x.as_str())
            .map(String::from);
        let cache_ttl_sec = v.get("cache_ttl_sec").and_then(|x| x.as_u64());
        let wasm_module_base64 = v
            .get("wasm_module_base64")
            .and_then(|x| x.as_str())
            .map(String::from);

        Some(Self {
            name,
            description,
            input_schema,
            output_schema,
            status,
            version,
            protocol,
            downstream_endpoint,
            downstream_method,
            charter_policy,
            metadata,
            idempotent_cacheable,
            canary_version,
            canary_weight_pct,
            fallback_tool,
            tags,
            is_mutation,
            compensation_spec,
            schema_source,
            retired_at,
            last_seen_in_sync,
            timeout_ms,
            http_method,
            cache_ttl_sec,
            wasm_module_base64,
        })
    }
}

#[derive(Clone, Debug)]
pub struct ConfigDiff {
    pub diff_id: String,
    pub server_name: String,
    pub tool_name: String,
    pub previous_version: u32,
    pub proposed_version: u32,
    pub before_state: JsonValue,
    pub after_state: JsonValue,
    pub changes_summary: Vec<String>,
    pub proposed_by: String,
    pub created_at: f64,
    pub approved: bool,
    pub approved_by: Option<String>,
    pub approved_at: Option<f64>,
    pub origin: ChangeOrigin,
}

impl ConfigDiff {
    pub fn to_json(&self) -> JsonValue {
        let changes: Vec<JsonValue> = self
            .changes_summary
            .iter()
            .map(|c| JsonValue::from(c.as_str()))
            .collect();
        json!({
            "diff_id": self.diff_id.clone(),
            "server_name": self.server_name.clone(),
            "tool_name": self.tool_name.clone(),
            "previous_version": self.previous_version as i64,
            "proposed_version": self.proposed_version as i64,
            "before_state": self.before_state.clone(),
            "after_state": self.after_state.clone(),
            "changes_summary": JsonValue::Array(changes),
            "proposed_by": self.proposed_by.clone(),
            "created_at": self.created_at,
            "approved": self.approved,
            "approved_by": self.approved_by.clone(),
            "approved_at": self.approved_at,
            "origin": self.origin.as_str(),
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let origin = v
            .get("origin")
            .and_then(|x| x.as_str())
            .map(ChangeOrigin::from_str)
            .unwrap_or(ChangeOrigin::Human);

        Some(Self {
            diff_id: v.get("diff_id")?.as_str()?.to_string(),
            server_name: v.get("server_name")?.as_str()?.to_string(),
            tool_name: v.get("tool_name")?.as_str()?.to_string(),
            previous_version: v.get("previous_version")?.as_u64()? as u32,
            proposed_version: v.get("proposed_version")?.as_u64()? as u32,
            before_state: v.get("before_state")?.clone(),
            after_state: v.get("after_state")?.clone(),
            changes_summary: v
                .get("changes_summary")
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            proposed_by: v
                .get("proposed_by")
                .and_then(|x| x.as_str())
                .unwrap_or("service_owner")
                .to_string(),
            created_at: v
                .get("created_at")
                .and_then(|x| x.as_f64())
                .unwrap_or_else(now_epoch_secs),
            approved: v.get("approved").and_then(|x| x.as_bool()).unwrap_or(false),
            approved_by: v
                .get("approved_by")
                .and_then(|x| x.as_str())
                .map(String::from),
            approved_at: v.get("approved_at").and_then(|x| x.as_f64()),
            origin,
        })
    }
}

#[derive(Clone, Debug)]
pub struct HistoryEntry {
    pub action: String,
    pub version: u32,
    pub time: f64,
}

impl HistoryEntry {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "action": self.action.clone(),
            "version": self.version as i64,
            "time": self.time,
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        Some(Self {
            action: v.get("action")?.as_str()?.to_string(),
            version: v.get("version")?.as_u64()? as u32,
            time: v.get("time")?.as_f64()?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpstreamPolicy {
    Deny,
    RelayToApproval,
}

impl UpstreamPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            UpstreamPolicy::Deny => "deny",
            UpstreamPolicy::RelayToApproval => "relay_to_approval",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "relay_to_approval" | "relay" | "approval" => UpstreamPolicy::RelayToApproval,
            _ => UpstreamPolicy::Deny,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExternalResourceRecord {
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
    pub mime_type: Option<String>,
}

impl ExternalResourceRecord {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "uri": self.uri.clone(),
            "name": self.name.clone(),
            "description": self.description.as_ref().map(|s| JsonValue::from(s.as_str())).unwrap_or(JsonValue::Null),
            "mime_type": self.mime_type.as_ref().map(|s| JsonValue::from(s.as_str())).unwrap_or(JsonValue::Null),
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let uri = v.get("uri")?.as_str()?.to_string();
        let name = v
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or(&uri)
            .to_string();
        let description = v
            .get("description")
            .and_then(|x| x.as_str())
            .map(String::from);
        let mime_type = v
            .get("mime_type")
            .or_else(|| v.get("mimeType"))
            .and_then(|x| x.as_str())
            .map(String::from);
        Some(Self {
            uri,
            name,
            description,
            mime_type,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExternalPromptRecord {
    pub name: String,
    pub description: Option<String>,
    pub arguments: JsonValue,
}

impl ExternalPromptRecord {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "name": self.name.clone(),
            "description": self.description.as_ref().map(|s| JsonValue::from(s.as_str())).unwrap_or(JsonValue::Null),
            "arguments": self.arguments.clone(),
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let name = v.get("name")?.as_str()?.to_string();
        let description = v
            .get("description")
            .and_then(|x| x.as_str())
            .map(String::from);
        let arguments = v
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| JsonValue::Array(Vec::new()));
        Some(Self {
            name,
            description,
            arguments,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolGroupEntry {
    pub server_name: String,
    pub tool_name: Option<String>, // None means all enabled tools on server_name
}

impl ToolGroupEntry {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "server_name": self.server_name.clone(),
            "tool_name": self.tool_name.as_ref().map(|s| JsonValue::from(s.as_str())).unwrap_or(JsonValue::Null),
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let server_name = v.get("server_name")?.as_str()?.to_string();
        let tool_name = v
            .get("tool_name")
            .and_then(|x| x.as_str())
            .map(String::from);
        Some(Self {
            server_name,
            tool_name,
        })
    }
}

#[derive(Clone, Debug)]
pub struct ToolGroupRecord {
    pub slug: String,
    pub display_name: String,
    pub description: String,
    pub entries: Vec<ToolGroupEntry>,
    pub created_at: f64,
}

impl ToolGroupRecord {
    pub fn new(
        slug: impl Into<String>,
        display_name: impl Into<String>,
        description: impl Into<String>,
        entries: Vec<ToolGroupEntry>,
    ) -> Self {
        Self {
            slug: slug.into(),
            display_name: display_name.into(),
            description: description.into(),
            entries,
            created_at: now_epoch_secs(),
        }
    }

    pub fn to_json(&self) -> JsonValue {
        json!({
            "slug": self.slug.clone(),
            "display_name": self.display_name.clone(),
            "description": self.description.clone(),
            "entries": JsonValue::Array(self.entries.iter().map(|e| e.to_json()).collect()),
            "created_at": self.created_at,
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let slug = v.get("slug")?.as_str()?.to_string();
        let display_name = v
            .get("display_name")
            .and_then(|x| x.as_str())
            .unwrap_or(&slug)
            .to_string();
        let description = v
            .get("description")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let entries = v
            .get("entries")
            .and_then(|e| e.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(ToolGroupEntry::from_json)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let created_at = v.get("created_at").and_then(|x| x.as_f64()).unwrap_or(0.0);
        Some(Self {
            slug,
            display_name,
            description,
            entries,
            created_at,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum SamplingElicitationPolicy {
    #[default]
    Deny,
    RelayToClient,
    ApprovalGate,
}

impl SamplingElicitationPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::RelayToClient => "relay_to_client",
            Self::ApprovalGate => "approval_gate",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "relay_to_client" | "relaytoclient" | "relay" => Self::RelayToClient,
            "approval_gate" | "approvalgate" | "approval" => Self::ApprovalGate,
            _ => Self::Deny,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum ServerHealth {
    #[default]
    Connected,
    Degraded,
    Unreachable,
}

impl ServerHealth {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Degraded => "degraded",
            Self::Unreachable => "unreachable",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "degraded" => Self::Degraded,
            "unreachable" => Self::Unreachable,
            _ => Self::Connected,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ServerRecord {
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub owning_team: String,
    pub protocol: ProtocolType,
    pub status: ToolStatus,
    pub version: u32,
    pub tools: BTreeMap<String, ToolRecord>,
    pub charter_policy: CharterPolicy,
    pub heartbeat_timestamp: Option<f64>,
    pub is_native: bool,
    pub is_third_party: bool,
    pub third_party_provider: Option<String>,
    pub history: Vec<HistoryEntry>,
    pub upstream_config: Option<UpstreamTransportConfig>,
    pub sync_interval_sec: Option<u64>,
    pub upstream_policy: UpstreamPolicy,
    pub sampling_elicitation_policy: SamplingElicitationPolicy,
    pub health: ServerHealth,
    pub external_resources: Vec<ExternalResourceRecord>,
    pub external_prompts: Vec<ExternalPromptRecord>,
}

impl ServerRecord {
    pub fn new(
        name: impl Into<String>,
        display_name: impl Into<String>,
        description: impl Into<String>,
        owning_team: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            display_name: display_name.into(),
            description: description.into(),
            owning_team: owning_team.into(),
            protocol: ProtocolType::Http,
            status: ToolStatus::Disabled,
            version: 1,
            tools: BTreeMap::new(),
            charter_policy: CharterPolicy::default(),
            heartbeat_timestamp: None,
            is_native: false,
            is_third_party: false,
            third_party_provider: None,
            history: Vec::new(),
            upstream_config: None,
            sync_interval_sec: None,
            upstream_policy: UpstreamPolicy::Deny,
            sampling_elicitation_policy: SamplingElicitationPolicy::Deny,
            health: ServerHealth::Connected,
            external_resources: Vec::new(),
            external_prompts: Vec::new(),
        }
    }

    pub fn get_tool(&self, tool_name: &str) -> Option<&ToolRecord> {
        self.tools.get(tool_name)
    }

    pub fn get_tool_mut(&mut self, tool_name: &str) -> Option<&mut ToolRecord> {
        self.tools.get_mut(tool_name)
    }

    pub fn add_tool(&mut self, tool: ToolRecord) {
        self.tools.insert(tool.name.clone(), tool);
    }

    pub fn enabled_tools_count(&self) -> usize {
        self.tools
            .values()
            .filter(|t| t.status == ToolStatus::Enabled)
            .count()
    }

    pub fn to_json(&self) -> JsonValue {
        let mut tools_obj = JsonValue::object();
        for (k, t) in &self.tools {
            tools_obj.insert(k.clone(), t.to_json());
        }
        let history_arr: Vec<JsonValue> = self.history.iter().map(|h| h.to_json()).collect();
        json!({
            "name": self.name.clone(),
            "display_name": self.display_name.clone(),
            "description": self.description.clone(),
            "owning_team": self.owning_team.clone(),
            "protocol": self.protocol.as_str(),
            "status": self.status.as_str(),
            "version": self.version as i64,
            "tools": tools_obj,
            "charter_policy": self.charter_policy.to_json(),
            "heartbeat_timestamp": self.heartbeat_timestamp,
            "is_native": self.is_native,
            "is_third_party": self.is_third_party,
            "third_party_provider": self.third_party_provider.clone(),
            "history": JsonValue::Array(history_arr),
            "upstream_config": self
                .upstream_config
                .as_ref()
                .map(|c| c.to_json())
                .unwrap_or(JsonValue::Null),
            "sync_interval_sec": self.sync_interval_sec.map(|v| v as i64).map(JsonValue::from).unwrap_or(JsonValue::Null),
            "upstream_policy": self.upstream_policy.as_str(),
            "sampling_elicitation_policy": self.sampling_elicitation_policy.as_str(),
            "health": self.health.as_str(),
            "external_resources": JsonValue::Array(self.external_resources.iter().map(|r| r.to_json()).collect()),
            "external_prompts": JsonValue::Array(self.external_prompts.iter().map(|p| p.to_json()).collect()),
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let name = v.get("name")?.as_str()?.to_string();
        let display_name = v
            .get("display_name")
            .and_then(|x| x.as_str())
            .unwrap_or(&name)
            .to_string();
        let description = v
            .get("description")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let owning_team = v
            .get("owning_team")
            .and_then(|x| x.as_str())
            .unwrap_or("engineering")
            .to_string();
        let protocol = v
            .get("protocol")
            .and_then(|x| x.as_str())
            .map(ProtocolType::from_str)
            .unwrap_or(ProtocolType::Http);
        let status = v
            .get("status")
            .and_then(|x| x.as_str())
            .map(ToolStatus::from_str)
            .unwrap_or(ToolStatus::Disabled);
        let version = v.get("version").and_then(|x| x.as_u64()).unwrap_or(1) as u32;

        let mut tools = BTreeMap::new();
        if let Some(tools_entries) = v.get("tools").and_then(|t| t.as_object()) {
            for (t_name, t_val) in tools_entries {
                if let Some(tr) = ToolRecord::from_json(t_val) {
                    tools.insert(t_name.clone(), tr);
                }
            }
        }

        let charter_policy = v
            .get("charter_policy")
            .map(CharterPolicy::from_json)
            .unwrap_or_default();

        let heartbeat_timestamp = v.get("heartbeat_timestamp").and_then(|x| x.as_f64());
        let is_native = v
            .get("is_native")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        let is_third_party = v
            .get("is_third_party")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        let third_party_provider = v
            .get("third_party_provider")
            .and_then(|x| x.as_str())
            .map(String::from);

        let history = v
            .get("history")
            .and_then(|h| h.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(HistoryEntry::from_json)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let upstream_config = v.get("upstream_config").and_then(|c| {
            if c.is_null() {
                None
            } else {
                UpstreamTransportConfig::from_json(c)
            }
        });
        let sync_interval_sec = v.get("sync_interval_sec").and_then(|x| x.as_u64());
        let upstream_policy = v
            .get("upstream_policy")
            .and_then(|x| x.as_str())
            .map(UpstreamPolicy::from_str)
            .unwrap_or(UpstreamPolicy::Deny);
        let sampling_elicitation_policy = v
            .get("sampling_elicitation_policy")
            .and_then(|x| x.as_str())
            .map(SamplingElicitationPolicy::from_str)
            .unwrap_or(SamplingElicitationPolicy::Deny);
        let health = v
            .get("health")
            .and_then(|x| x.as_str())
            .map(ServerHealth::from_str)
            .unwrap_or(ServerHealth::Connected);
        let external_resources = v
            .get("external_resources")
            .and_then(|r| r.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(ExternalResourceRecord::from_json)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let external_prompts = v
            .get("external_prompts")
            .and_then(|p| p.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(ExternalPromptRecord::from_json)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        Some(Self {
            name,
            display_name,
            description,
            owning_team,
            protocol,
            status,
            version,
            tools,
            charter_policy,
            heartbeat_timestamp,
            is_native,
            is_third_party,
            third_party_provider,
            history,
            upstream_config,
            sync_interval_sec,
            upstream_policy,
            sampling_elicitation_policy,
            health,
            external_resources,
            external_prompts,
        })
    }
}

// ============================================================================
// v6.8.0 Data Models: Skills over MCP, Postgres HA Tasks, Multi-Pod OAuth, File Blobs
// ============================================================================

/// Maximum number of files permitted in a single MCP Skill bundle (SEP-2640).
pub const SKILL_MAX_FILES: usize = 512;
/// Maximum total payload size (16 MiB) permitted in a single MCP Skill bundle (SEP-2640).
pub const SKILL_MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;

/// A single file entry within an MCP Skill bundle (`io.modelcontextprotocol/skills`).
#[derive(Clone, Debug, PartialEq)]
pub struct SkillFileEntry {
    pub path: String,
    pub mime_type: String,
    pub content: String,
    pub size_bytes: usize,
    pub sha256: String,
}

impl SkillFileEntry {
    pub fn new(
        path: impl Into<String>,
        mime_type: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        let path = path.into();
        let mime_type = mime_type.into();
        let content = content.into();
        let size_bytes = content.len();
        let sha256 = crate::storage::audit_log::sha256_hex(content.as_bytes());
        Self {
            path,
            mime_type,
            content,
            size_bytes,
            sha256,
        }
    }

    pub fn to_json(&self) -> JsonValue {
        json!({
            "path": self.path.clone(),
            "mime_type": self.mime_type.clone(),
            "content": self.content.clone(),
            "size_bytes": self.size_bytes as i64,
            "sha256": self.sha256.clone(),
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let path = v.get("path")?.as_str()?.to_string();
        let mime_type = v
            .get("mime_type")
            .or_else(|| v.get("mimeType"))
            .and_then(|x| x.as_str())
            .unwrap_or("text/markdown")
            .to_string();
        let content = v
            .get("content")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let size_bytes = v
            .get("size_bytes")
            .or_else(|| v.get("sizeBytes"))
            .and_then(|x| x.as_u64())
            .map(|n| n as usize)
            .unwrap_or_else(|| content.len());
        let sha256 = v
            .get("sha256")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| crate::storage::audit_log::sha256_hex(content.as_bytes()));
        Some(Self {
            path,
            mime_type,
            content,
            size_bytes,
            sha256,
        })
    }
}

/// Validates a relative skill file path against traversal, absolute paths, backslashes, or NUL bytes.
pub fn validate_skill_file_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("Skill file path cannot be empty".to_string());
    }
    if path.contains('\0') {
        return Err(format!("Skill file path '{}' contains NUL byte", path));
    }
    if path.starts_with('/') || path.contains('\\') || path.starts_with('~') {
        return Err(format!(
            "Skill file path '{}' must be a relative POSIX path",
            path
        ));
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(format!(
                "Skill file path '{}' contains invalid traversal or empty segment",
                path
            ));
        }
    }
    Ok(())
}

/// Extracts `(name, description)` from `SKILL.md` YAML frontmatter (`---\n...\n---`).
/// Supports top-level single-line scalars as well as YAML folded (`>`) and literal (`|`)
/// multi-line block scalars while ignoring indented nested YAML keys.
pub fn parse_skill_frontmatter(markdown: &str) -> Result<(String, String), String> {
    let trimmed = markdown.trim_start_matches('\u{feff}');
    let mut lines = trimmed.lines().peekable();
    let first = lines.next().unwrap_or("").trim();
    if first != "---" {
        return Err("SKILL.md must begin with YAML frontmatter delimiter '---'".to_string());
    }
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut closed = false;

    while let Some(line) = lines.next() {
        let t = line.trim();
        if t == "---" || t == "..." {
            closed = true;
            break;
        }
        // Ignore empty lines, comments, and indented nested YAML keys/list items.
        if t.is_empty() || t.starts_with('#') || line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            let raw_val = v.trim();
            if key != "name" && key != "description" {
                continue;
            }
            let resolved = if matches!(raw_val, ">" | ">-" | ">+" | "|" | "|-" | "|+") {
                let is_folded = raw_val.starts_with('>');
                let mut block_lines: Vec<String> = Vec::new();
                while let Some(&next_line) = lines.peek() {
                    let next_trim = next_line.trim();
                    if next_trim == "---" || next_trim == "..." {
                        break;
                    }
                    if next_trim.is_empty() {
                        block_lines.push(String::new());
                        lines.next();
                    } else if next_line.starts_with(' ') || next_line.starts_with('\t') {
                        block_lines.push(next_trim.to_string());
                        lines.next();
                    } else {
                        break;
                    }
                }
                while block_lines.last().is_some_and(|l| l.is_empty()) {
                    block_lines.pop();
                }
                if is_folded {
                    block_lines
                        .into_iter()
                        .filter(|l| !l.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ")
                } else {
                    block_lines.join("\n")
                }
            } else {
                raw_val
                    .trim_matches('"')
                    .trim_matches('\'')
                    .trim()
                    .to_string()
            };

            if !resolved.is_empty() {
                if key == "name" {
                    name = Some(resolved);
                } else if key == "description" {
                    description = Some(resolved);
                }
            }
        }
    }
    if !closed {
        return Err("SKILL.md YAML frontmatter is missing closing '---' delimiter".to_string());
    }
    let name =
        name.ok_or_else(|| "SKILL.md frontmatter missing required 'name' field".to_string())?;
    let description = description
        .ok_or_else(|| "SKILL.md frontmatter missing required 'description' field".to_string())?;
    Ok((name, description))
}

/// Enterprise MCP Skill bundle record (`io.modelcontextprotocol/skills`, SEP-2640).
#[derive(Clone, Debug, PartialEq)]
pub struct SkillRecord {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub group_slug: Option<String>,
    pub server_scope: Option<String>,
    pub auto_generated: bool,
    pub files: Vec<SkillFileEntry>,
    pub manifest_sha256: String,
    pub updated_at: f64,
}

impl SkillRecord {
    /// Computes the deterministic SHA-256 manifest digest over sorted
    /// `path + "\0" + size_bytes + "\0" + file.sha256 + "\n"`.
    pub fn compute_manifest_sha256(files: &[SkillFileEntry]) -> String {
        let mut sorted: Vec<&SkillFileEntry> = files.iter().collect();
        sorted.sort_by(|a, b| a.path.cmp(&b.path));
        let mut buf = String::new();
        for f in sorted {
            let file_hash = if f.sha256.is_empty() {
                crate::storage::audit_log::sha256_hex(f.content.as_bytes())
            } else {
                f.sha256.clone()
            };
            buf.push_str(&f.path);
            buf.push('\0');
            buf.push_str(&f.content.len().to_string());
            buf.push('\0');
            buf.push_str(&file_hash);
            buf.push('\n');
        }
        crate::storage::audit_log::sha256_hex(buf.as_bytes())
    }

    /// Validates SEP-2640 limits, path safety, duplicate paths, and `SKILL.md` YAML frontmatter,
    /// then refreshes `manifest_sha256` deterministically.
    pub fn validate(&mut self) -> Result<(), String> {
        if self.slug.trim().is_empty()
            || !self
                .slug
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(format!(
                "Invalid skill slug '{}': only [a-zA-Z0-9._-] allowed",
                self.slug
            ));
        }
        if self.files.is_empty() {
            return Err("Skill bundle must contain at least SKILL.md".to_string());
        }
        if self.files.len() > SKILL_MAX_FILES {
            return Err(format!(
                "Skill '{}' has {} files, exceeding maximum of {} files",
                self.slug,
                self.files.len(),
                SKILL_MAX_FILES
            ));
        }
        let mut seen = std::collections::HashSet::new();
        let mut total_bytes: usize = 0;
        let mut root_skill_md: Option<String> = None;

        for f in &mut self.files {
            validate_skill_file_path(&f.path)?;
            if !seen.insert(f.path.clone()) {
                return Err(format!(
                    "Duplicate file path '{}' in skill '{}'",
                    f.path, self.slug
                ));
            }
            f.size_bytes = f.content.len();
            f.sha256 = crate::storage::audit_log::sha256_hex(f.content.as_bytes());
            total_bytes = total_bytes.saturating_add(f.size_bytes);
            if total_bytes > SKILL_MAX_TOTAL_BYTES {
                return Err(format!(
                    "Skill '{}' total size {} bytes exceeds 16 MiB ({} bytes) limit",
                    self.slug, total_bytes, SKILL_MAX_TOTAL_BYTES
                ));
            }
            if f.path == "SKILL.md" {
                root_skill_md = Some(f.content.clone());
            }
        }

        let skill_md = root_skill_md
            .ok_or_else(|| format!("Skill '{}' is missing required root SKILL.md", self.slug))?;
        let (fm_name, fm_desc) = parse_skill_frontmatter(&skill_md)?;
        if self.name.trim().is_empty() {
            self.name = fm_name;
        }
        if self.description.trim().is_empty() {
            self.description = fm_desc;
        }
        self.files.sort_by(|a, b| a.path.cmp(&b.path));
        self.manifest_sha256 = Self::compute_manifest_sha256(&self.files);
        if self.version.trim().is_empty() {
            self.version = format!("sha256:{}", &self.manifest_sha256[..12]);
        }
        Ok(())
    }

    pub fn total_bytes(&self) -> usize {
        self.files.iter().map(|f| f.size_bytes).sum()
    }

    pub fn to_json(&self) -> JsonValue {
        let files_arr: Vec<JsonValue> = self.files.iter().map(|f| f.to_json()).collect();
        json!({
            "slug": self.slug.clone(),
            "name": self.name.clone(),
            "description": self.description.clone(),
            "version": self.version.clone(),
            "group_slug": self.group_slug.as_ref().map(|s| JsonValue::from(s.as_str())).unwrap_or(JsonValue::Null),
            "server_scope": self.server_scope.as_ref().map(|s| JsonValue::from(s.as_str())).unwrap_or(JsonValue::Null),
            "auto_generated": self.auto_generated,
            "files": JsonValue::Array(files_arr),
            "file_count": self.files.len() as i64,
            "total_bytes": self.total_bytes() as i64,
            "manifest_sha256": self.manifest_sha256.clone(),
            "updated_at": self.updated_at,
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let slug = v.get("slug")?.as_str()?.to_string();
        let name = v
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let description = v
            .get("description")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let version = v
            .get("version")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let group_slug = v
            .get("group_slug")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        let server_scope = v
            .get("server_scope")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        let auto_generated = v
            .get("auto_generated")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        let files: Vec<SkillFileEntry> = v
            .get("files")
            .and_then(|a| a.as_array())
            .map(|arr| arr.iter().filter_map(SkillFileEntry::from_json).collect())
            .unwrap_or_default();
        let manifest_sha256 = v
            .get("manifest_sha256")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| Self::compute_manifest_sha256(&files));
        let updated_at = v
            .get("updated_at")
            .and_then(|x| x.as_f64())
            .unwrap_or_else(now_epoch_secs);
        Some(Self {
            slug,
            name,
            description,
            version,
            group_slug,
            server_scope,
            auto_generated,
            files,
            manifest_sha256,
            updated_at,
        })
    }
}

/// Lifecycle state of an MCP Task (`io.modelcontextprotocol/tasks`, GA).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpTaskStatus {
    Working,
    InputRequired,
    Completed,
    Failed,
    Cancelled,
}

impl McpTaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            McpTaskStatus::Working => "working",
            McpTaskStatus::InputRequired => "input_required",
            McpTaskStatus::Completed => "completed",
            McpTaskStatus::Failed => "failed",
            McpTaskStatus::Cancelled => "cancelled",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "input_required" | "suspended" => McpTaskStatus::InputRequired,
            "completed" | "done" | "ok" => McpTaskStatus::Completed,
            "failed" | "error" => McpTaskStatus::Failed,
            "cancelled" | "canceled" => McpTaskStatus::Cancelled,
            _ => McpTaskStatus::Working,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            McpTaskStatus::Completed | McpTaskStatus::Failed | McpTaskStatus::Cancelled
        )
    }
}

/// Durable, multi-replica MCP Task record persisted in `omni_mcp_tasks`.
#[derive(Clone, Debug, PartialEq)]
pub struct McpTaskRecord {
    pub task_id: String,
    pub owner_pod_id: String,
    pub caller_actor: String,
    pub group_slug: Option<String>,
    pub server_scope: Option<String>,
    pub status: McpTaskStatus,
    pub status_message: Option<String>,
    pub progress_pct: u8,
    pub poll_interval_ms: u64,
    pub input_requests: Option<JsonValue>,
    pub input_responses: Option<JsonValue>,
    pub result_payload: Option<JsonValue>,
    pub error_payload: Option<JsonValue>,
    pub cancel_requested: bool,
    pub created_at: f64,
    pub updated_at: f64,
    pub expires_at: f64,
}

impl McpTaskRecord {
    pub fn new(
        task_id: impl Into<String>,
        owner_pod_id: impl Into<String>,
        caller_actor: impl Into<String>,
        group_slug: Option<String>,
        server_scope: Option<String>,
        ttl_sec: u64,
    ) -> Self {
        let now = now_epoch_secs();
        Self {
            task_id: task_id.into(),
            owner_pod_id: owner_pod_id.into(),
            caller_actor: caller_actor.into(),
            group_slug,
            server_scope,
            status: McpTaskStatus::Working,
            status_message: None,
            progress_pct: 0,
            poll_interval_ms: 500,
            input_requests: None,
            input_responses: None,
            result_payload: None,
            error_payload: None,
            cancel_requested: false,
            created_at: now,
            updated_at: now,
            expires_at: now + ttl_sec as f64,
        }
    }

    pub fn to_json(&self) -> JsonValue {
        json!({
            "task_id": self.task_id.clone(),
            "owner_pod_id": self.owner_pod_id.clone(),
            "caller_actor": self.caller_actor.clone(),
            "group_slug": self.group_slug.as_ref().map(|s| JsonValue::from(s.as_str())).unwrap_or(JsonValue::Null),
            "server_scope": self.server_scope.as_ref().map(|s| JsonValue::from(s.as_str())).unwrap_or(JsonValue::Null),
            "status": self.status.as_str(),
            "status_message": self.status_message.as_ref().map(|s| JsonValue::from(s.as_str())).unwrap_or(JsonValue::Null),
            "progress_pct": self.progress_pct as i64,
            "poll_interval_ms": self.poll_interval_ms as i64,
            "input_requests": self.input_requests.clone().unwrap_or(JsonValue::Null),
            "input_responses": self.input_responses.clone().unwrap_or(JsonValue::Null),
            "result_payload": self.result_payload.clone().unwrap_or(JsonValue::Null),
            "error_payload": self.error_payload.clone().unwrap_or(JsonValue::Null),
            "cancel_requested": self.cancel_requested,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "expires_at": self.expires_at,
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let task_id = v
            .get("task_id")
            .or_else(|| v.get("taskId"))?
            .as_str()?
            .to_string();
        let owner_pod_id = v
            .get("owner_pod_id")
            .and_then(|x| x.as_str())
            .unwrap_or("omni-pod-local")
            .to_string();
        let caller_actor = v
            .get("caller_actor")
            .and_then(|x| x.as_str())
            .unwrap_or("agent")
            .to_string();
        let group_slug = v
            .get("group_slug")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        let server_scope = v
            .get("server_scope")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        let status = v
            .get("status")
            .and_then(|x| x.as_str())
            .map(McpTaskStatus::from_str)
            .unwrap_or(McpTaskStatus::Working);
        let status_message = v
            .get("status_message")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        let progress_pct = v
            .get("progress_pct")
            .and_then(|x| x.as_u64())
            .unwrap_or(0)
            .min(100) as u8;
        let poll_interval_ms = v
            .get("poll_interval_ms")
            .and_then(|x| x.as_u64())
            .unwrap_or(500);
        let input_requests =
            v.get("input_requests")
                .and_then(|x| if x.is_null() { None } else { Some(x.clone()) });
        let input_responses =
            v.get("input_responses")
                .and_then(|x| if x.is_null() { None } else { Some(x.clone()) });
        let result_payload =
            v.get("result_payload")
                .and_then(|x| if x.is_null() { None } else { Some(x.clone()) });
        let error_payload =
            v.get("error_payload")
                .and_then(|x| if x.is_null() { None } else { Some(x.clone()) });
        let cancel_requested = v
            .get("cancel_requested")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        let created_at = v
            .get("created_at")
            .and_then(|x| x.as_f64())
            .unwrap_or_else(now_epoch_secs);
        let updated_at = v
            .get("updated_at")
            .and_then(|x| x.as_f64())
            .unwrap_or(created_at);
        let expires_at = v
            .get("expires_at")
            .and_then(|x| x.as_f64())
            .unwrap_or(created_at + 3600.0);

        Some(Self {
            task_id,
            owner_pod_id,
            caller_actor,
            group_slug,
            server_scope,
            status,
            status_message,
            progress_pct,
            poll_interval_ms,
            input_requests,
            input_responses,
            result_payload,
            error_payload,
            cancel_requested,
            created_at,
            updated_at,
            expires_at,
        })
    }

    /// Formats the record as a GA `io.modelcontextprotocol/tasks` wire response for `tasks/get`.
    pub fn to_mcp_wire_json(&self) -> JsonValue {
        let mut obj = json!({
            "taskId": self.task_id.clone(),
            "status": self.status.as_str(),
            "progressPct": self.progress_pct as i64,
            "pollInterval": self.poll_interval_ms as i64,
            "createdAtEpochMs": (self.created_at * 1000.0) as i64,
            "updatedAtEpochMs": (self.updated_at * 1000.0) as i64,
            "expiresAtEpochMs": (self.expires_at * 1000.0) as i64,
            "resultType": "complete"
        });
        if let Some(ref msg) = self.status_message {
            obj.insert("statusMessage", JsonValue::from(msg.as_str()));
        }
        if let Some(ref g) = self.group_slug {
            obj.insert("groupSlug", JsonValue::from(g.as_str()));
        }
        if let Some(ref s) = self.server_scope {
            obj.insert("serverScope", JsonValue::from(s.as_str()));
        }
        if self.status == McpTaskStatus::InputRequired {
            if let Some(ref reqs) = self.input_requests {
                obj.insert("inputRequests", reqs.clone());
            }
        }
        if self.status == McpTaskStatus::Completed {
            if let Some(ref res) = self.result_payload {
                obj.insert("result", res.clone());
            }
        }
        if self.status == McpTaskStatus::Failed {
            if let Some(ref err) = self.error_payload {
                obj.insert("error", err.clone());
            }
        }
        obj
    }
}

/// Persisted multi-replica OAuth2 PKCE pending flow (`omni_oauth_flows`).
#[derive(Clone, Debug, PartialEq)]
pub struct OAuthFlowRecord {
    pub state: String,
    pub server_name: String,
    pub elicitation_id: Option<String>,
    pub code_verifier: String,
    pub token_url: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub created_at: f64,
    pub expires_at: f64,
}

impl OAuthFlowRecord {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "state": self.state.clone(),
            "server_name": self.server_name.clone(),
            "elicitation_id": self.elicitation_id.as_ref().map(|s| JsonValue::from(s.as_str())).unwrap_or(JsonValue::Null),
            "code_verifier": self.code_verifier.clone(),
            "token_url": self.token_url.clone(),
            "client_id": self.client_id.clone(),
            "redirect_uri": self.redirect_uri.clone(),
            "created_at": self.created_at,
            "expires_at": self.expires_at,
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let state = v.get("state")?.as_str()?.to_string();
        let server_name = v.get("server_name")?.as_str()?.to_string();
        let elicitation_id = v
            .get("elicitation_id")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        let code_verifier = v.get("code_verifier")?.as_str()?.to_string();
        let token_url = v.get("token_url")?.as_str()?.to_string();
        let client_id = v.get("client_id")?.as_str()?.to_string();
        let redirect_uri = v.get("redirect_uri")?.as_str()?.to_string();
        let created_at = v
            .get("created_at")
            .and_then(|x| x.as_f64())
            .unwrap_or_else(now_epoch_secs);
        let expires_at = v
            .get("expires_at")
            .and_then(|x| x.as_f64())
            .unwrap_or(created_at + 600.0);
        Some(Self {
            state,
            server_name,
            elicitation_id,
            code_verifier,
            token_url,
            client_id,
            redirect_uri,
            created_at,
            expires_at,
        })
    }
}

/// Persisted multi-replica sealed AEAD OAuth2 token envelope (`omni_oauth_tokens`).
#[derive(Clone, Debug, PartialEq)]
pub struct OAuthTokenRecord {
    pub server_name: String,
    /// Hex-encoded sealed ChaCha20-Poly1305 AEAD envelope (or JSON plaintext fallback when crypto disabled).
    pub sealed_envelope_hex: String,
    pub updated_at: f64,
}

impl OAuthTokenRecord {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "server_name": self.server_name.clone(),
            "sealed_envelope_hex": self.sealed_envelope_hex.clone(),
            "updated_at": self.updated_at,
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let server_name = v.get("server_name")?.as_str()?.to_string();
        let sealed_envelope_hex = v.get("sealed_envelope_hex")?.as_str()?.to_string();
        let updated_at = v
            .get("updated_at")
            .and_then(|x| x.as_f64())
            .unwrap_or_else(now_epoch_secs);
        Some(Self {
            server_name,
            sealed_envelope_hex,
            updated_at,
        })
    }
}

/// Metadata record for an uploaded binary/text file blob (`io.modelcontextprotocol/files`, `omni_file_blobs`).
#[derive(Clone, Debug, PartialEq)]
pub struct FileBlobRecord {
    pub file_id: String,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub backend: String,
    pub storage_key: String,
    pub caller_actor: String,
    pub created_at: f64,
    pub expires_at: f64,
}

impl FileBlobRecord {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "file_id": self.file_id.clone(),
            "filename": self.filename.clone(),
            "mime_type": self.mime_type.clone(),
            "size_bytes": self.size_bytes as i64,
            "sha256": self.sha256.clone(),
            "backend": self.backend.clone(),
            "storage_key": self.storage_key.clone(),
            "caller_actor": self.caller_actor.clone(),
            "created_at": self.created_at,
            "expires_at": self.expires_at,
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let file_id = v
            .get("file_id")
            .or_else(|| v.get("fileId"))?
            .as_str()?
            .to_string();
        let filename = v
            .get("filename")
            .and_then(|x| x.as_str())
            .unwrap_or("upload.bin")
            .to_string();
        let mime_type = v
            .get("mime_type")
            .or_else(|| v.get("mimeType"))
            .and_then(|x| x.as_str())
            .unwrap_or("application/octet-stream")
            .to_string();
        let size_bytes = v
            .get("size_bytes")
            .or_else(|| v.get("sizeBytes"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let sha256 = v
            .get("sha256")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let backend = v
            .get("backend")
            .and_then(|x| x.as_str())
            .unwrap_or("local")
            .to_string();
        let storage_key = v
            .get("storage_key")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let caller_actor = v
            .get("caller_actor")
            .and_then(|x| x.as_str())
            .unwrap_or("agent")
            .to_string();
        let created_at = v
            .get("created_at")
            .and_then(|x| x.as_f64())
            .unwrap_or_else(now_epoch_secs);
        let expires_at = v
            .get("expires_at")
            .and_then(|x| x.as_f64())
            .unwrap_or(created_at + 900.0);
        Some(Self {
            file_id,
            filename,
            mime_type,
            size_bytes,
            sha256,
            backend,
            storage_key,
            caller_actor,
            created_at,
            expires_at,
        })
    }
}
