//! Stdio Upstream Client & Process Supervision for Omni MCP.
//! Pure Rust std-only process lifecycle management, CRLF-tolerant JSON-RPC framing,
//! detached non-blocking stderr ring buffer drain, and safe signal termination.

use crate::json::JsonValue;
use crate::logging::now_epoch_secs;
use crate::models::{ProtocolType, SchemaSource, ToolRecord, ToolStatus};
use crate::upstream::secrets::resolve_secret_ref;
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub const MAX_STDERR_LINES: usize = 256;
pub const MAX_STDERR_LINE_BYTES: usize = 4096;
pub const MAX_PAGINATION_PAGES: usize = 50;

/// Checks whether a candidate command matches any canonical entry in the stdio allowlist.
/// On Windows, probes `.exe` and `.cmd` when no extension is specified.
/// Rejects `cmd.exe /C` wrapper injection.
pub fn check_stdio_command_allowlist(
    candidate: &str,
    allowlist: &[String],
) -> Result<PathBuf, String> {
    let lower = candidate.to_lowercase();
    let trimmed_lower = lower.trim();
    if trimmed_lower.ends_with("cmd.exe") || trimmed_lower == "cmd" {
        return Err("Command injection guard: direct cmd.exe invocation is forbidden".to_string());
    }

    let candidate_path = Path::new(candidate);
    let probe_paths: Vec<PathBuf> = if cfg!(windows) && candidate_path.extension().is_none() {
        vec![
            candidate_path.to_path_buf(),
            PathBuf::from(format!("{}.exe", candidate)),
            PathBuf::from(format!("{}.cmd", candidate)),
        ]
    } else {
        vec![candidate_path.to_path_buf()]
    };

    let mut canonical_opt = None;
    for p in probe_paths {
        if let Ok(canon) = fs::canonicalize(&p) {
            canonical_opt = Some(canon);
            break;
        }
    }

    let canonical_candidate = match canonical_opt {
        Some(c) => c,
        None => {
            return Err(format!(
                "Command '{}' not found or cannot be canonicalized",
                candidate
            ));
        }
    };

    if allowlist.is_empty() {
        return Err(format!(
            "Command '{}' rejected: stdio allowlist is empty",
            candidate
        ));
    }

    let mut allowed = false;
    for entry in allowlist {
        if let Ok(canon_entry) = fs::canonicalize(entry) {
            if canon_entry == canonical_candidate {
                allowed = true;
                break;
            }
        }
    }

    if !allowed {
        return Err(format!(
            "Command '{}' ({}) is not in stdio allowlist",
            candidate,
            canonical_candidate.display()
        ));
    }

    Ok(canonical_candidate)
}

struct StdioProcess {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    rx: Receiver<String>,
}

pub struct StdioUpstreamSession {
    pub server_name: String,
    pub canonical_command: PathBuf,
    pub args: Vec<String>,
    pub env_refs: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub secrets_dir: PathBuf,
    pub call_timeout_ms: u64,
    process: Mutex<Option<StdioProcess>>,
    stderr_buffer: Arc<Mutex<VecDeque<String>>>,
    last_initialize: Mutex<Option<f64>>,
    next_request_id: AtomicU64,
}

impl StdioUpstreamSession {
    pub fn new(
        server_name: impl Into<String>,
        canonical_command: PathBuf,
        args: Vec<String>,
        env_refs: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
        secrets_dir: PathBuf,
        call_timeout_ms: u64,
    ) -> Self {
        Self {
            server_name: server_name.into(),
            canonical_command,
            args,
            env_refs,
            cwd,
            secrets_dir,
            call_timeout_ms,
            process: Mutex::new(None),
            stderr_buffer: Arc::new(Mutex::new(VecDeque::with_capacity(MAX_STDERR_LINES))),
            last_initialize: Mutex::new(None),
            next_request_id: AtomicU64::new(1),
        }
    }

    pub fn get_health(&self) -> &'static str {
        let guard = self.process.lock().unwrap();
        match &*guard {
            None => "Idle",
            Some(_) => "Connected",
        }
    }

    pub fn is_alive(&self) -> bool {
        let mut guard = self.process.lock().unwrap();
        if let Some(ref mut proc) = *guard {
            matches!(proc.child.try_wait(), Ok(None))
        } else {
            false
        }
    }

    pub fn get_last_initialize(&self) -> Option<f64> {
        *self.last_initialize.lock().unwrap()
    }

    pub fn get_stderr_tail(&self) -> Vec<String> {
        self.stderr_buffer.lock().unwrap().iter().cloned().collect()
    }

    fn spawn_process(&self) -> Result<StdioProcess, String> {
        // Resolve environment variable secrets
        let mut resolved_envs = Vec::new();
        for (k, secret_ref) in &self.env_refs {
            let val = resolve_secret_ref(secret_ref, &self.secrets_dir)?;
            resolved_envs.push((k.clone(), val));
        }

        let mut cmd = Command::new(&self.canonical_command);
        cmd.args(&self.args);
        cmd.env_clear();

        // Pass resolved secrets
        for (k, v) in resolved_envs {
            cmd.env(k, v);
        }

        // Pass safe OS loader variables
        if cfg!(windows) {
            if let Ok(sys_root) =
                std::env::var("SystemRoot").or_else(|_| std::env::var("SYSTEMROOT"))
            {
                cmd.env("SystemRoot", sys_root);
            }
            if let Ok(sys_drive) = std::env::var("SystemDrive") {
                cmd.env("SystemDrive", sys_drive);
            }
            if let Ok(path) = std::env::var("PATH").or_else(|_| std::env::var("Path")) {
                cmd.env("PATH", path);
            }
            if let Ok(temp) = std::env::var("TEMP") {
                cmd.env("TEMP", temp);
            }
        } else {
            if let Ok(path) = std::env::var("PATH") {
                cmd.env("PATH", path);
            }
            if let Ok(home) = std::env::var("HOME") {
                cmd.env("HOME", home);
            }
        }

        if let Some(ref cwd) = self.cwd {
            cmd.current_dir(cwd);
        }

        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| {
            format!(
                "Failed to spawn child process {:?}: {}",
                self.canonical_command, e
            )
        })?;

        let stdin = Arc::new(Mutex::new(
            child
                .stdin
                .take()
                .ok_or_else(|| "Failed to capture child stdin".to_string())?,
        ));

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Failed to capture child stdout".to_string())?;

        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "Failed to capture child stderr".to_string())?;

        // Detached background thread: Stderr ring buffer drain (capped at 256 lines)
        let ring_buffer = self.stderr_buffer.clone();
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line_res in reader.lines() {
                match line_res {
                    Ok(mut line) => {
                        if line.len() > MAX_STDERR_LINE_BYTES {
                            line.truncate(MAX_STDERR_LINE_BYTES);
                        }
                        let mut buf = ring_buffer.lock().unwrap();
                        if buf.len() >= MAX_STDERR_LINES {
                            buf.pop_front();
                        }
                        buf.push_back(line);
                    }
                    Err(_) => break,
                }
            }
        });

        // Detached background thread: Stdout reader with server-to-client request rejection
        let (tx, rx) = mpsc::channel();
        let stdin_clone = stdin.clone();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line_res in reader.lines() {
                match line_res {
                    Ok(line) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }

                        if let Ok(json_val) = JsonValue::parse(trimmed) {
                            let has_method = json_val.get("method").is_some();
                            let req_id_opt = json_val.get("id");

                            if has_method {
                                if let Some(id_val) = req_id_opt {
                                    let rejection = json!({
                                        "jsonrpc": "2.0",
                                        "id": id_val.clone(),
                                        "error": {
                                            "code": -32601,
                                            "message": "Method not found"
                                        }
                                    });
                                    let mut rej_str = rejection.to_string_compact();
                                    rej_str.push('\n');
                                    if let Ok(mut sin) = stdin_clone.lock() {
                                        let _ = sin.write_all(rej_str.as_bytes());
                                        let _ = sin.flush();
                                    }
                                }
                                continue;
                            }

                            // RPC Response: forward to receiver channel
                            if tx.send(trimmed.to_string()).is_err() {
                                break;
                            }
                        } else {
                            // Non-JSON line: ignore
                            continue;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(StdioProcess { child, stdin, rx })
    }

    fn ensure_started(&self) -> Result<(), String> {
        let mut guard = self.process.lock().unwrap();
        let needs_spawn = match *guard {
            None => true,
            Some(ref mut proc) => match proc.child.try_wait() {
                Ok(Some(_)) => true,
                Err(_) => true,
                Ok(None) => false,
            },
        };

        if needs_spawn {
            let proc = self.spawn_process()?;
            *guard = Some(proc);

            // Execute MCP Handshake (initialize + notifications/initialized)
            let init_req = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "omni-mcp",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }
            });

            let proc_ref = guard.as_mut().unwrap();
            let mut wire = init_req.to_string_compact();
            wire.push('\n');
            {
                let mut sin = proc_ref.stdin.lock().unwrap();
                sin.write_all(wire.as_bytes())
                    .map_err(|e| format!("Failed to write initialize to child: {}", e))?;
                sin.flush()
                    .map_err(|e| format!("Failed to flush initialize: {}", e))?;
            }

            let timeout = Duration::from_millis(self.call_timeout_ms);
            let resp_str = proc_ref.rx.recv_timeout(timeout).map_err(|e| match e {
                RecvTimeoutError::Timeout => "Initialize handshake timed out".to_string(),
                RecvTimeoutError::Disconnected => {
                    "Child process exited prematurely during initialize".to_string()
                }
            })?;

            let parsed = JsonValue::parse(&resp_str)
                .map_err(|e| format!("Invalid JSON from child on initialize: {}", e))?;
            if let Some(err) = parsed.get("error") {
                return Err(format!("Initialize failed: {}", err.to_string_compact()));
            }

            // Send notifications/initialized
            let notify = json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            });
            let mut notify_wire = notify.to_string_compact();
            notify_wire.push('\n');
            {
                let mut sin = proc_ref.stdin.lock().unwrap();
                let _ = sin.write_all(notify_wire.as_bytes());
                let _ = sin.flush();
            }

            *self.last_initialize.lock().unwrap() = Some(now_epoch_secs());
        }

        Ok(())
    }

    pub fn call_rpc(&self, method: &str, params: JsonValue) -> Result<JsonValue, String> {
        self.ensure_started()?;

        let req_id = self.next_request_id.fetch_add(1, Ordering::SeqCst) + 2;
        let req = json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": method,
            "params": params
        });

        let mut wire = req.to_string_compact();
        wire.push('\n');

        let mut guard = self.process.lock().unwrap();
        let proc = guard
            .as_mut()
            .ok_or_else(|| "Child process is not running".to_string())?;

        {
            let mut sin = proc.stdin.lock().unwrap();
            sin.write_all(wire.as_bytes())
                .map_err(|e| format!("Failed to write request to child: {}", e))?;
            sin.flush()
                .map_err(|e| format!("Failed to flush request: {}", e))?;
        }

        let timeout = Duration::from_millis(self.call_timeout_ms);
        let resp_str = proc.rx.recv_timeout(timeout).map_err(|e| match e {
            RecvTimeoutError::Timeout => format!("Upstream call '{}' timed out", method),
            RecvTimeoutError::Disconnected => "Child process exited unexpectedly".to_string(),
        })?;

        let parsed = JsonValue::parse(&resp_str)
            .map_err(|e| format!("Invalid JSON response from child: {}", e))?;

        if let Some(err) = parsed.get("error") {
            let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-32000);
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("Unknown upstream error");
            return Err(format!("MCP error {}: {}", code, msg));
        }

        parsed
            .get("result")
            .cloned()
            .ok_or_else(|| "Missing result in JSON-RPC response".to_string())
    }

    pub fn fetch_tools(&self) -> Result<Vec<ToolRecord>, String> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0;

        loop {
            pages += 1;
            if pages > MAX_PAGINATION_PAGES {
                break;
            }

            let mut params = JsonValue::object();
            if let Some(ref cur) = cursor {
                params.insert("cursor", cur.clone());
            }

            let result = self.call_rpc("tools/list", params)?;
            if let Some(tool_list) = result.get("tools").and_then(|t| t.as_array()) {
                for t in tool_list {
                    let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    if name.is_empty() {
                        continue;
                    }
                    let desc = t.get("description").and_then(|d| d.as_str()).unwrap_or("");
                    let schema = t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));

                    let mut tool = ToolRecord::new(name, desc, schema);
                    tool.protocol = ProtocolType::ExternalMcp;
                    tool.schema_source = SchemaSource::Upstream;
                    tool.status = ToolStatus::Disabled;
                    tool.last_seen_in_sync = Some(now_epoch_secs());
                    tools.push(tool);
                }
            }

            cursor = result
                .get("nextCursor")
                .and_then(|c| c.as_str())
                .filter(|c| !c.is_empty())
                .map(|c| c.to_string());

            if cursor.is_none() {
                break;
            }
        }

        Ok(tools)
    }

    pub fn call_tool(&self, tool_name: &str, arguments: &JsonValue) -> Result<JsonValue, String> {
        self.call_rpc(
            "tools/call",
            json!({
                "name": tool_name,
                "arguments": arguments.clone()
            }),
        )
    }

    pub fn shutdown(&self) {
        let mut guard = self.process.lock().unwrap();
        if let Some(mut proc) = guard.take() {
            let _ = proc.child.kill();
            let _ = proc.child.wait();
        }
    }
}

impl Drop for StdioUpstreamSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}
