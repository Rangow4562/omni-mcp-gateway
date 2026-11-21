//! Control Plane REST Management API (`/api/...`, `/healthz`, `/readyz`, `/metrics`).

use super::auth::{AuthPrincipal, AuthRole};
use super::http_codec::percent_decode;
use super::{OAuthPendingFlow, ProductionGatewayContext};
use crate::data_plane::{FaultInjectionConfig, PIIRedactor, SandboxLimits};
use crate::json::JsonValue;
use crate::logging::now_epoch_secs;
use crate::models::{
    generate_ulid, ProtocolType, ServerRecord, SessionMode, ToolRecord, ToolStatus,
    UpstreamTransportConfig,
};
use crate::upstream::{OAuthTokenRecord, UpstreamOAuthTokenStore};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::Ordering;

fn parse_query_params(query: Option<&str>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(q) = query else {
        return map;
    };
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        if let Some((k, v)) = pair.split_once('=') {
            map.insert(percent_decode(k), percent_decode(&v.replace('+', " ")));
        } else {
            map.insert(percent_decode(pair), String::new());
        }
    }
    map
}

fn is_secret_env_key(key: &str) -> bool {
    let u = key.to_ascii_uppercase();
    u.contains("KEY")
        || u.contains("TOKEN")
        || u.contains("SECRET")
        || u.contains("PASSWORD")
        || u.contains("PASS")
        || u.contains("AUTH")
        || u.contains("CREDENTIAL")
}

fn looks_like_plaintext_secret(val: &str) -> bool {
    let t = val.trim();
    t.starts_with("sk-")
        || t.starts_with("ghp_")
        || t.starts_with("gho_")
        || t.starts_with("npm_")
        || t.starts_with("AKIA")
        || t.to_ascii_lowercase().starts_with("bearer ")
}

fn validate_url_static_ssrf(url: &str, allow_loopback: bool) -> Result<(), String> {
    let trimmed = url.trim();
    let rest = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .ok_or_else(|| format!("Invalid URL scheme '{}': must be http:// or https://", url))?;
    let authority = rest.split('/').next().unwrap_or("");
    let host = match authority.rsplit_once(':') {
        Some((h, _)) if !h.contains(']') || authority.starts_with('[') => {
            h.trim_matches(&['[', ']'][..])
        }
        _ => authority.trim_matches(&['[', ']'][..]),
    };
    if host.is_empty() {
        return Err(format!("Empty host in URL '{}'", url));
    }
    let host_lc = host.to_ascii_lowercase();
    if !allow_loopback && (host_lc == "localhost" || host_lc.ends_with(".localhost")) {
        return Err("SSRF Guard: loopback hostname is forbidden".to_string());
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        match ip {
            std::net::IpAddr::V4(v4) => {
                if v4 == std::net::Ipv4Addr::new(169, 254, 169, 254) || v4.is_link_local() {
                    return Err(
                        "SSRF Guard: cloud metadata / link-local IP is forbidden".to_string()
                    );
                }
                if v4.is_private() || v4.is_unspecified() {
                    return Err(format!("SSRF Guard: private IP '{}' is forbidden", v4));
                }
                if v4.is_loopback() && !allow_loopback {
                    return Err("SSRF Guard: loopback IP is forbidden".to_string());
                }
            }
            std::net::IpAddr::V6(v6) => {
                if v6.is_loopback() && !allow_loopback {
                    return Err("SSRF Guard: IPv6 loopback is forbidden".to_string());
                }
                if v6.is_unspecified()
                    || (v6.segments()[0] & 0xfe00) == 0xfc00
                    || (v6.segments()[0] & 0xffc0) == 0xfe80
                {
                    return Err(format!(
                        "SSRF Guard: private/link-local IPv6 '{}' is forbidden",
                        v6
                    ));
                }
            }
        }
    }
    Ok(())
}

impl ProductionGatewayContext {
    pub(crate) fn handle_get(
        &self,
        path: &str,
        query: Option<&str>,
        headers: &HashMap<String, String>,
    ) -> (u16, String, Vec<(String, String)>, Vec<u8>) {
        match path {
            "/healthz" => self.json_response(
                200,
                json!({
                    "status": "healthy",
                    "timestamp": now_epoch_secs(),
                    "platform": "omni-mcp",
                    "runtime": "rust"
                }),
            ),
            "/readyz" => {
                let server_count = self.cache.server_count();
                self.json_response(
                    200,
                    json!({
                        "status": "ready",
                        "cache_servers": server_count as i64,
                        "db_connected": true,
                        "runtime": "rust",
                        "timestamp": now_epoch_secs()
                    }),
                )
            }
            "/metrics" => {
                let uptime = (now_epoch_secs() - self.metrics.start_time).max(0.0);
                let servers = self.cache.list_servers();
                let total_servers = servers.len();
                let total_tools: usize = servers.iter().map(|s| s.tools.len()).sum();
                let (cache_hits, cache_misses) = self.gateway.semantic_cache.stats();
                let open_circuits = self.gateway.circuit_breaker.open_circuits_count();
                let (p50_us, p95_us, p99_us) =
                    self.gateway.translator.mesh_client.latency_percentiles_us();

                let metrics_payload = format!(
                    "# HELP omni_uptime_seconds Total runtime in seconds\n\
                     # TYPE omni_uptime_seconds gauge\n\
                     omni_uptime_seconds {:.2}\n\
                     # HELP omni_requests_total Total HTTP requests received\n\
                     # TYPE omni_requests_total counter\n\
                     omni_requests_total {}\n\
                     # HELP omni_errors_total Total errors encountered\n\
                     # TYPE omni_errors_total counter\n\
                     omni_errors_total {}\n\
                     # HELP omni_rate_limit_hits_total Rate limit throttle count\n\
                     # TYPE omni_rate_limit_hits_total counter\n\
                     omni_rate_limit_hits_total {}\n\
                     # HELP omni_servers_total Active virtual MCP servers\n\
                     # TYPE omni_servers_total gauge\n\
                     omni_servers_total {}\n\
                     # HELP omni_tools_total Registered tools across all servers\n\
                     # TYPE omni_tools_total gauge\n\
                     omni_tools_total {}\n\
                     # HELP omni_semantic_cache_hits_total Semantic response cache hits\n\
                     # TYPE omni_semantic_cache_hits_total counter\n\
                     omni_semantic_cache_hits_total {}\n\
                     # HELP omni_semantic_cache_misses_total Semantic response cache misses\n\
                     # TYPE omni_semantic_cache_misses_total counter\n\
                     omni_semantic_cache_misses_total {}\n\
                     # HELP omni_circuit_breaker_open_total Downstream services with open circuit breakers\n\
                     # TYPE omni_circuit_breaker_open_total gauge\n\
                     omni_circuit_breaker_open_total {}\n\
                     # HELP omni_downstream_latency_p50_microseconds Downstream p50 latency in microseconds\n\
                     # TYPE omni_downstream_latency_p50_microseconds gauge\n\
                     omni_downstream_latency_p50_microseconds {}\n\
                     # HELP omni_downstream_latency_p95_microseconds Downstream p95 latency in microseconds\n\
                     # TYPE omni_downstream_latency_p95_microseconds gauge\n\
                     omni_downstream_latency_p95_microseconds {}\n\
                     # HELP omni_downstream_latency_p99_microseconds Downstream p99 latency in microseconds\n\
                     # TYPE omni_downstream_latency_p99_microseconds gauge\n\
                     omni_downstream_latency_p99_microseconds {}\n",
                    uptime,
                    self.metrics.requests_total.load(Ordering::Relaxed),
                    self.metrics.errors_total.load(Ordering::Relaxed),
                    self.metrics.rate_limit_hits_total.load(Ordering::Relaxed),
                    total_servers,
                    total_tools,
                    cache_hits,
                    cache_misses,
                    open_circuits,
                    p50_us,
                    p95_us,
                    p99_us,
                );
                (
                    200,
                    "text/plain; version=0.0.4; charset=utf-8".to_string(),
                    Vec::new(),
                    metrics_payload.into_bytes(),
                )
            }
            "/api/stats" => {
                let servers = self.registry.list_servers(None);
                let total_tools: usize = servers.iter().map(|s| s.tools.len()).sum();
                let enabled_tools: usize = servers.iter().map(|s| s.enabled_tools_count()).sum();
                let (hits, misses) = self.gateway.semantic_cache.stats();
                let (p50_us, _p95_us, p99_us) =
                    self.gateway.translator.mesh_client.latency_percentiles_us();

                self.json_response(
                    200,
                    json!({
                        "total_servers": servers.len() as i64,
                        "total_tools": total_tools as i64,
                        "enabled_tools": enabled_tools as i64,
                        "disabled_tools": (total_tools.saturating_sub(enabled_tools)) as i64,
                        "platform": "omni-mcp",
                        "runtime": "rust",
                        "version": env!("CARGO_PKG_VERSION"),
                        "cache_hits": hits as i64,
                        "cache_misses": misses as i64,
                        "latency_p50_us": p50_us as i64,
                        "latency_p99_us": p99_us as i64,
                    }),
                )
            }
            "/api/servers" => {
                let servers = self.registry.list_servers(None);
                let mut out = Vec::with_capacity(servers.len());
                for s in servers {
                    out.push(json!({
                        "name": s.name.clone(),
                        "displayName": s.display_name.clone(),
                        "description": s.description.clone(),
                        "owningTeam": s.owning_team.clone(),
                        "protocol": s.protocol.as_str(),
                        "status": s.status.as_str(),
                        "version": s.version as i64,
                        "toolsCount": s.tools.len() as i64,
                        "enabledToolsCount": s.enabled_tools_count() as i64,
                    }));
                }
                self.json_response(200, json!({"servers": JsonValue::Array(out)}))
            }
            "/api/diffs" => {
                let diffs = self.registry.list_pending_diffs(None);
                let mut out = Vec::with_capacity(diffs.len());
                for d in diffs {
                    let changes: Vec<JsonValue> = d
                        .changes_summary
                        .iter()
                        .map(|c| JsonValue::from(c.as_str()))
                        .collect();
                    out.push(json!({
                        "diffId": d.diff_id.clone(),
                        "serverName": d.server_name.clone(),
                        "toolName": d.tool_name.clone(),
                        "prevVersion": d.previous_version as i64,
                        "proposedVersion": d.proposed_version as i64,
                        "changes": JsonValue::Array(changes),
                        "proposedBy": d.proposed_by.clone(),
                        "createdAt": d.created_at,
                        "beforeState": d.before_state.clone(),
                        "afterState": d.after_state.clone(),
                    }));
                }
                self.json_response(200, json!({"diffs": JsonValue::Array(out)}))
            }
            _ if (path.starts_with("/api/servers/external/")
                || path.starts_with("/api/v1/servers/external/"))
                && path.ends_with("/inspect") =>
            {
                let name_part = path
                    .strip_prefix("/api/servers/external/")
                    .or_else(|| path.strip_prefix("/api/v1/servers/external/"))
                    .unwrap();
                let raw_name = name_part.strip_suffix("/inspect").unwrap_or(name_part).trim();
                let raw_name = percent_decode(raw_name);
                let server_name = if raw_name.starts_with("external-mcp:")
                    || self.registry.get_server(&raw_name).is_some()
                {
                    raw_name
                } else {
                    format!("external-mcp:{}", raw_name)
                };

                let server = match self.registry.get_server(&server_name) {
                    Some(s) => s,
                    None => {
                        return self.json_response(
                            404,
                            json!({"error": format!("Server '{}' not found", server_name)}),
                        )
                    }
                };

                match self.upstream_manager.inspect(&server) {
                    Ok(inspection) => self.json_response(200, inspection),
                    Err(e) => self.json_response(500, json!({"error": e})),
                }
            }
            _ if path.starts_with("/api/servers/") => {
                let raw_name = path.trim_start_matches("/api/servers/").trim();
                let server_name = percent_decode(raw_name);
                let server = self.registry.get_server(&server_name).or_else(|| {
                    if !server_name.starts_with("external-mcp:") {
                        self.registry.get_server(&format!("external-mcp:{}", server_name))
                    } else {
                        None
                    }
                });
                let server = match server {
                    Some(s) => s,
                    None => {
                        return self.json_response(
                            404,
                            json!({"error": format!("Server '{}' not found", server_name)}),
                        )
                    }
                };
                let mut tools_out = JsonValue::object();
                for (t_name, t) in &server.tools {
                    tools_out.insert(
                        t_name.clone(),
                        json!({
                            "name": t.name.clone(),
                            "description": t.description.clone(),
                            "inputSchema": t.input_schema.clone(),
                            "status": t.status.as_str(),
                            "version": t.version as i64,
                            "protocol": t.protocol.as_str(),
                            "downstreamEndpoint": t.downstream_endpoint.clone(),
                        }),
                    );
                }
                let history_out: Vec<JsonValue> =
                    server.history.iter().map(|h| h.to_json()).collect();
                self.json_response(
                    200,
                    json!({
                        "name": server.name.clone(),
                        "displayName": server.display_name.clone(),
                        "description": server.description.clone(),
                        "owningTeam": server.owning_team.clone(),
                        "protocol": server.protocol.as_str(),
                        "status": server.status.as_str(),
                        "version": server.version as i64,
                        "tools": tools_out,
                        "history": JsonValue::Array(history_out),
                    }),
                )
            }
            "/.well-known/oauth-protected-resource" => {
                let resource_url = format!("http://{}:{}", self.config.host, self.config.port);
                let auth_srv = format!("{}/oauth", resource_url);
                self.json_response(
                    200,
                    json!({
                        "resource": resource_url,
                        "authorization_servers": [auth_srv],
                        "scopes_supported": ["agent", "operator", "approver", "admin"],
                        "bearer_methods_supported": ["header"],
                        "client_id_metadata_document_supported": true,
                        "resource_documentation": "https://github.com/Rangow4562/omni-mcp-gateway"
                    }),
                )
            }
            "/api/events" | "/api/v1/events" => {
                let (cache_hits, cache_misses) = self.gateway.semantic_cache.stats();
                let open_circuits = self.gateway.circuit_breaker.open_circuits_count();
                let servers = self.registry.list_servers(None);
                let groups = self.registry.list_tool_groups();
                let payload = json!({
                    "type": "gateway.snapshot",
                    "timestamp": crate::logging::now_epoch_secs(),
                    "stats": {
                        "cache_hits": cache_hits as i64,
                        "cache_misses": cache_misses as i64,
                        "open_circuits": open_circuits as i64
                    },
                    "servers_count": servers.len() as i64,
                    "groups_count": groups.len() as i64,
                    "audit_head": self.audit_store.get_head_hash()
                });
                let sse_body = format!(
                    "event: gateway.snapshot\ndata: {}\n\n",
                    payload.to_string_compact()
                );
                (
                    200,
                    "text/event-stream; charset=utf-8".to_string(),
                    vec![
                        ("Cache-Control".to_string(), "no-cache".to_string()),
                        ("X-Accel-Buffering".to_string(), "no".to_string()),
                    ],
                    sse_body.into_bytes(),
                )
            }
            "/openapi.json" | "/api/openapi.json" => self.json_response(
                200,
                json!({
                    "openapi": "3.0.3",
                    "info": {
                        "title": "Omni MCP Gateway API",
                        "version": env!("CARGO_PKG_VERSION"),
                        "description": "Governed enterprise MCP proxy and Code Mode sandbox execution runtime"
                    },
                    "paths": {
                        "/omni/mcp": {
                            "post": {
                                "summary": "Streamable HTTP JSON-RPC 2.0 MCP interface",
                                "responses": {"200": {"description": "MCP response"}}
                            }
                        },
                        "/api/v1/servers": {
                            "get": {
                                "summary": "List registered MCP services",
                                "responses": {"200": {"description": "List of active microservices"}}
                            }
                        },
                        "/api/v1/stats": {
                            "get": {
                                "summary": "Operational metrics and circuit status",
                                "responses": {"200": {"description": "System telemetry"}}
                            }
                        },
                        "/api/v1/codemode/eval": {
                            "post": {
                                "summary": "Execute sandboxed Code Mode workflow",
                                "responses": {"200": {"description": "Execution result"}}
                            }
                        }
                    }
                }),
            ),
            "/sse" | "/omni/sse" => {
                #[cfg(not(feature = "legacy-sse"))]
                {
                    (
                        410,
                        "application/json".to_string(),
                        vec![
                            ("Sunset".to_string(), "2026-07-28".to_string()),
                            ("Deprecation".to_string(), "true".to_string()),
                        ],
                        json!({
                            "error": "Gone: HTTP+SSE transport has been deprecated and removed in MCP 2026-07-28. Use Streamable HTTP on /omni/mcp or enable the 'legacy-sse' cargo feature."
                        })
                        .to_string_compact()
                        .into_bytes(),
                    )
                }
                #[cfg(feature = "legacy-sse")]
                {
                    let sse_body = "event: endpoint\ndata: /omni/mcp\n\nevent: status\ndata: {\"status\":\"connected\",\"version\":\"5.0.0\",\"runtime\":\"rust\"}\n\n";
                    (
                        200,
                        "text/event-stream; charset=utf-8".to_string(),
                        vec![
                            ("Cache-Control".to_string(), "no-cache".to_string()),
                            ("Connection".to_string(), "keep-alive".to_string()),
                            ("Access-Control-Allow-Origin".to_string(), "*".to_string()),
                            (
                                "Deprecation".to_string(),
                                "@deprecated: use Streamable HTTP on /omni/mcp".to_string(),
                            ),
                            ("Sunset".to_string(), "2026-12-31".to_string()),
                        ],
                        sse_body.as_bytes().to_vec(),
                    )
                }
            }
            "/omni/mcp" => {
                let supported: Vec<JsonValue> = crate::models::SUPPORTED_MCP_PROTOCOL_VERSIONS
                    .iter()
                    .map(|v| JsonValue::from(*v))
                    .collect();
                self.json_response(
                    200,
                    json!({
                        "endpoint": "/omni/mcp",
                        "protocol": "Streamable HTTP",
                        "server": "omni-mcp-gateway",
                        "version": env!("CARGO_PKG_VERSION"),
                        "supported_versions": JsonValue::Array(supported),
                        "capabilities": {
                            "tools": {"listChanged": true},
                            "resources": {"subscribe": false, "listChanged": true},
                            "prompts": {"listChanged": true},
                            "extensions": {}
                        }
                    }),
                )
            }
            _ if path.ends_with("/mcp") => {
                let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
                if parts.len() == 2 && parts[1] == "mcp" {
                    let service_name = percent_decode(parts[0]);
                    let srv = self.registry.get_server(&service_name).or_else(|| {
                        if !service_name.starts_with("external-mcp:") {
                            self.registry.get_server(&format!("external-mcp:{}", service_name))
                        } else {
                            None
                        }
                    });
                    if let Some(srv) = srv {
                        return self.json_response(
                            200,
                            json!({
                                "service": srv.name,
                                "endpoint": path,
                                "protocol": "Streamable HTTP",
                                "status": srv.status.as_str(),
                                "tools_count": srv.tools.len() as i64,
                            }),
                        );
                    } else {
                        return self.json_response(
                            404,
                            json!({
                                "error": format!("Service '{}' not found", service_name)
                            }),
                        );
                    }
                }
                self.serve_static_or_index(path)
            }
            "/api/traces" => {
                let spans_guard = self.traces.read().unwrap();
                let spans: Vec<JsonValue> = spans_guard.iter().map(|s| s.to_json()).collect();
                self.json_response(
                    200,
                    json!({
                        "spans": JsonValue::Array(spans),
                        "total": spans_guard.len() as i64
                    }),
                )
            }
            "/api/audit" => {
                if !self.is_authorized(headers) {
                    return self.unauthorized_response("/api/audit");
                }
                let records = self.audit_store.list_records(Some(100));
                let recs_json: Vec<JsonValue> = records.iter().map(|r| r.to_json()).collect();
                let (verified, _) = self.audit_store.verify_chain();
                self.json_response(
                    200,
                    json!({
                        "head_hash": self.audit_store.get_head_hash(),
                        "total_records": recs_json.len() as i64,
                        "chain_verified": verified,
                        "records": JsonValue::Array(recs_json)
                    }),
                )
            }
            "/api/audit/verify" => {
                if !self.is_authorized(headers) {
                    return self.unauthorized_response("/api/audit/verify");
                }
                let (verified, count_or_idx) = self.audit_store.verify_chain();
                let (ext_valid, ext_count) = match &self.external_audit_sink {
                    Some(sink) => {
                        let (v, c) = sink.verify_integrity();
                        (Some(v), Some(c as i64))
                    }
                    None => (None, None),
                };
                if verified && ext_valid.unwrap_or(true) {
                    self.json_response(
                        200,
                        json!({
                            "verified": true,
                            "records_checked": count_or_idx as i64,
                            "algorithm": "SHA-256 (FIPS 180-4)",
                            "head_hash": self.audit_store.get_head_hash(),
                            "storage_backend": self.storage_backend_name.clone(),
                            "external_sink_verified": ext_valid.map(JsonValue::Bool).unwrap_or(JsonValue::Null),
                            "external_sink_records": ext_count.map(JsonValue::from).unwrap_or(JsonValue::Null),
                        }),
                    )
                } else {
                    self.json_response(
                        409,
                        json!({
                            "verified": false,
                            "corrupted_at_index": count_or_idx as i64,
                            "reason": "Hash mismatch in cryptographic Merkle chain",
                            "storage_backend": self.storage_backend_name.clone(),
                            "external_sink_verified": ext_valid.map(JsonValue::Bool).unwrap_or(JsonValue::Null),
                        }),
                    )
                }
            }
            "/api/codemode/types" => {
                let gen = self.gateway.generate_codemode_types(None, None);
                self.json_response(200, gen.to_json())
            }
            "/api/codemode/suspended" => {
                let qparams = parse_query_params(query);
                let include_preview = qparams
                    .get("include_preview")
                    .is_some_and(|v| v == "true" || v == "1");
                let frames = self.gateway.list_suspended_codemode_frames();
                let mut frames_json = Vec::with_capacity(frames.len());
                for f in frames {
                    let mut obj = f.to_json();
                    if let Some(entries) = obj.as_object_mut() {
                        entries.retain(|(k, _)| {
                            k != "script_source"
                                && k != "serialized_scope"
                                && k != "pending_arguments"
                                && k != "frame_hmac"
                        });
                        entries.push(("redacted".to_string(), JsonValue::Bool(true)));
                        if include_preview {
                            entries.push((
                                "script_preview".to_string(),
                                JsonValue::from(PIIRedactor::redact_text(&f.script_source)),
                            ));
                            entries.push((
                                "scope_preview".to_string(),
                                PIIRedactor::redact_payload(&f.serialized_scope),
                            ));
                            entries.push((
                                "pending_arguments_preview".to_string(),
                                PIIRedactor::redact_payload(&f.pending_arguments),
                            ));
                        }
                    }
                    frames_json.push(obj);
                }
                self.json_response(
                    200,
                    json!({
                        "total": frames_json.len() as i64,
                        "suspended_frames": JsonValue::Array(frames_json)
                    }),
                )
            }
            "/api/callbacks" => {
                let callbacks: Vec<JsonValue> = self
                    .upstream_manager
                    .callback_hub
                    .list_pending_callbacks()
                    .into_iter()
                    .map(|c| c.to_json())
                    .collect();
                self.json_response(
                    200,
                    json!({
                        "total": callbacks.len() as i64,
                        "callbacks": JsonValue::Array(callbacks)
                    }),
                )
            }
            "/api/telemetry/otlp/status" => {
                if let Some(ref worker) = self.otlp_worker {
                    self.json_response(200, worker.status().to_json())
                } else {
                    let mut hist = crate::telemetry::LatencyHistogram::default();
                    if let Ok(guard) = self.traces.read() {
                        for span in guard.iter() {
                            hist.record(span.duration_us, &span.status);
                        }
                    }
                    let (p50, p95, p99) = hist.percentiles();
                    self.json_response(
                        200,
                        json!({
                            "endpoint": JsonValue::Null,
                            "running": false,
                            "flush_interval_ms": self.config.otlp_flush_interval_ms as i64,
                            "queued_spans": 0i64,
                            "total_exported": 0i64,
                            "total_failed": 0i64,
                            "latency_quantiles_us": {
                                "p50": p50 as i64,
                                "p95": p95 as i64,
                                "p99": p99 as i64,
                            },
                            "storage_backend": self.storage_backend_name.clone(),
                        }),
                    )
                }
            }
            "/api/oauth/callback" => {
                let qparams = parse_query_params(query);
                let state = qparams.get("state").cloned().unwrap_or_default();
                let code = qparams.get("code").cloned().unwrap_or_default();
                let direct_token = qparams.get("access_token").cloned();
                if state.is_empty() {
                    return self.json_response(400, json!({"error": "Missing OAuth 'state' parameter"}));
                }
                let flow = {
                    let mut guard = self.oauth_pending_flows.write().unwrap();
                    guard.remove(&state)
                };
                let Some(flow) = flow else {
                    return self.json_response(400, json!({"error": "Invalid or expired OAuth state"}));
                };
                if now_epoch_secs() - flow.created_at > 600.0 {
                    return self.json_response(400, json!({"error": "OAuth PKCE state expired"}));
                }

                let token_store = UpstreamOAuthTokenStore::new(&self.config.secrets_dir, None);
                let token_record = if let Some(tok) = direct_token.filter(|s| !s.is_empty()) {
                    OAuthTokenRecord {
                        access_token: tok,
                        refresh_token: qparams.get("refresh_token").cloned(),
                        expires_at: Some(now_epoch_secs() + 3600.0),
                        token_endpoint: if flow.token_endpoint.is_empty() {
                            None
                        } else {
                            Some(flow.token_endpoint.clone())
                        },
                        client_id: Some(flow.client_id.clone()),
                        scope: Some(flow.scope.clone()),
                    }
                } else if !code.is_empty() && !flow.token_endpoint.is_empty() {
                    if let Err(e) = crate::upstream::http::validate_upstream_url_and_resolve(
                        &flow.token_endpoint,
                        self.config.allow_loopback_upstream,
                    ) {
                        return self.json_response(403, json!({"error": e}));
                    }
                    let form_body = UpstreamOAuthTokenStore::build_auth_code_exchange_body(
                        &flow.client_id,
                        &code,
                        &flow.redirect_uri,
                        &flow.code_verifier,
                    );
                    let mut hdrs = HashMap::new();
                    hdrs.insert(
                        "Content-Type".to_string(),
                        "application/x-www-form-urlencoded".to_string(),
                    );
                    hdrs.insert("Accept".to_string(), "application/json".to_string());
                    let http = crate::data_plane::HttpDownstreamClient::new();
                    match http.post_json(&flow.token_endpoint, form_body.as_bytes(), Some(&hdrs)) {
                        Ok(bytes) => {
                            let parsed = String::from_utf8(bytes)
                                .ok()
                                .and_then(|s| JsonValue::parse(&s).ok())
                                .unwrap_or_else(JsonValue::object);
                            let Some(access) = parsed
                                .get("access_token")
                                .and_then(|v| v.as_str())
                                .map(String::from)
                            else {
                                return self.json_response(
                                    502,
                                    json!({"error": "OAuth token endpoint did not return access_token"}),
                                );
                            };
                            OAuthTokenRecord {
                                access_token: access,
                                refresh_token: parsed
                                    .get("refresh_token")
                                    .and_then(|v| v.as_str())
                                    .map(String::from),
                                expires_at: parsed
                                    .get("expires_in")
                                    .and_then(|v| v.as_f64())
                                    .map(|e| now_epoch_secs() + e),
                                token_endpoint: Some(flow.token_endpoint.clone()),
                                client_id: Some(flow.client_id.clone()),
                                scope: Some(flow.scope.clone()),
                            }
                        }
                        Err(e) => {
                            return self.json_response(
                                502,
                                json!({"error": format!("OAuth code exchange failed: {}", e)}),
                            );
                        }
                    }
                } else {
                    return self.json_response(
                        400,
                        json!({"error": "Missing OAuth 'code' or token_endpoint for exchange"}),
                    );
                };

                if let Err(e) = token_store.save_token_record(&flow.server_name, &token_record) {
                    return self.json_response(
                        500,
                        json!({"error": format!("Failed to seal OAuth token: {}", e)}),
                    );
                }
                self.event_bus.publish(
                    "oauth.connected",
                    json!({"server_name": flow.server_name.clone()}),
                );
                self.json_response(
                    200,
                    json!({
                        "connected": true,
                        "server_name": flow.server_name,
                        "encryption": if cfg!(feature = "tls-rustls") { "ChaCha20-Poly1305" } else { "SHA256-CTR-HMAC" }
                    }),
                )
            }
            _ if path.starts_with("/api/oauth/authorize/") => {
                let raw_server = path
                    .strip_prefix("/api/oauth/authorize/")
                    .unwrap_or("")
                    .trim();
                let server_name = percent_decode(raw_server);
                if server_name.is_empty() {
                    return self.json_response(400, json!({"error": "Server name is required"}));
                }
                let qparams = parse_query_params(query);
                let authorize_endpoint = qparams
                    .get("authorize_endpoint")
                    .cloned()
                    .unwrap_or_else(|| "https://auth.example.com/oauth/authorize".to_string());
                let token_endpoint = qparams.get("token_endpoint").cloned().unwrap_or_default();
                if !token_endpoint.is_empty() {
                    if let Err(e) = validate_url_static_ssrf(
                        &token_endpoint,
                        self.config.allow_loopback_upstream,
                    ) {
                        return self.json_response(403, json!({"error": e}));
                    }
                }
                let client_id = qparams
                    .get("client_id")
                    .cloned()
                    .unwrap_or_else(|| "omni-mcp-gateway".to_string());
                let scope = qparams
                    .get("scope")
                    .cloned()
                    .unwrap_or_else(|| "mcp:tools".to_string());
                let redirect_uri = qparams.get("redirect_uri").cloned().unwrap_or_else(|| {
                    format!(
                        "http://{}:{}/api/v1/oauth/callback",
                        self.config.host, self.config.port
                    )
                });

                let (verifier, challenge) = UpstreamOAuthTokenStore::generate_pkce_pair();
                let state = format!("st_{}", generate_ulid());
                let auth_url = UpstreamOAuthTokenStore::build_authorization_url(
                    &authorize_endpoint,
                    &client_id,
                    &redirect_uri,
                    &scope,
                    &state,
                    &challenge,
                );
                self.oauth_pending_flows.write().unwrap().insert(
                    state.clone(),
                    OAuthPendingFlow {
                        server_name: server_name.clone(),
                        code_verifier: verifier,
                        redirect_uri: redirect_uri.clone(),
                        token_endpoint,
                        client_id,
                        scope,
                        created_at: now_epoch_secs(),
                    },
                );
                self.json_response(
                    200,
                    json!({
                        "server_name": server_name,
                        "authorization_url": auth_url,
                        "state": state,
                        "code_challenge": challenge,
                        "code_challenge_method": "S256",
                        "redirect_uri": redirect_uri,
                    }),
                )
            }
            "/api/groups" | "/api/v1/groups" => {
                let groups: Vec<JsonValue> = self
                    .registry
                    .list_tool_groups()
                    .into_iter()
                    .map(|g| g.to_json())
                    .collect();
                self.json_response(
                    200,
                    json!({
                        "total": groups.len() as i64,
                        "groups": JsonValue::Array(groups)
                    }),
                )
            }
            _ => self.serve_static_or_index(path),
        }
    }

    pub(crate) fn handle_delete(
        &self,
        path: &str,
        _headers: &HashMap<String, String>,
    ) -> (u16, String, Vec<(String, String)>, Vec<u8>) {
        if let Some(exec_id) = path
            .strip_prefix("/api/codemode/suspended/")
            .or_else(|| path.strip_prefix("/api/v1/codemode/suspended/"))
        {
            let exec_id = percent_decode(exec_id.trim());
            if exec_id.is_empty() {
                return self.json_response(400, json!({"error": "execution_id is required"}));
            }
            let removed = self.gateway.delete_suspended_codemode_frame(&exec_id);
            if removed.is_some() {
                self.event_bus.publish(
                    "codemode.suspended_cancelled",
                    json!({"execution_id": exec_id.clone()}),
                );
            }
            return self.json_response(
                200,
                json!({"success": removed.is_some(), "execution_id": exec_id}),
            );
        }
        if let Some(slug) = path
            .strip_prefix("/api/groups/")
            .or_else(|| path.strip_prefix("/api/v1/groups/"))
        {
            let slug = percent_decode(slug.trim());
            if slug.is_empty() {
                return self.json_response(400, json!({"error": "Group slug is required"}));
            }
            let removed = self.registry.delete_tool_group(&slug);
            return self.json_response(200, json!({"success": removed.is_some(), "slug": slug}));
        }
        if (path.starts_with("/api/servers/external/")
            || path.starts_with("/api/v1/servers/external/"))
            && !path.ends_with("/inspect")
            && !path.ends_with("/rediscover")
            && path != "/api/servers/external/register"
            && path != "/api/v1/servers/external/register"
        {
            let name_part = path
                .strip_prefix("/api/servers/external/")
                .or_else(|| path.strip_prefix("/api/v1/servers/external/"))
                .unwrap();
            let raw_name = name_part
                .strip_suffix("/deregister")
                .unwrap_or(name_part)
                .trim();
            if raw_name.is_empty() {
                return self.json_response(400, json!({"error": "Server name is required"}));
            }
            let raw_name = percent_decode(raw_name);
            let server_name = if raw_name.starts_with("external-mcp:")
                || self.registry.get_server(&raw_name).is_some()
            {
                raw_name
            } else {
                format!("external-mcp:{}", raw_name)
            };

            self.upstream_manager.deregister_server(&server_name);
            let removed = self.registry.remove_server(&server_name);
            self.cache.refresh_now();
            self.gateway.semantic_cache.invalidate_service(&server_name);
            return self.json_response(200, json!({"success": removed.is_some()}));
        }

        self.json_response(404, json!({"error": format!("Route '{}' not found", path)}))
    }

    pub(crate) fn handle_rest_post(
        &self,
        path: &str,
        headers: &HashMap<String, String>,
        actor: &str,
        token: &str,
        traceparent: Option<&str>,
        parsed_body: Result<JsonValue, String>,
    ) -> (u16, String, Vec<(String, String)>, Vec<u8>) {
        let body = parsed_body.unwrap_or_else(|_| JsonValue::object());

        // 3. Control Plane Management APIs
        match path {
            "/api/servers/external/register" | "/api/v1/servers/external/register" => {
                let raw_name = body
                    .get("name")
                    .or_else(|| body.get("slug"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .trim();
                let raw_name = percent_decode(raw_name);
                let raw_name = raw_name.trim();
                if raw_name.is_empty() {
                    return self.json_response(400, json!({"error": "Field 'name' is required"}));
                }
                let server_name = if raw_name.starts_with("external-mcp:") {
                    raw_name.to_string()
                } else {
                    format!("external-mcp:{}", raw_name)
                };

                // Check identifier collision for server name
                if let Err(coll) = self.registry.check_identifier_collisions(&server_name, &[]) {
                    return self.json_response(
                        409,
                        json!({
                            "error": "IDENTIFIER_COLLISION",
                            "sanitized": coll.sanitized,
                            "first_raw": coll.first_raw,
                            "second_raw": coll.second_raw
                        }),
                    );
                }

                let display_name = body
                    .get("display_name")
                    .or_else(|| body.get("displayName"))
                    .and_then(|x| x.as_str())
                    .unwrap_or(raw_name);
                let description = body
                    .get("description")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let owner_team = body
                    .get("owner_team")
                    .or_else(|| body.get("owningTeam"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("platform");

                let (transport_type, transport_val) = match body.get("transport") {
                    Some(v) if v.is_object() => {
                        let tt = v
                            .get("transport")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        (tt, v.clone())
                    }
                    Some(v) if v.is_string() => {
                        let tt = v.as_str().unwrap_or("").to_string();
                        let mut cfg = if let Some(sub) = body.get(&tt).and_then(|x| x.as_object()) {
                            let mut map = body.clone();
                            for (k, val) in sub {
                                map.insert(k.clone(), val.clone());
                            }
                            map
                        } else {
                            body.clone()
                        };
                        cfg.insert("transport", tt.clone());
                        (tt, cfg)
                    }
                    _ => {
                        return self
                            .json_response(400, json!({"error": "Field 'transport' is required"}))
                    }
                };

                let transport_config = match transport_type.as_str() {
                    "stdio" => {
                        let command = match transport_val.get("command").and_then(|x| x.as_str()) {
                            Some(c) if !c.trim().is_empty() => c.trim().to_string(),
                            _ => return self.json_response(400, json!({"error": "stdio transport requires non-empty 'command'"})),
                        };

                        let args: Vec<String> = transport_val
                            .get("args")
                            .and_then(|a| a.as_array())
                            .map(|arr| arr.iter().filter_map(|x| x.as_str()).map(|s| s.to_string()).collect())
                            .unwrap_or_default();

                        let mut env_refs = std::collections::BTreeMap::new();
                        if let Some(obj) = transport_val.get("env_refs").and_then(|e| e.as_object()) {
                            for (k, v) in obj {
                                if let Some(v_str) = v.as_str() {
                                    if let Err(e) = crate::upstream::secrets::validate_secret_ref_syntax(v_str) {
                                        return self.json_response(400, json!({"error": e}));
                                    }
                                    env_refs.insert(k.clone(), v_str.to_string());
                                }
                            }
                        }

                        let cwd = transport_val.get("cwd").and_then(|x| x.as_str()).map(|s| s.to_string());

                        let canonical_cmd = match crate::upstream::stdio::check_stdio_command_allowlist(&command, &self.config.stdio_allowlist) {
                            Ok(canon) => canon.to_string_lossy().to_string(),
                            Err(e) => return self.json_response(403, json!({"error": e})),
                        };

                        crate::models::UpstreamTransportConfig::Stdio {
                            command,
                            canonical_command: canonical_cmd,
                            args,
                            env_refs,
                            cwd,
                        }
                    }
                    "streamable_http" => {
                        let url = match transport_val.get("url").and_then(|x| x.as_str()) {
                            Some(u) if !u.trim().is_empty() => u.trim().to_string(),
                            _ => return self.json_response(400, json!({"error": "streamable_http transport requires non-empty 'url'"})),
                        };

                        let bearer_ref = match transport_val.get("bearer_ref").and_then(|x| x.as_str()) {
                            Some(b) if !b.trim().is_empty() => {
                                if let Err(e) = crate::upstream::secrets::validate_secret_ref_syntax(b) {
                                    return self.json_response(400, json!({"error": e}));
                                }
                                Some(b.to_string())
                            }
                            _ => None,
                        };

                        let mut header_refs = std::collections::BTreeMap::new();
                        if let Some(obj) = transport_val.get("header_refs").and_then(|h| h.as_object()) {
                            for (k, v) in obj {
                                if let Some(v_str) = v.as_str() {
                                    if let Err(e) = crate::upstream::secrets::validate_secret_ref_syntax(v_str) {
                                        return self.json_response(400, json!({"error": e}));
                                    }
                                    header_refs.insert(k.clone(), v_str.to_string());
                                }
                            }
                        }

                        let session_mode = match transport_val.get("session_mode").and_then(|x| x.as_str()) {
                            Some("stateless") => crate::models::SessionMode::Stateless,
                            _ => crate::models::SessionMode::Stateful,
                        };

                        if let Err(e) = crate::upstream::http::validate_upstream_url_and_resolve(&url, self.config.allow_loopback_upstream) {
                            if e.contains("-32014") {
                                return self.json_response(400, json!({"error": e, "code": -32014}));
                            } else if e.contains("SSRF Guard") {
                                return self.json_response(403, json!({"error": e}));
                            } else {
                                return self.json_response(400, json!({"error": e}));
                            }
                        }

                        crate::models::UpstreamTransportConfig::StreamableHttp {
                            url,
                            bearer_ref,
                            header_refs,
                            session_mode,
                        }
                    }
                    other => return self.json_response(400, json!({"error": format!("Unsupported transport '{}'; use 'stdio' or 'streamable_http'", other)})),
                };

                let mut server = crate::models::ServerRecord::new(
                    &server_name,
                    display_name,
                    description,
                    owner_team,
                );
                server.protocol = crate::models::ProtocolType::ExternalMcp;
                server.upstream_config = Some(transport_config);
                server.sync_interval_sec = body.get("sync_interval_sec").and_then(|x| x.as_u64());
                if let Some(pol) = body.get("upstream_policy").and_then(|x| x.as_str()) {
                    server.upstream_policy = crate::models::UpstreamPolicy::from_str(pol);
                }
                if let Some(spol) = body
                    .get("sampling_elicitation_policy")
                    .and_then(|x| x.as_str())
                {
                    server.sampling_elicitation_policy =
                        crate::models::SamplingElicitationPolicy::from_str(spol);
                }

                // Initial tool, resource & prompt discovery
                let initial_tools = match self.upstream_manager.fetch_tools(&server) {
                    Ok(tools) => {
                        if let Err(coll) = self
                            .registry
                            .check_identifier_collisions(&server_name, &tools)
                        {
                            return self.json_response(
                                409,
                                json!({
                                    "error": "IDENTIFIER_COLLISION",
                                    "sanitized": coll.sanitized,
                                    "first_raw": coll.first_raw,
                                    "second_raw": coll.second_raw
                                }),
                            );
                        }
                        tools
                    }
                    Err(_) => Vec::new(),
                };
                if let Ok(res) = self.upstream_manager.fetch_resources(&server) {
                    server.external_resources = res;
                }
                if let Ok(pr) = self.upstream_manager.fetch_prompts(&server) {
                    server.external_prompts = pr;
                }

                let registered = self.registry.register_server(server);
                if !initial_tools.is_empty() {
                    let _ = self
                        .registry
                        .apply_external_sync(&server_name, initial_tools);
                }
                self.cache.refresh_now();

                self.json_response(
                    200,
                    json!({
                        "success": true,
                        "server": registered.to_json()
                    }),
                )
            }
            _ if (path.starts_with("/api/servers/external/")
                || path.starts_with("/api/v1/servers/external/"))
                && path.ends_with("/rediscover") =>
            {
                let name_part = path
                    .strip_prefix("/api/servers/external/")
                    .or_else(|| path.strip_prefix("/api/v1/servers/external/"))
                    .unwrap();
                let raw_name = name_part
                    .strip_suffix("/rediscover")
                    .unwrap_or(name_part)
                    .trim();
                let raw_name = percent_decode(raw_name);
                let server_name = if raw_name.starts_with("external-mcp:")
                    || self.registry.get_server(&raw_name).is_some()
                {
                    raw_name
                } else {
                    format!("external-mcp:{}", raw_name)
                };

                let server = match self.registry.get_server(&server_name) {
                    Some(s) => s,
                    None => {
                        return self.json_response(
                            404,
                            json!({"error": format!("Server '{}' not found", server_name)}),
                        )
                    }
                };

                if !self.upstream_manager.try_lock_rediscovery(&server_name) {
                    return self.json_response(
                        409,
                        json!({"error": "Rediscovery already in progress for server"}),
                    );
                }

                let fetch_res = self.upstream_manager.fetch_tools(&server);
                self.upstream_manager.unlock_rediscovery(&server_name);

                let tools = match fetch_res {
                    Ok(t) => t,
                    Err(e) => {
                        return self.json_response(
                            502,
                            json!({"error": format!("Upstream tools/list failed: {}", e)}),
                        )
                    }
                };

                if let Err(coll) = self
                    .registry
                    .check_identifier_collisions(&server_name, &tools)
                {
                    return self.json_response(
                        409,
                        json!({
                            "error": "IDENTIFIER_COLLISION",
                            "sanitized": coll.sanitized,
                            "first_raw": coll.first_raw,
                            "second_raw": coll.second_raw
                        }),
                    );
                }

                match self.registry.apply_external_sync(&server_name, tools) {
                    Ok(sync_res) => {
                        self.cache.refresh_now();
                        self.gateway.semantic_cache.invalidate_service(&server_name);
                        self.json_response(
                            200,
                            json!({
                                "success": true,
                                "added": sync_res.added as i64,
                                "updated": sync_res.updated as i64,
                                "retired": sync_res.retired as i64,
                                "unchanged": sync_res.unchanged as i64,
                                "pending_human_conflicts": sync_res.pending_human_conflicts as i64
                            }),
                        )
                    }
                    Err(e) => self.json_response(500, json!({"error": e})),
                }
            }
            _ if (path.starts_with("/api/servers/external/")
                || path.starts_with("/api/v1/servers/external/"))
                && path.ends_with("/deregister") =>
            {
                let name_part = path
                    .strip_prefix("/api/servers/external/")
                    .or_else(|| path.strip_prefix("/api/v1/servers/external/"))
                    .unwrap();
                let raw_name = name_part
                    .strip_suffix("/deregister")
                    .unwrap_or(name_part)
                    .trim();
                let raw_name = percent_decode(raw_name);
                let server_name = if raw_name.starts_with("external-mcp:")
                    || self.registry.get_server(&raw_name).is_some()
                {
                    raw_name
                } else {
                    format!("external-mcp:{}", raw_name)
                };

                self.upstream_manager.deregister_server(&server_name);
                let removed = self.registry.remove_server(&server_name);
                self.cache.refresh_now();
                self.gateway.semantic_cache.invalidate_service(&server_name);
                self.json_response(200, json!({"success": removed.is_some()}))
            }
            "/api/crawler/trigger" => {
                let res = self.crawler.run_discovery_workflow();
                self.cache.refresh_now();
                self.json_response(200, res)
            }
            "/api/tools/toggle_status" => {
                let s_name = body
                    .get("serverName")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let decoded_s_name = percent_decode(s_name);
                let t_name = body.get("toolName").and_then(|x| x.as_str()).unwrap_or("");
                let status_str = body
                    .get("status")
                    .and_then(|x| x.as_str())
                    .unwrap_or("ENABLED");
                let new_status = ToolStatus::from_str(status_str);
                let ok = self
                    .registry
                    .toggle_tool_status(&decoded_s_name, t_name, new_status);
                self.cache.refresh_now();
                self.gateway
                    .semantic_cache
                    .invalidate_service(&decoded_s_name);
                self.json_response(200, json!({"success": ok}))
            }
            "/api/servers/toggle_status" => {
                let s_name = body
                    .get("serverName")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let decoded_s_name = percent_decode(s_name);
                let status_str = body
                    .get("status")
                    .and_then(|x| x.as_str())
                    .unwrap_or("ENABLED");
                let new_status = ToolStatus::from_str(status_str);
                let ok = self
                    .registry
                    .toggle_server_status(&decoded_s_name, new_status);
                self.cache.refresh_now();
                self.json_response(200, json!({"success": ok}))
            }
            "/api/tools/propose_update" => {
                let s_name = body
                    .get("serverName")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let t_name = body.get("toolName").and_then(|x| x.as_str()).unwrap_or("");
                let new_desc = body.get("description").and_then(|x| x.as_str());
                let new_schema = body.get("inputSchema").cloned();
                let author = body
                    .get("author")
                    .and_then(|x| x.as_str())
                    .unwrap_or("engineer");

                match self
                    .registry
                    .propose_tool_update(s_name, t_name, new_desc, new_schema, author)
                {
                    Ok(diff) => {
                        let changes: Vec<JsonValue> = diff
                            .changes_summary
                            .iter()
                            .map(|c| JsonValue::from(c.as_str()))
                            .collect();
                        self.json_response(
                            200,
                            json!({
                                "diffId": diff.diff_id,
                                "changes": JsonValue::Array(changes),
                                "proposedVersion": diff.proposed_version as i64,
                            }),
                        )
                    }
                    Err(e) => self.json_response(400, json!({"error": e})),
                }
            }
            "/api/diffs/approve" => {
                let diff_id = body.get("diffId").and_then(|x| x.as_str()).unwrap_or("");
                let approver = body
                    .get("approver")
                    .and_then(|x| x.as_str())
                    .unwrap_or("service_owner");
                match self.registry.approve_diff(diff_id, approver) {
                    Ok(true) => {
                        self.cache.refresh_now();
                        self.json_response(200, json!({"success": true}))
                    }
                    Ok(false) => self.json_response(
                        404,
                        json!({"error": "Diff not found or already approved", "success": false}),
                    ),
                    Err(e) if e.contains("CONFLICT") => {
                        self.json_response(409, json!({"error": e}))
                    }
                    Err(e) => self.json_response(400, json!({"error": e})),
                }
            }
            "/api/tools/rollback" => {
                let s_name = body
                    .get("serverName")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let t_name = body.get("toolName").and_then(|x| x.as_str()).unwrap_or("");
                let target_v = body
                    .get("targetVersion")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(1) as u32;
                let ok = self.registry.rollback_tool(s_name, t_name, target_v);
                self.cache.refresh_now();
                self.gateway.semantic_cache.invalidate_service(s_name);
                self.json_response(200, json!({"success": ok}))
            }
            "/api/execute" => {
                if !self.is_authorized(headers) {
                    return self.unauthorized_response("/api/execute");
                }
                let srv = body
                    .get("server_name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let tool = body.get("tool_name").and_then(|x| x.as_str()).unwrap_or("");
                let args = body
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(JsonValue::object);
                let req = crate::models::RPCRequest::new(
                    1,
                    "tools/call",
                    json!({"name": tool, "arguments": args}),
                );
                let resp = self.gateway.handle_request_with_trace(
                    srv,
                    &req,
                    actor,
                    Some(token),
                    traceparent,
                );
                self.json_response(200, resp.to_json())
            }
            "/api/codemode/eval" => {
                let code = body.get("code").and_then(|x| x.as_str()).unwrap_or("");
                let require_approval = body
                    .get("require_mutation_approval")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false);
                let mut limits = SandboxLimits::default();
                if let Some(max_ops) = body.get("max_operations").and_then(|x| x.as_u64()) {
                    limits.max_operations = max_ops.clamp(100, 500_000);
                }
                let out = self.gateway.execute_codemode(
                    code,
                    Some(limits),
                    require_approval,
                    actor,
                    Some(token),
                    traceparent,
                );
                if let Some(ref frame) = out.suspended_frame {
                    self.event_bus.publish(
                        "codemode.suspended",
                        json!({
                            "execution_id": frame.execution_id.clone(),
                            "pending_server": frame.pending_server.clone(),
                            "pending_tool": frame.pending_tool.clone(),
                            "caller_actor": frame.caller_actor.clone(),
                        }),
                    );
                }
                self.json_response(200, out.to_json())
            }
            "/api/codemode/resume" => {
                let execution_id = body
                    .get("execution_id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let digest = body
                    .get("arguments_digest")
                    .or_else(|| body.get("pending_arguments_hash"))
                    .and_then(|x| x.as_str());
                let principal = self.authenticate(headers).unwrap_or(AuthPrincipal {
                    id: actor.to_string(),
                    role: AuthRole::Approver,
                });
                let approver = &principal.id;
                let out = self.gateway.resume_codemode(
                    execution_id,
                    approver,
                    None,
                    Some(token),
                    traceparent,
                    digest,
                );
                self.event_bus.publish(
                    "codemode.resumed",
                    json!({
                        "execution_id": execution_id,
                        "approver": approver.as_str(),
                        "status": out.status.clone(),
                    }),
                );
                self.json_response(200, out.to_json())
            }
            "/api/codemode/speculative_types" => {
                let intent = body.get("intent").and_then(|x| x.as_str()).unwrap_or("");
                let top_k = body.get("top_k").and_then(|x| x.as_u64()).unwrap_or(4) as usize;
                let threshold = body
                    .get("confidence_threshold")
                    .and_then(|x| x.as_f64())
                    .unwrap_or(0.82);
                let spec = self
                    .gateway
                    .generate_speculative_codemode_types(intent, top_k, threshold);
                self.json_response(200, spec.to_json())
            }
            "/api/codemode/simulate" => {
                let code = body.get("code").and_then(|x| x.as_str()).unwrap_or("");
                let cfg = FaultInjectionConfig::from_json(&body);
                let report = self
                    .gateway
                    .simulate_codemode(code, None, cfg, actor, Some(token));
                self.json_response(200, report.to_json())
            }
            "/api/groups" | "/api/v1/groups" => {
                let slug = body
                    .get("slug")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .trim();
                if slug.is_empty() {
                    return self.json_response(400, json!({"error": "Group 'slug' is required"}));
                }
                let mut entries = Vec::new();
                if let Some(entries_arr) = body.get("entries").and_then(|x| x.as_array()) {
                    for item in entries_arr {
                        if let Some(entry) = crate::models::ToolGroupEntry::from_json(item) {
                            entries.push(entry);
                        }
                    }
                }
                let group = crate::models::ToolGroupRecord::new(
                    slug,
                    body.get("display_name")
                        .and_then(|x| x.as_str())
                        .unwrap_or(slug),
                    body.get("description")
                        .and_then(|x| x.as_str())
                        .unwrap_or(""),
                    entries,
                );
                let saved = self.registry.save_tool_group(group);
                self.json_response(200, json!({"group": saved.to_json()}))
            }
            "/api/wasm-tools" => {
                let server_name = body
                    .get("server_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("wasm-plugins")
                    .trim();
                let tool_name = body
                    .get("tool_name")
                    .or_else(|| body.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                let description = body
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Native Wasm plugin tool");
                let wasm_b64 = body
                    .get("wasm_module_base64")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                if server_name.is_empty() || tool_name.is_empty() || wasm_b64.is_empty() {
                    return self.json_response(
                        400,
                        json!({"error": "Fields 'tool_name' and 'wasm_module_base64' are required"}),
                    );
                }
                #[cfg(feature = "sandbox-wasm")]
                if let Err(e) = crate::data_plane::codemode::decode_wasm_base64(wasm_b64) {
                    return self.json_response(
                        400,
                        json!({"error": format!("Invalid WebAssembly module payload: {}", e)}),
                    );
                }

                let input_schema = body
                    .get("input_schema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                let mut tool = ToolRecord::new(tool_name, description, input_schema);
                tool.protocol = ProtocolType::WasmPlugin;
                tool.status = ToolStatus::Enabled;
                tool.wasm_module_base64 = Some(wasm_b64.to_string());
                tool.is_mutation = body
                    .get("is_mutation")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                let mut srv = self.registry.get_server(server_name).unwrap_or_else(|| {
                    let mut s = ServerRecord::new(
                        server_name,
                        format!("{} (Wasm Plugins)", server_name),
                        "Zero-process WebAssembly plugin tools",
                        "platform",
                    );
                    s.protocol = ProtocolType::WasmPlugin;
                    s.status = ToolStatus::Enabled;
                    s
                });
                srv.status = ToolStatus::Enabled;
                srv.add_tool(tool.clone());
                self.registry.register_server(srv);
                self.cache.refresh_now();
                self.event_bus.publish(
                    "registry.wasm_tool_registered",
                    json!({"server_name": server_name, "tool_name": tool_name}),
                );
                self.json_response(
                    200,
                    json!({
                        "success": true,
                        "server_name": server_name,
                        "tool": tool.to_json()
                    }),
                )
            }
            _ if path.starts_with("/api/callbacks/") && path.ends_with("/respond") => {
                let cb_id = path
                    .strip_prefix("/api/callbacks/")
                    .and_then(|s| s.strip_suffix("/respond"))
                    .unwrap_or("")
                    .trim();
                let cb_id = percent_decode(cb_id);
                let approved = body
                    .get("approved")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let result_val = body.get("result").cloned();
                let err_msg = body.get("error").and_then(|v| v.as_str());
                match self
                    .upstream_manager
                    .callback_hub
                    .resolve_callback(&cb_id, approved, result_val, err_msg)
                {
                    Ok(resolved) => {
                        self.event_bus
                            .publish("mcp.callback_resolved", resolved.to_json());
                        self.json_response(
                            200,
                            json!({"success": true, "callback": resolved.to_json()}),
                        )
                    }
                    Err(e) => self.json_response(404, json!({"error": e})),
                }
            }
            "/api/import-config" => {
                let dry_run = body
                    .get("dry_run")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let cfg_root =
                    if let Some(raw_str) = body.get("config_json").and_then(|v| v.as_str()) {
                        match JsonValue::parse(raw_str) {
                            Ok(v) => v,
                            Err(e) => {
                                return self.json_response(
                                    400,
                                    json!({"error": format!("Invalid config JSON: {}", e)}),
                                );
                            }
                        }
                    } else {
                        body.clone()
                    };
                let Some(servers_obj) = cfg_root
                    .get("mcpServers")
                    .or_else(|| cfg_root.get("servers"))
                    .and_then(|v| v.as_object())
                else {
                    return self.json_response(
                        400,
                        json!({"error": "Expected 'mcpServers' (Claude/Cursor mcp.json) object"}),
                    );
                };

                let mut preview_items = Vec::new();
                let mut registered_count = 0usize;

                for (raw_slug, entry) in servers_obj {
                    let slug = raw_slug.trim();
                    if slug.is_empty() {
                        continue;
                    }
                    let server_name = if slug.starts_with("external-mcp:") {
                        slug.to_string()
                    } else {
                        format!("external-mcp:{}", slug)
                    };
                    let mut converted_secrets = Vec::new();
                    let transport = if let Some(url) = entry.get("url").and_then(|v| v.as_str()) {
                        if let Err(e) =
                            validate_url_static_ssrf(url, self.config.allow_loopback_upstream)
                        {
                            return self.json_response(403, json!({"error": e}));
                        }
                        let mut header_refs = BTreeMap::new();
                        if let Some(hdrs) = entry.get("headers").and_then(|v| v.as_object()) {
                            for (hk, hv) in hdrs {
                                if let Some(s) = hv.as_str() {
                                    if !s.starts_with("{env:")
                                        && !s.starts_with("{file:")
                                        && (is_secret_env_key(hk) || looks_like_plaintext_secret(s))
                                    {
                                        let env_name = format!(
                                            "OMNI_{}_{}",
                                            slug.to_ascii_uppercase().replace('-', "_"),
                                            hk.to_ascii_uppercase().replace('-', "_")
                                        );
                                        let ref_str = format!("{{env:{}}}", env_name);
                                        converted_secrets.push(json!({
                                            "key": hk.clone(),
                                            "converted_to": ref_str.clone()
                                        }));
                                        header_refs.insert(hk.clone(), ref_str);
                                    } else {
                                        header_refs.insert(hk.clone(), s.to_string());
                                    }
                                }
                            }
                        }
                        UpstreamTransportConfig::StreamableHttp {
                            url: url.to_string(),
                            bearer_ref: None,
                            header_refs,
                            session_mode: SessionMode::Stateful,
                        }
                    } else if let Some(cmd) = entry.get("command").and_then(|v| v.as_str()) {
                        let cmd_lc = cmd.trim().to_ascii_lowercase();
                        if cmd_lc == "cmd" || cmd_lc.ends_with("cmd.exe") {
                            return self.json_response(
                                403,
                                json!({"error": "Command injection guard: direct cmd.exe invocation is forbidden"}),
                            );
                        }
                        let args: Vec<String> = entry
                            .get("args")
                            .and_then(|v| v.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|x| x.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default();
                        let mut env_refs = BTreeMap::new();
                        if let Some(envs) = entry.get("env").and_then(|v| v.as_object()) {
                            for (ek, ev) in envs {
                                if let Some(s) = ev.as_str() {
                                    if !s.starts_with("{env:")
                                        && !s.starts_with("{file:")
                                        && (is_secret_env_key(ek) || looks_like_plaintext_secret(s))
                                    {
                                        let ref_str = format!("{{env:{}}}", ek);
                                        converted_secrets.push(json!({
                                            "key": ek.clone(),
                                            "converted_to": ref_str.clone()
                                        }));
                                        env_refs.insert(ek.clone(), ref_str);
                                    } else {
                                        env_refs.insert(ek.clone(), s.to_string());
                                    }
                                }
                            }
                        }
                        UpstreamTransportConfig::Stdio {
                            command: cmd.to_string(),
                            canonical_command: cmd.to_string(),
                            args,
                            env_refs,
                            cwd: entry.get("cwd").and_then(|v| v.as_str()).map(String::from),
                        }
                    } else {
                        continue;
                    };

                    if !dry_run {
                        let mut srv = ServerRecord::new(
                            &server_name,
                            slug,
                            format!("Imported from mcp.json ({})", slug),
                            "imported",
                        );
                        srv.protocol = ProtocolType::ExternalMcp;
                        srv.status = ToolStatus::Enabled;
                        srv.upstream_config = Some(transport.clone());
                        self.registry.register_server(srv);
                        registered_count += 1;
                    }

                    preview_items.push(json!({
                        "server_name": server_name,
                        "slug": slug,
                        "transport": transport.to_json(),
                        "converted_secrets": JsonValue::Array(converted_secrets),
                    }));
                }

                if !dry_run && registered_count > 0 {
                    self.cache.refresh_now();
                    self.event_bus.publish(
                        "registry.config_imported",
                        json!({"imported_count": registered_count as i64}),
                    );
                }

                self.json_response(
                    200,
                    json!({
                        "dry_run": dry_run,
                        "imported_count": registered_count as i64,
                        "servers": JsonValue::Array(preview_items),
                    }),
                )
            }
            _ => self.json_response(404, json!({"error": "Endpoint not found"})),
        }
    }
}
