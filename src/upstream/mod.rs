//! Upstream Client Engine & Session Supervisor for External MCP Servers.
//! Supports local subprocesses (`stdio`) and remote endpoints (`Streamable HTTP`).

pub mod http;
pub mod secrets;
pub mod stdio;

use crate::config::OmniConfig;
use crate::data_plane::security::PIIRedactor;
use crate::json::JsonValue;
use crate::logging::now_epoch_secs;
use crate::models::{
    generate_ulid, PendingUpstreamCallback, SamplingElicitationPolicy, ServerRecord, ToolRecord,
    ToolStatus, UpstreamTransportConfig, UNAUTHORIZED,
};
pub use http::{
    clear_mock_http, register_mock_http, HttpUpstreamSession, OAuthTokenRecord,
    UpstreamOAuthTokenStore,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use stdio::{check_stdio_command_allowlist, StdioUpstreamSession};

pub type ProgressSink = Arc<dyn Fn(&str, &JsonValue) + Send + Sync>;
pub type CallbackEventSink = Arc<dyn Fn(&str, &PendingUpstreamCallback) + Send + Sync>;
pub type PolicyResolver = Arc<dyn Fn(&str) -> SamplingElicitationPolicy + Send + Sync>;

struct PendingCallbackEntry {
    meta: PendingUpstreamCallback,
    tx: Sender<JsonValue>,
}

/// Bidirectional MCP coordinator for server-initiated reverse requests (`sampling/createMessage`,
/// `elicitation/create`, `roots/list`) and streaming `notifications/progress` events (v6.5.0).
pub struct UpstreamCallbackHub {
    pending: RwLock<HashMap<String, PendingCallbackEntry>>,
    progress_sink: RwLock<Option<ProgressSink>>,
    callback_event_sink: RwLock<Option<CallbackEventSink>>,
    policy_resolver: RwLock<Option<PolicyResolver>>,
    max_sampling_tokens: u64,
}

impl Default for UpstreamCallbackHub {
    fn default() -> Self {
        Self::new()
    }
}

impl UpstreamCallbackHub {
    pub fn new() -> Self {
        Self {
            pending: RwLock::new(HashMap::new()),
            progress_sink: RwLock::new(None),
            callback_event_sink: RwLock::new(None),
            policy_resolver: RwLock::new(None),
            max_sampling_tokens: 4096,
        }
    }

    pub fn set_progress_sink(&self, sink: ProgressSink) {
        *self.progress_sink.write().unwrap() = Some(sink);
    }

    pub fn set_callback_event_sink(&self, sink: CallbackEventSink) {
        *self.callback_event_sink.write().unwrap() = Some(sink);
    }

    pub fn set_policy_resolver(&self, resolver: PolicyResolver) {
        *self.policy_resolver.write().unwrap() = Some(resolver);
    }

    pub fn emit_progress(&self, server_name: &str, notification: &JsonValue) {
        if let Some(ref sink) = *self.progress_sink.read().unwrap() {
            sink(server_name, notification);
        }
    }

    pub fn list_pending_callbacks(&self) -> Vec<PendingUpstreamCallback> {
        let guard = self.pending.read().unwrap();
        guard.values().map(|e| e.meta.clone()).collect()
    }

    /// Resolves a pending `RelayToClient` or `ApprovalGate` callback, waking the waiting
    /// upstream session thread to send the JSON-RPC reply back to the upstream server.
    pub fn resolve_callback(
        &self,
        callback_id: &str,
        approved: bool,
        result_payload: Option<JsonValue>,
        error_message: Option<&str>,
    ) -> Result<PendingUpstreamCallback, String> {
        let entry = self
            .pending
            .write()
            .unwrap()
            .remove(callback_id)
            .ok_or_else(|| format!("Pending upstream callback '{}' not found", callback_id))?;

        let rpc_reply = if approved {
            json!({
                "jsonrpc": "2.0",
                "id": entry.meta.upstream_request_id.clone(),
                "result": result_payload.unwrap_or_else(JsonValue::object),
            })
        } else {
            json!({
                "jsonrpc": "2.0",
                "id": entry.meta.upstream_request_id.clone(),
                "error": {
                    "code": UNAUTHORIZED,
                    "message": error_message.unwrap_or("Upstream request rejected by operator/client"),
                }
            })
        };

        let _ = entry.tx.send(rpc_reply);
        let mut resolved = entry.meta;
        resolved.status = if approved {
            "resolved".to_string()
        } else {
            "rejected".to_string()
        };
        Ok(resolved)
    }

    /// Evaluates policy and handles an upstream server-initiated JSON-RPC request
    /// (`sampling/createMessage`, `elicitation/create`, `roots/list`).
    pub fn handle_reverse_request(
        &self,
        server_name: &str,
        upstream_req_id: JsonValue,
        method: &str,
        params: JsonValue,
        default_policy: SamplingElicitationPolicy,
        timeout_ms: u64,
    ) -> JsonValue {
        let policy = self
            .policy_resolver
            .read()
            .unwrap()
            .as_ref()
            .map(|r| r(server_name))
            .unwrap_or(default_policy);

        // Enforce maxTokens cap for sampling requests
        if method == "sampling/createMessage" {
            if let Some(max_toks) = params.get("maxTokens").and_then(|v| v.as_u64()) {
                if max_toks > self.max_sampling_tokens {
                    return json!({
                        "jsonrpc": "2.0",
                        "id": upstream_req_id,
                        "error": {
                            "code": UNAUTHORIZED,
                            "message": format!(
                                "Sampling maxTokens {} exceeds gateway cap {}",
                                max_toks, self.max_sampling_tokens
                            )
                        }
                    });
                }
            }
        }

        match policy {
            SamplingElicitationPolicy::Deny => json!({
                "jsonrpc": "2.0",
                "id": upstream_req_id,
                "error": {
                    "code": UNAUTHORIZED,
                    "message": format!(
                        "Sampling/Elicitation '{}' denied by gateway policy on server '{}'",
                        method, server_name
                    )
                }
            }),
            SamplingElicitationPolicy::RelayToClient | SamplingElicitationPolicy::ApprovalGate => {
                let redacted_params = PIIRedactor::redact_payload(&params);
                let callback_id = format!("cb_{}", generate_ulid());
                let (mode_str, status_str, event_kind) = match policy {
                    SamplingElicitationPolicy::RelayToClient => (
                        "RelayToClient".to_string(),
                        "input_required".to_string(),
                        "mcp.upstream_request",
                    ),
                    SamplingElicitationPolicy::ApprovalGate => (
                        "ApprovalGate".to_string(),
                        "pending_approval".to_string(),
                        "mcp.approval_required",
                    ),
                    SamplingElicitationPolicy::Deny => unreachable!(),
                };

                let meta = PendingUpstreamCallback {
                    callback_id: callback_id.clone(),
                    server_name: server_name.to_string(),
                    upstream_request_id: upstream_req_id.clone(),
                    method: method.to_string(),
                    params: redacted_params,
                    mode: mode_str,
                    status: status_str,
                    created_at: now_epoch_secs(),
                };

                let (tx, rx) = mpsc::channel();
                self.pending.write().unwrap().insert(
                    callback_id.clone(),
                    PendingCallbackEntry {
                        meta: meta.clone(),
                        tx,
                    },
                );

                if let Some(ref sink) = *self.callback_event_sink.read().unwrap() {
                    sink(event_kind, &meta);
                }

                let wait_dur = Duration::from_millis(timeout_ms.max(100));
                match rx.recv_timeout(wait_dur) {
                    Ok(reply) => reply,
                    Err(RecvTimeoutError::Timeout) => {
                        self.pending.write().unwrap().remove(&callback_id);
                        json!({
                            "jsonrpc": "2.0",
                            "id": upstream_req_id,
                            "error": {
                                "code": crate::models::TIMEOUT,
                                "message": format!(
                                    "Upstream callback '{}' ({}) timed out awaiting response",
                                    callback_id, method
                                )
                            }
                        })
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        self.pending.write().unwrap().remove(&callback_id);
                        json!({
                            "jsonrpc": "2.0",
                            "id": upstream_req_id,
                            "error": {
                                "code": UNAUTHORIZED,
                                "message": "Upstream callback channel closed"
                            }
                        })
                    }
                }
            }
        }
    }
}

pub struct UpstreamManager {
    pub config: Arc<OmniConfig>,
    pub callback_hub: Arc<UpstreamCallbackHub>,
    stdio_sessions: RwLock<HashMap<String, Arc<StdioUpstreamSession>>>,
    http_sessions: RwLock<HashMap<String, Arc<HttpUpstreamSession>>>,
    rediscovery_in_flight: Mutex<HashSet<String>>,
}

impl UpstreamManager {
    pub fn new(config: Arc<OmniConfig>) -> Self {
        Self {
            config,
            callback_hub: Arc::new(UpstreamCallbackHub::new()),
            stdio_sessions: RwLock::new(HashMap::new()),
            http_sessions: RwLock::new(HashMap::new()),
            rediscovery_in_flight: Mutex::new(HashSet::new()),
        }
    }

    /// Propagates downstream `notifications/cancelled` (`requestId`) to an upstream session
    /// and immediately aborts any in-flight waiting channel.
    pub fn cancel_upstream_request(
        &self,
        server_name: &str,
        request_id: u64,
        reason: Option<&str>,
    ) -> bool {
        if let Some(stdio_sess) = self
            .stdio_sessions
            .read()
            .unwrap()
            .get(server_name)
            .cloned()
        {
            return stdio_sess.cancel_request(request_id, reason);
        }
        if let Some(http_sess) = self.http_sessions.read().unwrap().get(server_name).cloned() {
            return http_sess.cancel_request(request_id, reason).is_ok();
        }
        false
    }

    pub fn try_lock_rediscovery(&self, server_name: &str) -> bool {
        let mut guard = self.rediscovery_in_flight.lock().unwrap();
        guard.insert(server_name.to_string())
    }

    pub fn unlock_rediscovery(&self, server_name: &str) {
        let mut guard = self.rediscovery_in_flight.lock().unwrap();
        guard.remove(server_name);
    }

    pub fn get_or_create_stdio_session(
        &self,
        server: &ServerRecord,
    ) -> Result<Arc<StdioUpstreamSession>, String> {
        let transport = server
            .upstream_config
            .as_ref()
            .ok_or_else(|| format!("Server '{}' has no upstream configuration", server.name))?;

        match transport {
            UpstreamTransportConfig::Stdio {
                command,
                canonical_command: _,
                args,
                env_refs,
                cwd,
            } => {
                {
                    let r_guard = self.stdio_sessions.read().unwrap();
                    if let Some(session) = r_guard.get(&server.name) {
                        return Ok(session.clone());
                    }
                }

                // Verify command against allowlist
                let canonical_cmd =
                    check_stdio_command_allowlist(command, &self.config.stdio_allowlist)?;

                let cwd_path = cwd.as_ref().map(PathBuf::from);
                let session = Arc::new(
                    StdioUpstreamSession::new(
                        &server.name,
                        canonical_cmd,
                        args.clone(),
                        env_refs.clone(),
                        cwd_path,
                        self.config.secrets_dir.clone(),
                        self.config.upstream_call_timeout_ms,
                    )
                    .with_bidirectional_hub(
                        self.callback_hub.clone(),
                        server.sampling_elicitation_policy.clone(),
                    ),
                );

                let mut w_guard = self.stdio_sessions.write().unwrap();
                w_guard.insert(server.name.clone(), session.clone());
                Ok(session)
            }
            _ => Err(format!(
                "Server '{}' is not configured with stdio transport",
                server.name
            )),
        }
    }

    pub fn get_or_create_http_session(
        &self,
        server: &ServerRecord,
    ) -> Result<Arc<HttpUpstreamSession>, String> {
        let transport = server
            .upstream_config
            .as_ref()
            .ok_or_else(|| format!("Server '{}' has no upstream configuration", server.name))?;

        match transport {
            UpstreamTransportConfig::StreamableHttp {
                url,
                bearer_ref,
                header_refs,
                session_mode,
            } => {
                {
                    let r_guard = self.http_sessions.read().unwrap();
                    if let Some(session) = r_guard.get(&server.name) {
                        return Ok(session.clone());
                    }
                }

                let session = Arc::new(
                    HttpUpstreamSession::new(
                        &server.name,
                        url.clone(),
                        bearer_ref.clone(),
                        header_refs.clone(),
                        *session_mode,
                        self.config.secrets_dir.clone(),
                        self.config.upstream_call_timeout_ms,
                        self.config.allow_loopback_upstream,
                    )
                    .with_bidirectional_hub(
                        self.callback_hub.clone(),
                        server.sampling_elicitation_policy.clone(),
                    ),
                );

                let mut w_guard = self.http_sessions.write().unwrap();
                w_guard.insert(server.name.clone(), session.clone());
                Ok(session)
            }
            _ => Err(format!(
                "Server '{}' is not configured with streamable HTTP transport",
                server.name
            )),
        }
    }

    pub fn fetch_tools(&self, server: &ServerRecord) -> Result<Vec<ToolRecord>, String> {
        let transport = server
            .upstream_config
            .as_ref()
            .ok_or_else(|| format!("Server '{}' has no upstream transport config", server.name))?;

        match transport {
            UpstreamTransportConfig::Stdio { .. } => {
                let session = self.get_or_create_stdio_session(server)?;
                session.fetch_tools()
            }
            UpstreamTransportConfig::StreamableHttp { .. } => {
                let session = self.get_or_create_http_session(server)?;
                session.fetch_tools()
            }
        }
    }

    pub fn call_tool(
        &self,
        server: &ServerRecord,
        tool_name: &str,
        arguments: &JsonValue,
    ) -> Result<JsonValue, String> {
        let transport = server
            .upstream_config
            .as_ref()
            .ok_or_else(|| format!("Server '{}' has no upstream transport config", server.name))?;
        let timeout_ms = server.tools.get(tool_name).and_then(|t| t.timeout_ms);

        match transport {
            UpstreamTransportConfig::Stdio { .. } => {
                let session = self.get_or_create_stdio_session(server)?;
                session.call_tool_with_timeout(tool_name, arguments, timeout_ms)
            }
            UpstreamTransportConfig::StreamableHttp { .. } => {
                let session = self.get_or_create_http_session(server)?;
                session.call_tool(tool_name, arguments)
            }
        }
    }

    pub fn call_rpc_for_server(
        &self,
        server: &ServerRecord,
        method: &str,
        params: JsonValue,
    ) -> Result<JsonValue, String> {
        let transport = server
            .upstream_config
            .as_ref()
            .ok_or_else(|| format!("Server '{}' has no upstream transport config", server.name))?;

        match transport {
            UpstreamTransportConfig::Stdio { .. } => {
                let session = self.get_or_create_stdio_session(server)?;
                session.call_rpc(method, params)
            }
            UpstreamTransportConfig::StreamableHttp { .. } => {
                let session = self.get_or_create_http_session(server)?;
                session.call_rpc(method, params)
            }
        }
    }

    pub fn fetch_resources(
        &self,
        server: &ServerRecord,
    ) -> Result<Vec<crate::models::ExternalResourceRecord>, String> {
        let transport = server
            .upstream_config
            .as_ref()
            .ok_or_else(|| format!("Server '{}' has no upstream transport config", server.name))?;

        match transport {
            UpstreamTransportConfig::Stdio { .. } => {
                let session = self.get_or_create_stdio_session(server)?;
                session.fetch_resources()
            }
            UpstreamTransportConfig::StreamableHttp { .. } => {
                let session = self.get_or_create_http_session(server)?;
                session.fetch_resources()
            }
        }
    }

    pub fn fetch_prompts(
        &self,
        server: &ServerRecord,
    ) -> Result<Vec<crate::models::ExternalPromptRecord>, String> {
        let transport = server
            .upstream_config
            .as_ref()
            .ok_or_else(|| format!("Server '{}' has no upstream transport config", server.name))?;

        match transport {
            UpstreamTransportConfig::Stdio { .. } => {
                let session = self.get_or_create_stdio_session(server)?;
                session.fetch_prompts()
            }
            UpstreamTransportConfig::StreamableHttp { .. } => {
                let session = self.get_or_create_http_session(server)?;
                session.fetch_prompts()
            }
        }
    }

    pub fn deregister_server(&self, server_name: &str) {
        if let Some(stdio_session) = self.stdio_sessions.write().unwrap().remove(server_name) {
            stdio_session.shutdown();
        }
        let _ = self.http_sessions.write().unwrap().remove(server_name);
    }

    pub fn inspect(&self, server: &ServerRecord) -> Result<JsonValue, String> {
        let mut total_tools = 0;
        let mut enabled_tools = 0;
        let mut disabled_tools = 0;
        let mut unavailable_tools = 0;

        for t in server.tools.values() {
            total_tools += 1;
            match t.status {
                ToolStatus::Enabled => enabled_tools += 1,
                ToolStatus::Disabled | ToolStatus::PendingReview => disabled_tools += 1,
                ToolStatus::Unavailable => unavailable_tools += 1,
            }
        }

        let mut health = "Idle";
        let mut last_init = None;
        let mut session_mode_str = None;
        let mut stderr_tail = Vec::new();
        let mut transport_summary = JsonValue::object();

        if let Some(ref config) = server.upstream_config {
            match config {
                UpstreamTransportConfig::Stdio {
                    command,
                    canonical_command,
                    args,
                    env_refs,
                    cwd,
                } => {
                    transport_summary.insert("transport", "stdio");
                    transport_summary.insert("command", command.clone());
                    transport_summary.insert("canonical_command", canonical_command.clone());
                    transport_summary.insert(
                        "args",
                        JsonValue::Array(args.iter().map(|a| JsonValue::from(a.clone())).collect()),
                    );
                    transport_summary.insert(
                        "env_refs",
                        JsonValue::Array(
                            env_refs
                                .keys()
                                .map(|k| JsonValue::from(k.clone()))
                                .collect(),
                        ),
                    );
                    if let Some(ref c) = cwd {
                        transport_summary.insert("cwd", c.clone());
                    }

                    if let Some(session) = self.stdio_sessions.read().unwrap().get(&server.name) {
                        health = session.get_health();
                        last_init = session.get_last_initialize();
                        stderr_tail = session.get_stderr_tail();
                    }
                }
                UpstreamTransportConfig::StreamableHttp {
                    url,
                    bearer_ref,
                    header_refs,
                    session_mode,
                } => {
                    transport_summary.insert("transport", "streamable_http");
                    transport_summary.insert("url", url.clone());
                    transport_summary.insert("has_bearer", bearer_ref.is_some());
                    transport_summary.insert(
                        "header_refs",
                        JsonValue::Array(
                            header_refs
                                .keys()
                                .map(|k| JsonValue::from(k.clone()))
                                .collect(),
                        ),
                    );
                    let mode_str = match session_mode {
                        crate::models::SessionMode::Stateful => "stateful",
                        crate::models::SessionMode::Stateless => "stateless",
                    };
                    transport_summary.insert("session_mode", mode_str);
                    session_mode_str = Some(mode_str.to_string());

                    if let Some(session) = self.http_sessions.read().unwrap().get(&server.name) {
                        health = session.get_health();
                        last_init = session.get_last_initialize();
                    }
                }
            }
        }

        Ok(json!({
            "server_name": server.name.clone(),
            "health": health,
            "last_initialize": last_init,
            "session_mode": session_mode_str,
            "total_tools": total_tools,
            "enabled_tools": enabled_tools,
            "disabled_tools": disabled_tools,
            "unavailable_tools": unavailable_tools,
            "transport_summary": transport_summary,
            "stderr_tail": JsonValue::Array(stderr_tail.into_iter().map(JsonValue::from).collect())
        }))
    }

    pub fn shutdown_all(&self) {
        let mut stdio_guard = self.stdio_sessions.write().unwrap();
        for session in stdio_guard.values() {
            session.shutdown();
        }
        stdio_guard.clear();
        self.http_sessions.write().unwrap().clear();
    }
}

impl Drop for UpstreamManager {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}
