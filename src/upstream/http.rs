//! Streamable HTTP Upstream Client & SSRF Guard for Omni MCP.
//! Pure Rust std-only HTTP/1.1 client over raw TCP sockets.
//! Enforces zero-dependency SSRF & DNS-pinning guards, stateful `Mcp-Session-Id`
//! header propagation, cursor pagination, and transparent 404 session-expiry recovery.

use crate::json::JsonValue;
use crate::logging::now_epoch_secs;
use crate::models::{ProtocolType, SchemaSource, SessionMode, ToolRecord, ToolStatus};
use crate::upstream::secrets::resolve_secret_ref;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

pub const MAX_PAGINATION_PAGES: usize = 50;

pub type MockHttpHandler = Box<
    dyn Fn(&str, &[u8], &BTreeMap<String, String>) -> (u16, BTreeMap<String, String>, Vec<u8>)
        + Send
        + Sync,
>;

static MOCK_HTTP_REGISTRY: Mutex<Option<BTreeMap<String, MockHttpHandler>>> = Mutex::new(None);

pub fn register_mock_http<F>(url_prefix: &str, handler: F)
where
    F: Fn(&str, &[u8], &BTreeMap<String, String>) -> (u16, BTreeMap<String, String>, Vec<u8>)
        + Send
        + Sync
        + 'static,
{
    let mut guard = MOCK_HTTP_REGISTRY.lock().unwrap();
    if guard.is_none() {
        *guard = Some(BTreeMap::new());
    }
    if let Some(ref mut map) = *guard {
        map.insert(url_prefix.to_string(), Box::new(handler));
    }
}

pub fn clear_mock_http(url_prefix: &str) {
    let mut guard = MOCK_HTTP_REGISTRY.lock().unwrap();
    if let Some(ref mut map) = *guard {
        map.remove(url_prefix);
    }
}

fn invoke_mock_http(
    url: &str,
    path: &str,
    headers: &BTreeMap<String, String>,
    body: &[u8],
) -> Option<(u16, BTreeMap<String, String>, Vec<u8>)> {
    let guard = MOCK_HTTP_REGISTRY.lock().unwrap();
    if let Some(ref map) = *guard {
        for (prefix, handler) in map {
            if url.starts_with(prefix) {
                return Some(handler(path, body, headers));
            }
        }
    }
    None
}

/// Validates upstream URL, checks SSRF IP deny-lists, and resolves to a pinned `SocketAddr`.
pub fn validate_upstream_url_and_resolve(
    url: &str,
    allow_loopback: bool,
) -> Result<(SocketAddr, String, String), String> {
    if url.starts_with("https://") {
        return Err(format!(
            "Error -32014 UPSTREAM_TLS_UNSUPPORTED: Upstream HTTPS endpoints are not supported directly in std-only runtime. Use a local stdio bridge (npx -y supergateway --streamableHttp {}) or a local TLS proxy.",
            url
        ));
    }

    if !url.starts_with("http://") {
        return Err(format!(
            "Invalid URL scheme in '{}'; must start with http://",
            url
        ));
    }

    let without_scheme = &url["http://".len()..];
    let (authority, path) = match without_scheme.find('/') {
        Some(idx) => (&without_scheme[..idx], &without_scheme[idx..]),
        None => (without_scheme, "/"),
    };

    let (host, port) = match authority.find(':') {
        Some(idx) => {
            let h = &authority[..idx];
            let p: u16 = authority[idx + 1..]
                .parse()
                .map_err(|e| format!("Invalid port in URL '{}': {}", url, e))?;
            (h, p)
        }
        None => (authority, 80),
    };

    if host.is_empty() {
        return Err(format!("Empty host in URL '{}'", url));
    }

    let socket_addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("DNS resolution failed for '{}:{}': {}", host, port, e))?
        .collect();

    if socket_addrs.is_empty() {
        return Err(format!("No IP addresses found for host '{}'", host));
    }

    // SSRF Deny-list Verification
    for addr in &socket_addrs {
        let ip = addr.ip();

        // 1. Cloud metadata IP: 169.254.169.254
        if ip == IpAddr::V4(std::net::Ipv4Addr::new(169, 254, 169, 254)) {
            return Err(
                "SSRF Guard: connection to cloud metadata IP 169.254.169.254 is forbidden"
                    .to_string(),
            );
        }

        // 2. Link-local (169.254.0.0/16 and fe80::/10)
        match ip {
            IpAddr::V4(v4) => {
                if v4.is_link_local() {
                    return Err(
                        "SSRF Guard: connection to link-local IP 169.254.0.0/16 is forbidden"
                            .to_string(),
                    );
                }
                if v4.is_loopback() && !allow_loopback {
                    return Err(
                        "SSRF Guard: loopback connections are disabled in this environment"
                            .to_string(),
                    );
                }
            }
            IpAddr::V6(v6) => {
                let segments = v6.segments();
                if (segments[0] & 0xffc0) == 0xfe80 {
                    return Err(
                        "SSRF Guard: connection to link-local IPv6 fe80::/10 is forbidden"
                            .to_string(),
                    );
                }
                if v6.is_loopback() && !allow_loopback {
                    return Err(
                        "SSRF Guard: loopback connections are disabled in this environment"
                            .to_string(),
                    );
                }
            }
        }
    }

    // Pin the first resolved SocketAddr to guard against DNS rebinding
    let pinned_addr = socket_addrs[0];
    Ok((pinned_addr, authority.to_string(), path.to_string()))
}

pub struct HttpUpstreamSession {
    pub server_name: String,
    pub url: String,
    pub bearer_ref: Option<String>,
    pub header_refs: BTreeMap<String, String>,
    pub session_mode: SessionMode,
    pub secrets_dir: PathBuf,
    pub call_timeout_ms: u64,
    pub allow_loopback: bool,
    session_id: Mutex<Option<String>>,
    last_initialize: Mutex<Option<f64>>,
    next_request_id: AtomicU64,
}

impl HttpUpstreamSession {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        server_name: impl Into<String>,
        url: impl Into<String>,
        bearer_ref: Option<String>,
        header_refs: BTreeMap<String, String>,
        session_mode: SessionMode,
        secrets_dir: PathBuf,
        call_timeout_ms: u64,
        allow_loopback: bool,
    ) -> Self {
        Self {
            server_name: server_name.into(),
            url: url.into(),
            bearer_ref,
            header_refs,
            session_mode,
            secrets_dir,
            call_timeout_ms,
            allow_loopback,
            session_id: Mutex::new(None),
            last_initialize: Mutex::new(None),
            next_request_id: AtomicU64::new(1),
        }
    }

    pub fn get_health(&self) -> &'static str {
        if self.last_initialize.lock().unwrap().is_some() {
            "Connected"
        } else {
            "Idle"
        }
    }

    pub fn get_last_initialize(&self) -> Option<f64> {
        *self.last_initialize.lock().unwrap()
    }

    pub fn get_session_id(&self) -> Option<String> {
        self.session_id.lock().unwrap().clone()
    }

    #[allow(clippy::type_complexity)]
    fn send_raw_http(
        &self,
        body: &[u8],
    ) -> Result<(u16, BTreeMap<String, String>, Vec<u8>), String> {
        let (pinned_addr, host_header, path) =
            validate_upstream_url_and_resolve(&self.url, self.allow_loopback)?;

        // Resolve bearer token if present
        let bearer_val = if let Some(ref b_ref) = self.bearer_ref {
            Some(resolve_secret_ref(b_ref, &self.secrets_dir)?)
        } else {
            None
        };

        // Resolve headers
        let mut resolved_headers = Vec::new();
        for (k, v_ref) in &self.header_refs {
            let v = resolve_secret_ref(v_ref, &self.secrets_dir)?;
            resolved_headers.push((k.clone(), v));
        }

        let sid_opt = if self.session_mode == SessionMode::Stateful {
            self.session_id.lock().unwrap().clone()
        } else {
            None
        };

        let mut req_headers_map = BTreeMap::new();
        req_headers_map.insert("content-type".to_string(), "application/json".to_string());
        req_headers_map.insert("content-length".to_string(), body.len().to_string());
        req_headers_map.insert(
            "user-agent".to_string(),
            format!("omni-mcp/{}", env!("CARGO_PKG_VERSION")),
        );
        if let Some(ref tok) = bearer_val {
            req_headers_map.insert("authorization".to_string(), format!("Bearer {}", tok));
        }
        for (k, v) in &resolved_headers {
            req_headers_map.insert(k.to_lowercase(), v.clone());
        }
        if let Some(ref sid) = sid_opt {
            req_headers_map.insert("mcp-session-id".to_string(), sid.clone());
        }

        if let Some(mock_resp) = invoke_mock_http(&self.url, &path, &req_headers_map, body) {
            let (status_code, headers_map, resp_body) = mock_resp;
            let sid = headers_map
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("mcp-session-id"))
                .map(|(_, v)| v.clone());
            if let Some(upstream_sid) = sid {
                if !upstream_sid.is_empty() {
                    *self.session_id.lock().unwrap() = Some(upstream_sid);
                }
            }
            return Ok((status_code, headers_map, resp_body));
        }

        let timeout = Duration::from_millis(self.call_timeout_ms);
        let mut stream = TcpStream::connect_timeout(&pinned_addr, timeout)
            .or_else(|_| TcpStream::connect(pinned_addr))
            .map_err(|e| {
                format!(
                    "Failed to connect to upstream {} ({}): {}",
                    self.url, pinned_addr, e
                )
            })?;

        stream
            .set_read_timeout(Some(timeout))
            .map_err(|e| format!("Failed to set read timeout: {}", e))?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|e| format!("Failed to set write timeout: {}", e))?;

        let mut req_header = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nUser-Agent: omni-mcp/{}\r\nConnection: close\r\n",
            path,
            host_header,
            body.len(),
            env!("CARGO_PKG_VERSION")
        );

        if let Some(ref tok) = bearer_val {
            req_header.push_str(&format!("Authorization: Bearer {}\r\n", tok));
        }

        for (k, v) in resolved_headers {
            req_header.push_str(&format!("{}: {}\r\n", k, v));
        }

        if let Some(ref sid) = sid_opt {
            req_header.push_str(&format!("Mcp-Session-Id: {}\r\n", sid));
        }

        req_header.push_str("\r\n");

        stream
            .write_all(req_header.as_bytes())
            .map_err(|e| format!("Write headers failed: {}", e))?;
        stream
            .write_all(body)
            .map_err(|e| format!("Write body failed: {}", e))?;
        stream.flush().map_err(|e| format!("Flush failed: {}", e))?;

        // Read response
        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        reader
            .read_line(&mut status_line)
            .map_err(|e| format!("Failed to read HTTP status line: {}", e))?;

        let status_parts: Vec<&str> = status_line.split_whitespace().collect();
        if status_parts.len() < 2 {
            return Err(format!(
                "Malformed HTTP status line: '{}'",
                status_line.trim()
            ));
        }

        let status_code: u16 = status_parts[1]
            .parse()
            .map_err(|_| format!("Invalid status code in line: '{}'", status_line.trim()))?;

        // Reject HTTP redirects (301, 302, 307, 308) to guard against SSRF redirects
        if (300..=399).contains(&status_code) {
            return Err("SSRF Guard: HTTP redirects are not followed".to_string());
        }

        let mut headers_map: BTreeMap<String, String> = BTreeMap::new();
        let mut content_length: Option<usize> = None;

        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                let key_lower = k.trim().to_lowercase();
                let val_trimmed = v.trim().to_string();
                if key_lower == "content-length" {
                    content_length = val_trimmed.parse().ok();
                }
                headers_map.insert(key_lower, val_trimmed);
            }
        }

        // Capture Mcp-Session-Id header if returned by upstream
        if let Some(upstream_sid) = headers_map.get("mcp-session-id") {
            if !upstream_sid.is_empty() {
                *self.session_id.lock().unwrap() = Some(upstream_sid.clone());
            }
        }

        let mut resp_body = Vec::new();
        if let Some(len) = content_length {
            resp_body.resize(len, 0);
            reader
                .read_exact(&mut resp_body)
                .map_err(|e| format!("Failed to read HTTP response body: {}", e))?;
        } else {
            let _ = reader.read_to_end(&mut resp_body);
        }

        Ok((status_code, headers_map, resp_body))
    }

    pub fn initialize(&self) -> Result<(), String> {
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

        let body = init_req.to_string_compact().into_bytes();
        let (status, _headers, resp_bytes) = self.send_raw_http(&body)?;

        if status != 200 {
            return Err(format!(
                "Upstream initialize returned HTTP {}: {}",
                status,
                String::from_utf8_lossy(&resp_bytes)
            ));
        }

        let resp_str = String::from_utf8(resp_bytes)
            .map_err(|e| format!("Invalid UTF-8 from upstream initialize: {}", e))?;
        let parsed = JsonValue::parse(&resp_str)
            .map_err(|e| format!("Invalid JSON from upstream initialize: {}", e))?;

        if let Some(err) = parsed.get("error") {
            return Err(format!("Initialize error: {}", err.to_string_compact()));
        }

        // Send notifications/initialized
        let notify = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        let _ = self.send_raw_http(&notify.to_string_compact().into_bytes());

        *self.last_initialize.lock().unwrap() = Some(now_epoch_secs());
        Ok(())
    }

    pub fn call_rpc(&self, method: &str, params: JsonValue) -> Result<JsonValue, String> {
        if self.last_initialize.lock().unwrap().is_none() {
            self.initialize()?;
        }

        let req_id = self.next_request_id.fetch_add(1, Ordering::SeqCst) + 2;
        let req = json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": method,
            "params": params
        });

        let body = req.to_string_compact().into_bytes();
        let (mut status, mut _headers, mut resp_bytes) = self.send_raw_http(&body)?;

        // Transparent session-expiry recovery:
        // If HTTP 404 in Stateful mode, re-initialize once to get a fresh session ID and retry
        if status == 404 && self.session_mode == SessionMode::Stateful {
            *self.session_id.lock().unwrap() = None;
            self.initialize()?;
            let retry = self.send_raw_http(&body)?;
            status = retry.0;
            _headers = retry.1;
            resp_bytes = retry.2;
        }

        if status != 200 {
            return Err(format!(
                "Upstream call '{}' returned HTTP {}: {}",
                method,
                status,
                String::from_utf8_lossy(&resp_bytes)
            ));
        }

        let resp_str = String::from_utf8(resp_bytes)
            .map_err(|e| format!("Invalid UTF-8 in response from upstream: {}", e))?;
        let parsed = JsonValue::parse(&resp_str)
            .map_err(|e| format!("Invalid JSON response from upstream: {}", e))?;

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
}
