//! Upstream Client Engine & Session Supervisor for External MCP Servers.
//! Supports local subprocesses (`stdio`) and remote endpoints (`Streamable HTTP`).

pub mod http;
pub mod secrets;
pub mod stdio;

use crate::config::OmniConfig;
use crate::json::JsonValue;
use crate::models::{ServerRecord, ToolRecord, ToolStatus, UpstreamTransportConfig};
pub use http::{clear_mock_http, register_mock_http, HttpUpstreamSession};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use stdio::{check_stdio_command_allowlist, StdioUpstreamSession};

pub struct UpstreamManager {
    pub config: Arc<OmniConfig>,
    stdio_sessions: RwLock<HashMap<String, Arc<StdioUpstreamSession>>>,
    http_sessions: RwLock<HashMap<String, Arc<HttpUpstreamSession>>>,
    rediscovery_in_flight: Mutex<HashSet<String>>,
}

impl UpstreamManager {
    pub fn new(config: Arc<OmniConfig>) -> Self {
        Self {
            config,
            stdio_sessions: RwLock::new(HashMap::new()),
            http_sessions: RwLock::new(HashMap::new()),
            rediscovery_in_flight: Mutex::new(HashSet::new()),
        }
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
                let session = Arc::new(StdioUpstreamSession::new(
                    &server.name,
                    canonical_cmd,
                    args.clone(),
                    env_refs.clone(),
                    cwd_path,
                    self.config.secrets_dir.clone(),
                    self.config.upstream_call_timeout_ms,
                ));

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

                let session = Arc::new(HttpUpstreamSession::new(
                    &server.name,
                    url.clone(),
                    bearer_ref.clone(),
                    header_refs.clone(),
                    *session_mode,
                    self.config.secrets_dir.clone(),
                    self.config.upstream_call_timeout_ms,
                    self.config.allow_loopback_upstream,
                ));

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

        match transport {
            UpstreamTransportConfig::Stdio { .. } => {
                let session = self.get_or_create_stdio_session(server)?;
                session.call_tool(tool_name, arguments)
            }
            UpstreamTransportConfig::StreamableHttp { .. } => {
                let session = self.get_or_create_http_session(server)?;
                session.call_tool(tool_name, arguments)
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
