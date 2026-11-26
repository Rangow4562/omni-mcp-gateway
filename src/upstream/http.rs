//! Streamable HTTP Upstream Client & SSRF Guard for Omni MCP.
//! Pure Rust std-only HTTP/1.1 client over raw TCP sockets.
//! Enforces zero-dependency SSRF & DNS-pinning guards, stateful `Mcp-Session-Id`
//! header propagation, cursor pagination, and transparent 404 session-expiry recovery.

use crate::json::JsonValue;
use crate::logging::now_epoch_secs;
use crate::models::{
    ExternalPromptRecord, ExternalResourceRecord, ProtocolType, SamplingElicitationPolicy,
    SchemaSource, SessionMode, ToolRecord, ToolStatus, UNAUTHORIZED,
};
use crate::upstream::secrets::resolve_secret_ref;
use crate::upstream::UpstreamCallbackHub;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
    let (is_https, default_port, without_scheme) = if let Some(rest) = url.strip_prefix("https://")
    {
        #[cfg(not(feature = "tls-rustls"))]
        {
            let _ = rest;
            return Err(format!(
                "Error -32014 UPSTREAM_TLS_UNSUPPORTED: Upstream HTTPS endpoints require the 'tls-rustls' Cargo feature. Rebuild with `--features tls-rustls` or use a local stdio bridge (npx -y supergateway --streamableHttp {}).",
                url
            ));
        }
        #[cfg(feature = "tls-rustls")]
        {
            (true, 443u16, rest)
        }
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, 80u16, rest)
    } else {
        return Err(format!(
            "Invalid URL scheme in '{}'; must start with http:// or https://",
            url
        ));
    };
    let _ = is_https;

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
        None => (authority, default_port),
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
    last_event_id: Mutex<Option<String>>,
    last_initialize: Mutex<Option<f64>>,
    retry_ms: Mutex<Option<u64>>,
    progress_notifications: Mutex<Vec<JsonValue>>,
    callback_hub: Option<Arc<UpstreamCallbackHub>>,
    sampling_policy: SamplingElicitationPolicy,
    registry: Option<crate::control_plane::MCPRegistry>,
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
            last_event_id: Mutex::new(None),
            last_initialize: Mutex::new(None),
            retry_ms: Mutex::new(None),
            progress_notifications: Mutex::new(Vec::new()),
            callback_hub: None,
            sampling_policy: SamplingElicitationPolicy::Deny,
            registry: None,
            next_request_id: AtomicU64::new(1),
        }
    }

    pub fn with_bidirectional_hub(
        mut self,
        hub: Arc<UpstreamCallbackHub>,
        policy: SamplingElicitationPolicy,
    ) -> Self {
        self.callback_hub = Some(hub);
        self.sampling_policy = policy;
        self
    }

    pub fn with_registry(mut self, registry: Option<crate::control_plane::MCPRegistry>) -> Self {
        self.registry = registry;
        self
    }

    /// Sends a `notifications/cancelled` notification to the upstream Streamable HTTP MCP server.
    pub fn cancel_request(&self, request_id: u64, reason: Option<&str>) -> Result<(), String> {
        let cancel_msg = json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {
                "requestId": request_id as i64,
                "reason": reason.unwrap_or("Cancelled by client")
            }
        });
        let _ = self.send_raw_http(&cancel_msg.to_string_compact().into_bytes())?;
        Ok(())
    }

    pub fn get_retry_ms(&self) -> Option<u64> {
        *self.retry_ms.lock().unwrap()
    }

    pub fn drain_progress_notifications(&self) -> Vec<JsonValue> {
        let mut guard = self.progress_notifications.lock().unwrap();
        std::mem::take(&mut *guard)
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

        // Resolve bearer token if present, or fall back to encrypted OAuth 2.1 token store
        // (auto-refreshing if the stored token record has expired and carries a refresh_token).
        let bearer_val = if let Some(ref b_ref) = self.bearer_ref {
            Some(resolve_secret_ref(b_ref, &self.secrets_dir)?)
        } else {
            let mut store = UpstreamOAuthTokenStore::new(&self.secrets_dir, None);
            if let Some(ref reg) = self.registry {
                store = store.with_registry(reg.clone());
            }
            store
                .load_fresh_access_token(&self.server_name, self.allow_loopback)
                .ok()
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
        req_headers_map.insert(
            "accept".to_string(),
            "application/json, text/event-stream".to_string(),
        );
        let last_ev_opt = self.last_event_id.lock().unwrap().clone();
        if let Some(ref ev_id) = last_ev_opt {
            req_headers_map.insert("last-event-id".to_string(), ev_id.clone());
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
        let stream = TcpStream::connect_timeout(&pinned_addr, timeout)
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
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nUser-Agent: omni-mcp/{}\r\nConnection: close\r\n",
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
        if let Some(ref ev_id) = last_ev_opt {
            req_header.push_str(&format!("Last-Event-ID: {}\r\n", ev_id));
        }

        req_header.push_str("\r\n");

        #[cfg(feature = "tls-rustls")]
        if self.url.starts_with("https://") {
            let sni_host = host_header
                .split(':')
                .next()
                .unwrap_or(&host_header)
                .to_string();
            let mut root_store = rustls::RootCertStore::empty();
            root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            if let Ok(ca_bundle_path) = std::env::var("OMNI_MCP_UPSTREAM_CA_BUNDLE") {
                if !ca_bundle_path.trim().is_empty() {
                    if let Ok(ca_pem) = std::fs::read_to_string(&ca_bundle_path) {
                        for der in
                            decode_pem_blocks(&ca_pem, "BEGIN CERTIFICATE", "END CERTIFICATE")
                        {
                            let _ = root_store.add(rustls::pki_types::CertificateDer::from(der));
                        }
                    }
                }
            }

            let builder = rustls::ClientConfig::builder().with_root_certificates(root_store);
            let config = if let (Some(cert_ref), Some(key_ref)) = (
                self.header_refs.get("x-omni-mtls-cert"),
                self.header_refs.get("x-omni-mtls-key"),
            ) {
                let cert_pem = resolve_secret_ref(cert_ref, &self.secrets_dir)?;
                let key_pem = resolve_secret_ref(key_ref, &self.secrets_dir)?;
                let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
                    decode_pem_blocks(&cert_pem, "BEGIN CERTIFICATE", "END CERTIFICATE")
                        .into_iter()
                        .map(rustls::pki_types::CertificateDer::from)
                        .collect();
                let key_der = decode_pem_blocks(&key_pem, "BEGIN PRIVATE KEY", "END PRIVATE KEY")
                    .into_iter()
                    .next()
                    .map(rustls::pki_types::PrivatePkcs8KeyDer::from)
                    .map(rustls::pki_types::PrivateKeyDer::Pkcs8)
                    .ok_or_else(|| "Invalid mTLS client private key PEM".to_string())?;
                builder
                    .with_client_auth_cert(certs, key_der)
                    .map_err(|e| format!("Invalid upstream mTLS client cert/key: {}", e))?
            } else {
                builder.with_no_client_auth()
            };

            let server_name = rustls::pki_types::ServerName::try_from(sni_host.clone())
                .map_err(|e| format!("Invalid TLS SNI hostname '{}': {}", sni_host, e))?;
            let conn = rustls::ClientConnection::new(std::sync::Arc::new(config), server_name)
                .map_err(|e| format!("Failed to initialize TLS session: {}", e))?;
            let mut tls_stream = rustls::StreamOwned::new(conn, stream);
            return self.exchange_http_on_stream(&mut tls_stream, &req_header, body);
        }

        let mut plain_stream = stream;
        self.exchange_http_on_stream(&mut plain_stream, &req_header, body)
    }

    #[allow(clippy::type_complexity)]
    fn exchange_http_on_stream<S: Read + Write>(
        &self,
        stream: &mut S,
        req_header: &str,
        body: &[u8],
    ) -> Result<(u16, BTreeMap<String, String>, Vec<u8>), String> {
        stream
            .write_all(req_header.as_bytes())
            .map_err(|e| format!("Write headers failed: {}", e))?;
        stream
            .write_all(body)
            .map_err(|e| format!("Write body failed: {}", e))?;
        stream.flush().map_err(|e| format!("Flush failed: {}", e))?;

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
        let mut is_chunked = false;

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
                } else if key_lower == "transfer-encoding"
                    && val_trimmed.to_lowercase().contains("chunked")
                {
                    is_chunked = true;
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
        } else if is_chunked {
            resp_body = read_chunked_body(&mut reader)?;
        } else {
            let _ = reader.read_to_end(&mut resp_body);
        }

        Ok((status_code, headers_map, resp_body))
    }

    fn decode_response_json(
        &self,
        headers: &BTreeMap<String, String>,
        resp_bytes: &[u8],
        expected_id: Option<u64>,
    ) -> Result<JsonValue, String> {
        let is_sse = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.to_lowercase().contains("text/event-stream"))
            .unwrap_or(false);

        let resp_str = std::str::from_utf8(resp_bytes)
            .map_err(|e| format!("Invalid UTF-8 in response from upstream: {}", e))?;

        if is_sse {
            let mut last_ev_id: Option<String> = None;
            let mut parsed_retry_ms: Option<u64> = None;
            let mut current_data: Vec<String> = Vec::new();
            let mut parsed_msgs: Vec<JsonValue> = Vec::new();

            let flush_event = |data_lines: &mut Vec<String>, msgs: &mut Vec<JsonValue>| {
                if data_lines.is_empty() {
                    return;
                }
                let joined = data_lines.join("\n");
                data_lines.clear();
                if let Ok(v) = JsonValue::parse(joined.trim()) {
                    msgs.push(v);
                }
            };

            for raw_line in resp_str.lines() {
                let line = raw_line.trim_end_matches('\r');
                if line.is_empty() {
                    flush_event(&mut current_data, &mut parsed_msgs);
                } else if let Some(rest) = line.strip_prefix("data:") {
                    current_data.push(rest.trim_start().to_string());
                } else if let Some(rest) = line.strip_prefix("id:") {
                    let ev = rest.trim().to_string();
                    if !ev.is_empty() {
                        last_ev_id = Some(ev);
                    }
                } else if let Some(rest) = line.strip_prefix("retry:") {
                    if let Ok(ms) = rest.trim().parse::<u64>() {
                        parsed_retry_ms = Some(ms);
                    }
                }
            }
            flush_event(&mut current_data, &mut parsed_msgs);

            if let Some(ev) = last_ev_id {
                *self.last_event_id.lock().unwrap() = Some(ev);
            }
            if let Some(ms) = parsed_retry_ms {
                *self.retry_ms.lock().unwrap() = Some(ms);
            }

            // Capture MCP notifications/progress frames and dispatch reverse server-initiated
            // requests (`sampling/createMessage`, `elicitation/create`, `roots/list`) (v6.5.0).
            let mut progress_buf = self.progress_notifications.lock().unwrap();
            let mut reverse_requests: Vec<(JsonValue, String, JsonValue)> = Vec::new();
            for msg in &parsed_msgs {
                if let Some(method_name) = msg.get("method").and_then(|m| m.as_str()) {
                    if method_name == "notifications/progress" {
                        progress_buf.push(msg.clone());
                        if let Some(ref hub) = self.callback_hub {
                            hub.emit_progress(&self.server_name, msg);
                        }
                    } else if let Some(rev_id) = msg.get("id") {
                        if matches!(
                            method_name,
                            "sampling/createMessage" | "elicitation/create" | "roots/list"
                        ) {
                            reverse_requests.push((
                                rev_id.clone(),
                                method_name.to_string(),
                                msg.get("params").cloned().unwrap_or_else(JsonValue::object),
                            ));
                        }
                    }
                }
            }
            drop(progress_buf);

            for (rev_id, rev_method, rev_params) in reverse_requests {
                let reply = if let Some(ref hub) = self.callback_hub {
                    hub.handle_reverse_request(
                        &self.server_name,
                        rev_id,
                        &rev_method,
                        rev_params,
                        self.sampling_policy.clone(),
                        self.call_timeout_ms,
                    )
                } else {
                    json!({
                        "jsonrpc": "2.0",
                        "id": rev_id,
                        "error": {
                            "code": UNAUTHORIZED,
                            "message": "Sampling/Elicitation denied by gateway policy"
                        }
                    })
                };
                let _ = self.send_raw_http(&reply.to_string_compact().into_bytes());
            }

            if let Some(eid) = expected_id {
                if let Some(matched) = parsed_msgs
                    .iter()
                    .find(|m| m.get("id").and_then(|x| x.as_u64()) == Some(eid))
                {
                    return Ok(matched.clone());
                }
            }

            return parsed_msgs
                .into_iter()
                .rev()
                .find(|m| {
                    m.get("id").is_some() || m.get("result").is_some() || m.get("error").is_some()
                })
                .ok_or_else(|| "Empty SSE stream from upstream".to_string());
        }

        JsonValue::parse(resp_str)
            .map_err(|e| format!("Invalid JSON response from upstream: {}", e))
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
        let (status, headers, resp_bytes) = self.send_raw_http(&body)?;

        if status != 200 {
            return Err(format!(
                "Upstream initialize returned HTTP {}: {}",
                status,
                String::from_utf8_lossy(&resp_bytes)
            ));
        }

        let parsed = self.decode_response_json(&headers, &resp_bytes, Some(1))?;

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
        let (mut status, mut headers, mut resp_bytes) = self.send_raw_http(&body)?;

        // Automatic OAuth 2.1 refresh_token rotation on HTTP 401 Unauthorized (retry once) (v6.5.0)
        if status == 401 && self.bearer_ref.is_none() {
            let mut store = UpstreamOAuthTokenStore::new(&self.secrets_dir, None);
            if let Some(ref reg) = self.registry {
                store = store.with_registry(reg.clone());
            }
            if store
                .refresh_access_token(&self.server_name, self.allow_loopback)
                .is_ok()
            {
                let retry = self.send_raw_http(&body)?;
                status = retry.0;
                headers = retry.1;
                resp_bytes = retry.2;
            }
        }

        // Transparent session-expiry recovery:
        // If HTTP 404 in Stateful mode, re-initialize once to get a fresh session ID and retry
        if status == 404 && self.session_mode == SessionMode::Stateful {
            *self.session_id.lock().unwrap() = None;
            self.initialize()?;
            let retry = self.send_raw_http(&body)?;
            status = retry.0;
            headers = retry.1;
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

        let parsed = self.decode_response_json(&headers, &resp_bytes, Some(req_id))?;

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

    pub fn fetch_resources(&self) -> Result<Vec<ExternalResourceRecord>, String> {
        let result = match self.call_rpc("resources/list", JsonValue::object()) {
            Ok(r) => r,
            Err(e) if e.contains("-32601") || e.contains("Method not found") => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(e),
        };
        let mut out = Vec::new();
        if let Some(items) = result.get("resources").and_then(|r| r.as_array()) {
            for item in items {
                let uri = item.get("uri").and_then(|x| x.as_str()).unwrap_or("");
                let name = item.get("name").and_then(|x| x.as_str()).unwrap_or(uri);
                if uri.is_empty() {
                    continue;
                }
                out.push(ExternalResourceRecord {
                    uri: uri.to_string(),
                    name: name.to_string(),
                    description: item
                        .get("description")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string()),
                    mime_type: item
                        .get("mimeType")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string()),
                });
            }
        }
        Ok(out)
    }

    pub fn fetch_prompts(&self) -> Result<Vec<ExternalPromptRecord>, String> {
        let result = match self.call_rpc("prompts/list", JsonValue::object()) {
            Ok(r) => r,
            Err(e) if e.contains("-32601") || e.contains("Method not found") => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(e),
        };
        let mut out = Vec::new();
        if let Some(items) = result.get("prompts").and_then(|r| r.as_array()) {
            for item in items {
                let name = item.get("name").and_then(|x| x.as_str()).unwrap_or("");
                if name.is_empty() {
                    continue;
                }
                let arguments = item
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| JsonValue::Array(Vec::new()));
                out.push(ExternalPromptRecord {
                    name: name.to_string(),
                    description: item
                        .get("description")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string()),
                    arguments,
                });
            }
        }
        Ok(out)
    }
}

fn read_chunked_body<R: Read>(reader: &mut BufReader<R>) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let mut size_line = String::new();
        let n = reader
            .read_line(&mut size_line)
            .map_err(|e| format!("Failed reading chunk size: {}", e))?;
        if n == 0 {
            break;
        }
        let hex_part = size_line.trim().split(';').next().unwrap_or("").trim();
        if hex_part.is_empty() {
            continue;
        }
        let chunk_size = usize::from_str_radix(hex_part, 16)
            .map_err(|e| format!("Invalid chunk size '{}': {}", hex_part, e))?;
        if chunk_size == 0 {
            break;
        }
        let mut buf = vec![0u8; chunk_size];
        reader
            .read_exact(&mut buf)
            .map_err(|e| format!("Failed reading chunk payload: {}", e))?;
        out.extend_from_slice(&buf);
        let mut crlf = String::new();
        let _ = reader.read_line(&mut crlf);
    }
    Ok(out)
}

#[cfg(feature = "tls-rustls")]
fn decode_pem_blocks(pem: &str, begin_marker: &str, end_marker: &str) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut in_block = false;
    let mut b64 = String::new();
    for line in pem.lines() {
        let trimmed = line.trim();
        if trimmed.contains(begin_marker) {
            in_block = true;
            b64.clear();
        } else if trimmed.contains(end_marker) {
            if in_block {
                if let Some(der) = base64_decode_std(&b64) {
                    out.push(der);
                }
            }
            in_block = false;
            b64.clear();
        } else if in_block {
            b64.push_str(trimmed);
        }
    }
    out
}

#[cfg(feature = "tls-rustls")]
fn base64_decode_std(input: &str) -> Option<Vec<u8>> {
    let cleaned: String = input.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let mut bytes = Vec::with_capacity(cleaned.len() * 3 / 4);
    let mut val = 0u32;
    let mut valb = -8i32;
    for b in cleaned.bytes() {
        if b == b'=' {
            break;
        }
        let d = match b {
            b'A'..=b'Z' => (b - b'A') as u32,
            b'a'..=b'z' => (b - b'a' + 26) as u32,
            b'0'..=b'9' => (b - b'0' + 52) as u32,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        val = (val << 6) | d;
        valb += 6;
        if valb >= 0 {
            bytes.push(((val >> valb) & 0xFF) as u8);
            valb -= 8;
        }
    }
    Some(bytes)
}

/// Full OAuth 2.1 token record sealed in `UpstreamOAuthTokenStore` (v6.5.0).
#[derive(Clone, Debug, PartialEq)]
pub struct OAuthTokenRecord {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<f64>,
    pub token_endpoint: Option<String>,
    pub client_id: Option<String>,
    pub scope: Option<String>,
}

impl OAuthTokenRecord {
    pub fn from_access_token(access_token: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
            refresh_token: None,
            expires_at: None,
            token_endpoint: None,
            client_id: None,
            scope: None,
        }
    }

    pub fn is_expired(&self) -> bool {
        self.expires_at
            .is_some_and(|exp| now_epoch_secs() >= (exp - 15.0))
    }

    pub fn to_json(&self) -> JsonValue {
        json!({
            "access_token": self.access_token.clone(),
            "refresh_token": self.refresh_token.as_ref().map(|s| JsonValue::from(s.as_str())).unwrap_or(JsonValue::Null),
            "expires_at": self.expires_at.map(JsonValue::from).unwrap_or(JsonValue::Null),
            "token_endpoint": self.token_endpoint.as_ref().map(|s| JsonValue::from(s.as_str())).unwrap_or(JsonValue::Null),
            "client_id": self.client_id.as_ref().map(|s| JsonValue::from(s.as_str())).unwrap_or(JsonValue::Null),
            "scope": self.scope.as_ref().map(|s| JsonValue::from(s.as_str())).unwrap_or(JsonValue::Null),
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let access_token = v.get("access_token")?.as_str()?.to_string();
        Some(Self {
            access_token,
            refresh_token: v
                .get("refresh_token")
                .and_then(|x| x.as_str())
                .map(String::from),
            expires_at: v.get("expires_at").and_then(|x| x.as_f64()),
            token_endpoint: v
                .get("token_endpoint")
                .and_then(|x| x.as_str())
                .map(String::from),
            client_id: v
                .get("client_id")
                .and_then(|x| x.as_str())
                .map(String::from),
            scope: v.get("scope").and_then(|x| x.as_str()).map(String::from),
        })
    }
}

/// Encrypted OAuth 2.1 + PKCE token store for hosted upstream SaaS MCP servers (v6.0.0 / v6.5.0).
/// Uses `ring::aead::CHACHA20_POLY1305` AEAD (`version: 2`) when `tls-rustls` is enabled,
/// with transparent read-fallback for `v6.0.0` SHA-256 CTR+HMAC envelopes.
pub struct UpstreamOAuthTokenStore {
    store_dir: PathBuf,
    master_key: [u8; 32],
    registry: Option<crate::control_plane::MCPRegistry>,
}

impl UpstreamOAuthTokenStore {
    pub fn new(store_dir: impl Into<PathBuf>, master_key_str: Option<&str>) -> Self {
        let key_src = master_key_str
            .map(|s| s.to_string())
            .or_else(|| std::env::var("OMNI_TOKEN_ENCRYPTION_KEY").ok())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| crate::models::continuation_key().to_string());
        let master_key = crate::storage::sha256(key_src.as_bytes());
        Self {
            store_dir: store_dir.into(),
            master_key,
            registry: None,
        }
    }

    pub fn with_registry(mut self, registry: crate::control_plane::MCPRegistry) -> Self {
        self.registry = Some(registry);
        self
    }

    pub fn generate_pkce_pair() -> (String, String) {
        let mut entropy = [0u8; 32];
        crate::models::gather_entropy(&mut entropy);
        let verifier = base64url_encode_no_pad(&entropy);
        let challenge_hash = crate::storage::sha256(verifier.as_bytes());
        let challenge = base64url_encode_no_pad(&challenge_hash);
        (verifier, challenge)
    }

    /// Build an OAuth 2.1 Authorization Code + PKCE (S256) authorization URL.
    pub fn build_authorization_url(
        authorize_endpoint: &str,
        client_id: &str,
        redirect_uri: &str,
        scope: &str,
        state: &str,
        code_challenge: &str,
    ) -> String {
        let sep = if authorize_endpoint.contains('?') {
            '&'
        } else {
            '?'
        };
        format!(
            "{}{}response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
            authorize_endpoint,
            sep,
            url_encode_component(client_id),
            url_encode_component(redirect_uri),
            url_encode_component(scope),
            url_encode_component(state),
            url_encode_component(code_challenge),
        )
    }

    /// Build a `application/x-www-form-urlencoded` body for OAuth 2.1 `client_credentials` grant.
    pub fn build_client_credentials_body(
        client_id: &str,
        client_secret: &str,
        scope: Option<&str>,
    ) -> String {
        let mut body = format!(
            "grant_type=client_credentials&client_id={}&client_secret={}",
            url_encode_component(client_id),
            url_encode_component(client_secret),
        );
        if let Some(s) = scope.filter(|s| !s.is_empty()) {
            body.push_str("&scope=");
            body.push_str(&url_encode_component(s));
        }
        body
    }

    /// Build a `application/x-www-form-urlencoded` body for OAuth 2.1 `authorization_code` + PKCE exchange.
    pub fn build_auth_code_exchange_body(
        client_id: &str,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> String {
        format!(
            "grant_type=authorization_code&client_id={}&code={}&redirect_uri={}&code_verifier={}",
            url_encode_component(client_id),
            url_encode_component(code),
            url_encode_component(redirect_uri),
            url_encode_component(code_verifier),
        )
    }

    /// Build a `application/x-www-form-urlencoded` body for OAuth 2.1 `refresh_token` rotation.
    pub fn build_refresh_token_body(
        client_id: Option<&str>,
        refresh_token: &str,
        scope: Option<&str>,
    ) -> String {
        let mut body = format!(
            "grant_type=refresh_token&refresh_token={}",
            url_encode_component(refresh_token),
        );
        if let Some(cid) = client_id.filter(|s| !s.is_empty()) {
            body.push_str("&client_id=");
            body.push_str(&url_encode_component(cid));
        }
        if let Some(s) = scope.filter(|s| !s.is_empty()) {
            body.push_str("&scope=");
            body.push_str(&url_encode_component(s));
        }
        body
    }

    fn token_file_path(&self, server_name: &str) -> PathBuf {
        let safe_name: String = server_name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.store_dir.join(format!("{}.oauth.enc", safe_name))
    }

    pub fn save_token(&self, server_name: &str, access_token: &str) -> Result<(), String> {
        self.save_token_record(
            server_name,
            &OAuthTokenRecord::from_access_token(access_token),
        )
    }

    /// Seals a full `OAuthTokenRecord` using `ring::aead::CHACHA20_POLY1305` (with 96-bit OS CSPRNG
    /// nonce and `server_name` AAD) when `tls-rustls` is compiled in, or SHA-256 CTR + HMAC fallback.
    pub fn save_token_record(
        &self,
        server_name: &str,
        record: &OAuthTokenRecord,
    ) -> Result<(), String> {
        let _ = std::fs::create_dir_all(&self.store_dir);
        let plaintext = record.to_json().to_string_compact().into_bytes();
        let path = self.token_file_path(server_name);

        #[cfg(feature = "tls-rustls")]
        {
            use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
            let mut nonce_bytes = [0u8; 12];
            crate::models::gather_entropy(&mut nonce_bytes);
            let unbound = UnboundKey::new(&CHACHA20_POLY1305, &self.master_key)
                .map_err(|_| "Failed to initialize ChaCha20-Poly1305 key".to_string())?;
            let key = LessSafeKey::new(unbound);
            let mut in_out = plaintext;
            key.seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce_bytes),
                Aad::from(server_name.as_bytes()),
                &mut in_out,
            )
            .map_err(|_| "ChaCha20-Poly1305 AEAD seal failed".to_string())?;

            let payload = json!({
                "version": 2i64,
                "alg": "ChaCha20-Poly1305",
                "server": server_name,
                "nonce_hex": hex_encode_bytes(&nonce_bytes),
                "ciphertext_hex": hex_encode_bytes(&in_out),
            });
            let compact = payload.to_string_compact();
            if let Some(ref reg) = self.registry {
                reg.save_oauth_token(&crate::models::OAuthTokenRecord {
                    server_name: server_name.to_string(),
                    sealed_envelope_hex: hex_encode_bytes(compact.as_bytes()),
                    updated_at: crate::logging::now_epoch_secs(),
                });
            }
            std::fs::write(&path, compact)
                .map_err(|e| format!("Failed to write AEAD OAuth token: {}", e))
        }

        #[cfg(not(feature = "tls-rustls"))]
        {
            let mut nonce = [0u8; 16];
            crate::models::gather_entropy(&mut nonce);
            let mut ciphertext = Vec::with_capacity(plaintext.len());
            for (counter, chunk) in plaintext.chunks(32).enumerate() {
                let mut block_in = Vec::with_capacity(68);
                block_in.extend_from_slice(&self.master_key);
                block_in.extend_from_slice(&nonce);
                block_in.extend_from_slice(&(counter as u32).to_le_bytes());
                let keystream = crate::storage::sha256(&block_in);
                for (i, b) in chunk.iter().enumerate() {
                    ciphertext.push(b ^ keystream[i]);
                }
            }
            let mut blob = Vec::with_capacity(nonce.len() + ciphertext.len());
            blob.extend_from_slice(&nonce);
            blob.extend_from_slice(&ciphertext);
            let mac = crate::storage::audit_log::hmac_sha256_hex(&self.master_key, &blob);
            let payload = json!({
                "version": 1i64,
                "alg": "SHA256-CTR-HMAC",
                "server": server_name,
                "nonce_hex": hex_encode_bytes(&nonce),
                "ciphertext_hex": hex_encode_bytes(&ciphertext),
                "hmac_sha256": mac,
            });
            let compact = payload.to_string_compact();
            if let Some(ref reg) = self.registry {
                reg.save_oauth_token(&crate::models::OAuthTokenRecord {
                    server_name: server_name.to_string(),
                    sealed_envelope_hex: hex_encode_bytes(compact.as_bytes()),
                    updated_at: crate::logging::now_epoch_secs(),
                });
            }
            std::fs::write(&path, compact)
                .map_err(|e| format!("Failed to write encrypted OAuth token: {}", e))
        }
    }

    pub fn load_token(&self, server_name: &str) -> Result<String, String> {
        self.load_token_record(server_name).map(|r| r.access_token)
    }

    /// Loads and decrypts an `OAuthTokenRecord`, checking shared multi-replica `omni_oauth_tokens`
    /// first and falling back to local `.oauth.enc` files (`v6.0.0` / `v6.5.0` / `v6.8.0`).
    pub fn load_token_record(&self, server_name: &str) -> Result<OAuthTokenRecord, String> {
        let raw = if let Some(ref reg) = self.registry {
            if let Some(rec) = reg.get_oauth_token(server_name) {
                let bytes = hex_decode_bytes(&rec.sealed_envelope_hex)?;
                String::from_utf8(bytes)
                    .map_err(|_| "Corrupt UTF-8 in distributed OAuth token envelope".to_string())?
            } else {
                let path = self.token_file_path(server_name);
                std::fs::read_to_string(&path)
                    .map_err(|e| format!("Failed to read encrypted OAuth token: {}", e))?
            }
        } else {
            let path = self.token_file_path(server_name);
            std::fs::read_to_string(&path)
                .map_err(|e| format!("Failed to read encrypted OAuth token: {}", e))?
        };
        let parsed = JsonValue::parse(&raw)?;
        let alg = parsed.get("alg").and_then(|v| v.as_str()).unwrap_or("");

        let plaintext_str = if alg == "ChaCha20-Poly1305" {
            #[cfg(not(feature = "tls-rustls"))]
            {
                return Err(
                    "ChaCha20-Poly1305 token envelope requires the 'tls-rustls' (ring) feature"
                        .to_string(),
                );
            }
            #[cfg(feature = "tls-rustls")]
            {
                use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
                let nonce = hex_decode_bytes(
                    parsed
                        .get("nonce_hex")
                        .and_then(|v| v.as_str())
                        .ok_or("Missing nonce_hex")?,
                )?;
                if nonce.len() != 12 {
                    return Err(format!("Invalid AEAD nonce length {}", nonce.len()));
                }
                let mut nonce_arr = [0u8; 12];
                nonce_arr.copy_from_slice(&nonce);
                let mut ciphertext = hex_decode_bytes(
                    parsed
                        .get("ciphertext_hex")
                        .and_then(|v| v.as_str())
                        .ok_or("Missing ciphertext_hex")?,
                )?;
                let unbound = UnboundKey::new(&CHACHA20_POLY1305, &self.master_key)
                    .map_err(|_| "Failed to initialize ChaCha20-Poly1305 key".to_string())?;
                let key = LessSafeKey::new(unbound);
                let plain = key
                    .open_in_place(
                        Nonce::assume_unique_for_key(nonce_arr),
                        Aad::from(server_name.as_bytes()),
                        &mut ciphertext,
                    )
                    .map_err(|_| {
                        "OAuth token AEAD ChaCha20-Poly1305 authentication/decryption failed"
                            .to_string()
                    })?;
                String::from_utf8(plain.to_vec())
                    .map_err(|e| format!("Invalid UTF-8 token payload: {}", e))?
            }
        } else {
            // Transparent legacy v6.0.0 SHA-256 CTR + HMAC read fallback
            let nonce = hex_decode_bytes(
                parsed
                    .get("nonce_hex")
                    .and_then(|v| v.as_str())
                    .ok_or("Missing nonce_hex")?,
            )?;
            let ciphertext = hex_decode_bytes(
                parsed
                    .get("ciphertext_hex")
                    .and_then(|v| v.as_str())
                    .ok_or("Missing ciphertext_hex")?,
            )?;
            let mac = parsed
                .get("hmac_sha256")
                .and_then(|v| v.as_str())
                .ok_or("Missing hmac_sha256")?;
            let mut blob = Vec::with_capacity(nonce.len() + ciphertext.len());
            blob.extend_from_slice(&nonce);
            blob.extend_from_slice(&ciphertext);
            let expected_mac = crate::storage::audit_log::hmac_sha256_hex(&self.master_key, &blob);
            if !crate::models::constant_time_eq(mac.as_bytes(), expected_mac.as_bytes()) {
                return Err("OAuth token store HMAC verification failed".to_string());
            }
            let mut plaintext = Vec::with_capacity(ciphertext.len());
            for (counter, chunk) in ciphertext.chunks(32).enumerate() {
                let mut block_in = Vec::with_capacity(68);
                block_in.extend_from_slice(&self.master_key);
                block_in.extend_from_slice(&nonce);
                block_in.extend_from_slice(&(counter as u32).to_le_bytes());
                let keystream = crate::storage::sha256(&block_in);
                for (i, b) in chunk.iter().enumerate() {
                    plaintext.push(b ^ keystream[i]);
                }
            }
            String::from_utf8(plaintext).map_err(|e| format!("Invalid UTF-8 token: {}", e))?
        };

        // Support both v6.5 JSON OAuthTokenRecord and legacy raw access_token string
        if plaintext_str.trim_start().starts_with('{') {
            if let Ok(json_val) = JsonValue::parse(&plaintext_str) {
                if let Some(rec) = OAuthTokenRecord::from_json(&json_val) {
                    return Ok(rec);
                }
            }
        }
        Ok(OAuthTokenRecord::from_access_token(plaintext_str))
    }

    /// Returns a valid access token, automatically rotating via `refresh_token` if expired.
    pub fn load_fresh_access_token(
        &self,
        server_name: &str,
        allow_loopback: bool,
    ) -> Result<String, String> {
        let rec = self.load_token_record(server_name)?;
        if rec.is_expired() && rec.refresh_token.is_some() && rec.token_endpoint.is_some() {
            return self.refresh_access_token(server_name, allow_loopback);
        }
        Ok(rec.access_token)
    }

    /// Executes OAuth 2.1 `grant_type=refresh_token` rotation against `token_endpoint`
    /// and seals the refreshed token record back to disk in AEAD `v2` format.
    pub fn refresh_access_token(
        &self,
        server_name: &str,
        allow_loopback: bool,
    ) -> Result<String, String> {
        let mut rec = self.load_token_record(server_name)?;
        let refresh_tok = rec
            .refresh_token
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("No refresh_token stored for '{}'", server_name))?;
        let token_endpoint = rec
            .token_endpoint
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("No token_endpoint stored for '{}'", server_name))?;

        let form_body = Self::build_refresh_token_body(
            rec.client_id.as_deref(),
            refresh_tok,
            rec.scope.as_deref(),
        );
        let body_bytes = form_body.into_bytes();

        let mut req_headers = BTreeMap::new();
        req_headers.insert(
            "content-type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        );
        req_headers.insert("accept".to_string(), "application/json".to_string());

        let resp_bytes = if let Some((status, _hdrs, bytes)) =
            invoke_mock_http(token_endpoint, token_endpoint, &req_headers, &body_bytes)
        {
            if status >= 400 {
                return Err(format!(
                    "OAuth token refresh failed with HTTP {}: {}",
                    status,
                    String::from_utf8_lossy(&bytes)
                ));
            }
            bytes
        } else {
            let _ = validate_upstream_url_and_resolve(token_endpoint, allow_loopback)?;
            let mut hdr_map = std::collections::HashMap::new();
            hdr_map.insert(
                "Content-Type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            );
            hdr_map.insert("Accept".to_string(), "application/json".to_string());
            let client = crate::data_plane::HttpDownstreamClient::new();
            client.post_json(token_endpoint, &body_bytes, Some(&hdr_map))?
        };

        let resp_str = String::from_utf8(resp_bytes)
            .map_err(|e| format!("Invalid UTF-8 in OAuth refresh response: {}", e))?;
        let parsed = JsonValue::parse(&resp_str)?;
        let new_access = parsed
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "OAuth refresh response missing 'access_token'".to_string())?
            .to_string();

        rec.access_token = new_access.clone();
        if let Some(new_ref) = parsed.get("refresh_token").and_then(|v| v.as_str()) {
            if !new_ref.is_empty() {
                rec.refresh_token = Some(new_ref.to_string());
            }
        }
        if let Some(expires_in) = parsed.get("expires_in").and_then(|v| v.as_f64()) {
            rec.expires_at = Some(now_epoch_secs() + expires_in);
        }
        self.save_token_record(server_name, &rec)?;
        Ok(new_access)
    }
}

fn hex_encode_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn hex_decode_bytes(hex: &str) -> Result<Vec<u8>, String> {
    if hex.len() % 2 != 0 {
        return Err("Odd hex length".to_string());
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

fn base64url_encode_no_pad(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 63) as usize] as char);
        }
    }
    out
}

fn url_encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{:02X}", b));
            }
        }
    }
    out
}

#[cfg(test)]
mod oauth_tests {
    use super::*;

    #[test]
    fn test_upstream_oauth_pkce_and_encrypted_token_store() {
        let dir = std::env::temp_dir().join(format!(
            "omni_oauth_test_{}",
            crate::logging::now_epoch_millis()
        ));
        let store = UpstreamOAuthTokenStore::new(&dir, Some("test-master-key-v6"));
        let (verifier, challenge) = UpstreamOAuthTokenStore::generate_pkce_pair();
        assert!(!verifier.is_empty());
        assert!(!challenge.is_empty());
        assert_ne!(verifier, challenge);

        let auth_url = UpstreamOAuthTokenStore::build_authorization_url(
            "https://auth.example.com/authorize",
            "client-123",
            "https://gw.example.com/callback",
            "mcp:tools",
            "st-xyz",
            &challenge,
        );
        assert!(auth_url.contains("code_challenge_method=S256"));
        assert!(auth_url.contains(&challenge));

        let cc_body = UpstreamOAuthTokenStore::build_client_credentials_body(
            "cid",
            "csec",
            Some("read write"),
        );
        assert!(cc_body.contains("grant_type=client_credentials"));
        assert!(cc_body.contains("scope=read%20write"));

        let ac_body = UpstreamOAuthTokenStore::build_auth_code_exchange_body(
            "cid",
            "code123",
            "https://gw.example.com/callback",
            &verifier,
        );
        assert!(ac_body.contains("grant_type=authorization_code"));
        assert!(ac_body.contains(&verifier));

        store
            .save_token("linear-mcp", "tok_secret_value_999")
            .unwrap();
        let loaded = store.load_token("linear-mcp").unwrap();
        assert_eq!(loaded, "tok_secret_value_999");

        // Wrong key must fail HMAC verification
        let wrong_store = UpstreamOAuthTokenStore::new(&dir, Some("wrong-key"));
        assert!(wrong_store.load_token("linear-mcp").is_err());

        // Verify HttpUpstreamSession automatically loads encrypted OAuth token when bearer_ref is None
        let default_store = UpstreamOAuthTokenStore::new(&dir, None);
        default_store
            .save_token("linear-mcp", "tok_wire_secret_777")
            .unwrap();
        let mock_url = "http://127.0.0.1:59991/mcp";
        register_mock_http(mock_url, |_path, _body, hdrs| {
            let auth = hdrs.get("authorization").cloned().unwrap_or_default();
            let mut resp_hdrs = BTreeMap::new();
            resp_hdrs.insert("content-type".to_string(), "application/json".to_string());
            let resp_body = format!(
                r#"{{"jsonrpc":"2.0","id":1,"result":{{"auth":"{}"}}}}"#,
                auth
            );
            (200, resp_hdrs, resp_body.into_bytes())
        });
        let session = HttpUpstreamSession::new(
            "linear-mcp",
            mock_url,
            None,
            BTreeMap::new(),
            SessionMode::Stateless,
            dir.clone(),
            2000,
            true,
        );
        let (_, _, resp_bytes) = session.send_raw_http(b"{}").unwrap();
        let resp_str = String::from_utf8(resp_bytes).unwrap();
        assert!(
            resp_str.contains("Bearer tok_wire_secret_777"),
            "expected decrypted OAuth token in Authorization header, got: {}",
            resp_str
        );
        clear_mock_http(mock_url);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
