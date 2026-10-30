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

/// Gathers OS/system entropy from system time, thread ID, process ID, monotonic clock, and ASLR pointers.
/// Never uses the Deterministic Simulation Testing (DST) Xoshiro256StarStar chaos generator.
fn gather_entropy(dest: &mut [u8]) {
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

    let hash = crate::storage::audit_log::sha256(&mixed);
    let len = dest.len().min(32);
    dest[..len].copy_from_slice(&hash[..len]);
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
}

impl ProtocolType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProtocolType::Http => "HTTP",
            ProtocolType::Grpc => "gRPC",
            ProtocolType::TChannel => "TChannel",
            ProtocolType::NativeMcp => "Native_MCP",
            ProtocolType::ThirdParty => "ThirdParty",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "gRPC" | "grpc" | "GRPC" => ProtocolType::Grpc,
            "TChannel" | "tchannel" | "TCHANNEL" => ProtocolType::TChannel,
            "Native_MCP" | "native_mcp" | "NATIVE_MCP" => ProtocolType::NativeMcp,
            "ThirdParty" | "third_party" | "THIRDPARTY" => ProtocolType::ThirdParty,
            _ => ProtocolType::Http,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Disabled,
    PendingReview,
    Enabled,
}

impl ToolStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ToolStatus::Disabled => "DISABLED",
            ToolStatus::PendingReview => "PENDING_REVIEW",
            ToolStatus::Enabled => "ENABLED",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s.to_uppercase().as_str() {
            "ENABLED" => ToolStatus::Enabled,
            "PENDING_REVIEW" => ToolStatus::PendingReview,
            _ => ToolStatus::Disabled,
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
        match s.to_lowercase().as_str() {
            "human" => Some(ActorType::Human),
            "service" => Some(ActorType::Service),
            "agent" => Some(ActorType::Agent),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CharterPolicy {
    pub allowed_actors: Vec<ActorType>,
    pub allowed_tools: Vec<String>,
    pub rate_limit_rpm: u32,
    pub require_redaction: bool,
    pub custom_attributes: JsonValue,
}

impl Default for CharterPolicy {
    fn default() -> Self {
        Self {
            allowed_actors: vec![ActorType::Human, ActorType::Service, ActorType::Agent],
            allowed_tools: vec!["*".to_string()],
            rate_limit_rpm: 1200,
            require_redaction: true,
            custom_attributes: JsonValue::object(),
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
        json!({
            "allowed_actors": JsonValue::Array(actors),
            "allowed_tools": JsonValue::Array(tools),
            "rate_limit_rpm": self.rate_limit_rpm as i64,
            "require_redaction": self.require_redaction,
            "custom_attributes": self.custom_attributes.clone()
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

        Self {
            allowed_actors,
            allowed_tools,
            rate_limit_rpm,
            require_redaction,
            custom_attributes,
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
        let canonical = format!(
            "{}:{}:{}:{}:{}:{}:{}",
            self.execution_id,
            self.script_hash,
            self.instruction_ptr,
            self.pending_server,
            self.pending_tool,
            self.pending_arguments_hash,
            self.expires_at as u64,
        );
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
        if current_time > self.expires_at {
            return Err("Approval frame has expired");
        }
        if !self.frame_hmac.is_empty() {
            let expected_hmac = self.compute_hmac(secret);
            if self.frame_hmac != expected_hmac {
                return Err("Frame HMAC signature mismatch (tampering detected)");
            }
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
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
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
        })
    }
}
