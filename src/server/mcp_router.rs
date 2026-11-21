//! HTTP Dispatch & MCP JSON-RPC 2.0 Routing (`/omni/mcp`, `/<service>/mcp`, `/g/<slug>/mcp`, stdio server).

use super::auth::{is_origin_allowed, AuthRole};
use super::http_codec::{get_header, percent_decode};
use super::{get_gateway_context, ProductionGatewayContext};
use crate::json::JsonValue;
use crate::models::{RPCBatchOrSingle, ToolStatus, HEADER_MISMATCH, PARSE_ERROR, RATE_LIMITED};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::atomic::Ordering;

impl ProductionGatewayContext {
    /// Dispatches an HTTP request and returns `(status_code, content_type, extra_headers, body_bytes)`.
    pub fn handle_http(
        &self,
        method: &str,
        raw_path: &str,
        headers: &HashMap<String, String>,
        body_bytes: &[u8],
    ) -> (u16, String, Vec<(String, String)>, Vec<u8>) {
        self.metrics.requests_total.fetch_add(1, Ordering::Relaxed);
        let (raw_path_clean, query_opt) = match raw_path.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (raw_path, None),
        };
        let normalized_path = if let Some(stripped) = raw_path_clean.strip_prefix("/api/v1") {
            if stripped == "/openapi.json" {
                "/openapi.json".to_string()
            } else {
                format!("/api{}", stripped)
            }
        } else {
            raw_path_clean.to_string()
        };
        let path = normalized_path.as_str();

        if let Some(origin) = get_header(headers, "origin") {
            if !is_origin_allowed(
                origin,
                &self.config.cors_allowed_origins,
                get_header(headers, "host"),
            ) {
                return (
                    403,
                    "application/json".to_string(),
                    Vec::new(),
                    json!({
                        "error": "Forbidden: Untrusted Origin for localhost (DNS rebinding protection)"
                    })
                    .to_string_compact()
                    .into_bytes(),
                );
            }
        }

        if path.starts_with("/api/")
            && path != "/api/openapi.json"
            && path != "/api/oauth/callback"
            && !method.eq_ignore_ascii_case("OPTIONS")
        {
            let required_role = match path {
                _ if (path.starts_with("/api/servers/external/")
                    || path.starts_with("/api/v1/servers/external/"))
                    && path.ends_with("/inspect")
                    && method.eq_ignore_ascii_case("GET") =>
                {
                    AuthRole::Agent
                }
                _ if path.starts_with("/api/servers/external/")
                    || path.starts_with("/api/v1/servers/external/") =>
                {
                    AuthRole::Admin
                }
                "/api/servers/toggle_status"
                | "/api/tools/toggle_status"
                | "/api/tools/rollback"
                | "/api/crawler/trigger"
                | "/api/wasm-tools"
                | "/api/import-config" => AuthRole::Admin,
                _ if path.starts_with("/api/oauth/authorize/") => AuthRole::Admin,
                "/api/codemode/suspended"
                | "/api/codemode/resume"
                | "/api/diffs/approve"
                | "/api/callbacks" => AuthRole::Approver,
                _ if path.starts_with("/api/codemode/suspended/")
                    || path.starts_with("/api/callbacks/") =>
                {
                    AuthRole::Approver
                }
                "/api/diffs"
                | "/api/traces"
                | "/api/audit"
                | "/api/audit/verify"
                | "/api/telemetry/otlp/status"
                | "/api/tools/propose_update" => AuthRole::Operator,
                "/api/stats"
                | "/api/v1/stats"
                | "/api/events"
                | "/api/v1/events"
                | "/api/servers"
                | "/api/v1/servers"
                | "/api/groups"
                | "/api/v1/groups"
                | "/api/codemode/types"
                | "/api/codemode/speculative_types"
                | "/api/execute"
                | "/api/codemode/eval"
                | "/api/codemode/simulate" => AuthRole::Agent,
                _ if path.starts_with("/api/servers/") => AuthRole::Agent,
                _ => AuthRole::Admin,
            };

            let principal = match self.authenticate(headers) {
                Ok(p) => p,
                Err(_) => {
                    return self.unauthorized_response(path);
                }
            };

            if !principal.role.can_satisfy(required_role) {
                return self.forbidden_response(path, required_role);
            }
        }

        if method.eq_ignore_ascii_case("OPTIONS") {
            return (200, "text/plain".to_string(), Vec::new(), Vec::new());
        }

        if method.eq_ignore_ascii_case("GET") {
            return self.handle_get(path, query_opt, headers);
        }

        if method.eq_ignore_ascii_case("POST") {
            return self.handle_post(path, headers, body_bytes);
        }

        if method.eq_ignore_ascii_case("DELETE") {
            return self.handle_delete(path, headers);
        }

        self.json_response(405, json!({"error": "Method not allowed"}))
    }

    fn handle_post(
        &self,
        path: &str,
        headers: &HashMap<String, String>,
        body_bytes: &[u8],
    ) -> (u16, String, Vec<(String, String)>, Vec<u8>) {
        if body_bytes.len() > self.config.max_body_size_bytes {
            self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
            return self.json_response(
                413,
                json!({
                    "error": format!(
                        "Payload size {} bytes exceeds maximum allowed limit {} bytes",
                        body_bytes.len(),
                        self.config.max_body_size_bytes
                    )
                }),
            );
        }

        let auth_hdr = headers
            .get("authorization")
            .map(|s| s.as_str())
            .unwrap_or("");
        let token = auth_hdr
            .strip_prefix("Bearer ")
            .or_else(|| auth_hdr.strip_prefix("bearer "))
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or("anonymous");

        // A12: Derive actor and rate-limiting key from authenticated principal identity
        // rather than trusting client-supplied headers (e.g. X-Caller-Actor)
        let principal = self.authenticate(headers).ok();
        let (actor, rate_key) = if let Some(ref p) = principal {
            (p.id.clone(), format!("{}:{}", p.id, p.role.as_str()))
        } else {
            ("anonymous".to_string(), format!("{}:anonymous", token))
        };
        let actor = actor.as_str();

        let (allowed, _rem) = self.rate_limiter.check_rate_limit(&rate_key, 0);
        if !allowed {
            self.metrics
                .rate_limit_hits_total
                .fetch_add(1, Ordering::Relaxed);
            let err_payload = json!({
                "jsonrpc": "2.0",
                "error": {
                    "code": RATE_LIMITED,
                    "message": "Rate limit exceeded. Too many requests."
                }
            });
            return (
                429,
                "application/json".to_string(),
                vec![("Retry-After".to_string(), "5".to_string())],
                err_payload.to_string_compact().into_bytes(),
            );
        }

        let body_str = String::from_utf8_lossy(body_bytes);
        let parsed_body = if body_str.trim().is_empty() {
            Ok(JsonValue::object())
        } else {
            JsonValue::parse(&body_str)
        };

        let traceparent = get_header(headers, "traceparent");

        // 0. Virtual Tool Group MCP Route: `/groups/<slug>/mcp`
        if let Some(slug) = path
            .strip_prefix("/groups/")
            .and_then(|s| s.strip_suffix("/mcp"))
        {
            let slug = percent_decode(slug.trim());
            let group = match self.registry.get_tool_group(&slug) {
                Some(g) => g,
                None => {
                    return self.json_response(
                        404,
                        json!({
                            "jsonrpc": "2.0",
                            "error": {"code": -32001, "message": format!("Tool group '{}' not found", slug)}
                        }),
                    );
                }
            };
            let json_val = match parsed_body {
                Ok(v) => v,
                Err(e) => {
                    return self.json_response(
                        400,
                        json!({"jsonrpc": "2.0", "error": {"code": PARSE_ERROR, "message": e}}),
                    );
                }
            };
            let req_id = json_val.get("id").cloned();
            let rpc_method = json_val
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("");
            let incoming_sid = get_header(headers, "mcp-session-id").map(|s| s.to_string());
            let session_id = if rpc_method == "initialize" {
                Some(incoming_sid.unwrap_or_else(|| {
                    format!("omni-grp-{}-{}", group.slug, crate::models::generate_ulid())
                }))
            } else {
                incoming_sid
            };
            let group_headers = || -> Vec<(String, String)> {
                session_id
                    .as_ref()
                    .map(|sid| vec![("Mcp-Session-Id".to_string(), sid.clone())])
                    .unwrap_or_default()
            };
            if rpc_method == "initialize" {
                return self.json_response_with_headers(
                    200,
                    json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "result": {
                            "protocolVersion": "2025-11-25",
                            "capabilities": {
                                "tools": {"listChanged": false},
                                "resources": {"listChanged": false},
                                "prompts": {"listChanged": false}
                            },
                            "serverInfo": {
                                "name": format!("omni-group:{}", group.slug),
                                "version": env!("CARGO_PKG_VERSION")
                            }
                        }
                    }),
                    group_headers(),
                );
            } else if rpc_method == "notifications/initialized" {
                return (
                    202,
                    "application/json".to_string(),
                    group_headers(),
                    Vec::new(),
                );
            } else if rpc_method == "ping" {
                return self.json_response_with_headers(
                    200,
                    json!({"jsonrpc": "2.0", "id": req_id, "result": {}}),
                    group_headers(),
                );
            } else if rpc_method == "tools/list" {
                let mut tools_arr = Vec::new();
                for entry in &group.entries {
                    if let Some(srv) = self.cache.get_server_arc(&entry.server_name) {
                        for (t_name, tool) in &srv.tools {
                            if tool.status != ToolStatus::Enabled {
                                continue;
                            }
                            if let Some(ref only_tool) = entry.tool_name {
                                if only_tool != t_name {
                                    continue;
                                }
                            }
                            let exposed = format!("{}_{}", entry.server_name, t_name);
                            tools_arr.push(json!({
                                "name": exposed,
                                "description": tool.description.clone(),
                                "inputSchema": tool.input_schema.clone()
                            }));
                        }
                    }
                }
                return self.json_response_with_headers(
                    200,
                    json!({"jsonrpc": "2.0", "id": req_id, "result": {"tools": JsonValue::Array(tools_arr)}}),
                    group_headers(),
                );
            } else if rpc_method == "tools/call" {
                let params = json_val
                    .get("params")
                    .cloned()
                    .unwrap_or_else(JsonValue::object);
                let tool_req = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(JsonValue::object);
                let mut matched_target: Option<(String, String)> = None;
                for entry in &group.entries {
                    if let Some(srv) = self.cache.get_server_arc(&entry.server_name) {
                        for t_name in srv.tools.keys() {
                            if let Some(ref only_tool) = entry.tool_name {
                                if only_tool != t_name {
                                    continue;
                                }
                            }
                            let exposed = format!("{}_{}", entry.server_name, t_name);
                            if exposed == tool_req {
                                matched_target = Some((entry.server_name.clone(), t_name.clone()));
                                break;
                            }
                        }
                    }
                    if matched_target.is_some() {
                        break;
                    }
                }
                if let Some((srv_name, t_name)) = matched_target {
                    let rpc_req = crate::models::RPCRequest {
                        jsonrpc: "2.0".to_string(),
                        id: req_id.clone().unwrap_or(JsonValue::Null),
                        method: "tools/call".to_string(),
                        params: json!({
                            "name": t_name,
                            "arguments": args
                        }),
                    };
                    let rpc_resp = self.gateway.handle_request_with_trace(
                        &srv_name,
                        &rpc_req,
                        actor,
                        Some(token),
                        traceparent,
                    );
                    return self.json_response_with_headers(
                        200,
                        rpc_resp.to_json(),
                        group_headers(),
                    );
                }
                return self.json_response_with_headers(
                    200,
                    json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "error": {"code": -32601, "message": format!("Tool '{}' not in group '{}'", tool_req, group.slug)}
                    }),
                    group_headers(),
                );
            } else if rpc_method == "resources/list" {
                let mut resources = Vec::new();
                for entry in &group.entries {
                    if let Some(srv) = self.cache.get_server_arc(&entry.server_name) {
                        for res in &srv.external_resources {
                            resources.push(json!({
                                "uri": format!("ext://{}/{}", srv.name, res.uri),
                                "name": res.name.clone(),
                                "description": res.description.clone(),
                                "mimeType": res.mime_type.clone().unwrap_or_else(|| "application/json".to_string())
                            }));
                        }
                    }
                }
                return self.json_response_with_headers(
                    200,
                    json!({"jsonrpc": "2.0", "id": req_id, "result": {"resources": JsonValue::Array(resources), "resultType": "complete"}}),
                    group_headers(),
                );
            } else if rpc_method == "resources/templates/list"
                || rpc_method == "resources/read"
                || rpc_method == "prompts/list"
                || rpc_method == "prompts/get"
                || rpc_method == "tasks/get"
                || rpc_method == "tasks/update"
            {
                let allowed_servers: std::collections::HashSet<String> = group
                    .entries
                    .iter()
                    .map(|e| e.server_name.clone())
                    .collect();
                if rpc_method == "resources/read" {
                    let uri = json_val
                        .get("params")
                        .and_then(|p| p.get("uri"))
                        .and_then(|u| u.as_str())
                        .unwrap_or("");
                    if let Some(rest) = uri.strip_prefix("ext://") {
                        if let Some((srv_name, _)) = rest.split_once('/') {
                            if !allowed_servers.contains(srv_name) {
                                return self.json_response(
                                    200,
                                    json!({
                                        "jsonrpc": "2.0",
                                        "id": req_id,
                                        "error": {"code": -32601, "message": format!("Resource '{}' not in group '{}'", uri, group.slug)}
                                    }),
                                );
                            }
                        }
                    }
                } else if rpc_method == "prompts/get" {
                    let pname = json_val
                        .get("params")
                        .and_then(|p| p.get("name"))
                        .and_then(|u| u.as_str())
                        .unwrap_or("");
                    if let Some((srv_name, _)) = pname.split_once(':') {
                        if !allowed_servers.contains(srv_name) {
                            return self.json_response(
                                200,
                                json!({
                                    "jsonrpc": "2.0",
                                    "id": req_id,
                                    "error": {"code": -32601, "message": format!("Prompt '{}' not in group '{}'", pname, group.slug)}
                                }),
                            );
                        }
                    }
                }
                let rpc_req = crate::models::RPCRequest {
                    jsonrpc: "2.0".to_string(),
                    id: req_id.clone().unwrap_or(JsonValue::Null),
                    method: rpc_method.to_string(),
                    params: json_val
                        .get("params")
                        .cloned()
                        .unwrap_or_else(JsonValue::object),
                };
                let mut resp = self.omni_mcp.handle_request(&rpc_req, actor, Some(token));
                if rpc_method == "prompts/list" {
                    if let Some(res_val) = resp.result.as_mut() {
                        if let Some(arr) = res_val.get("prompts").and_then(|p| p.as_array()) {
                            let filtered: Vec<JsonValue> = arr
                                .iter()
                                .filter(|item| {
                                    let n = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                                    n.split_once(':')
                                        .map(|(srv, _)| allowed_servers.contains(srv))
                                        .unwrap_or(false)
                                })
                                .cloned()
                                .collect();
                            res_val.insert("prompts", JsonValue::Array(filtered));
                        }
                    }
                }
                return self.json_response_with_headers(200, resp.to_json(), group_headers());
            } else if rpc_method == "sampling/createMessage" || rpc_method == "elicitation/create" {
                let params = json_val
                    .get("params")
                    .cloned()
                    .unwrap_or_else(JsonValue::object);
                let target_srv = params.get("server").and_then(|s| s.as_str());
                if let Some(srv_name) = target_srv {
                    if !group.entries.iter().any(|e| e.server_name == srv_name) {
                        return self.json_response_with_headers(
                            200,
                            json!({
                                "jsonrpc": "2.0",
                                "id": req_id,
                                "error": {"code": -32601, "message": format!("Server '{}' not in group '{}'", srv_name, group.slug)}
                            }),
                            group_headers(),
                        );
                    }
                }
                let policy = if let Some(srv_name) = target_srv {
                    self.cache
                        .get_server_arc(srv_name)
                        .map(|s| s.sampling_elicitation_policy.clone())
                        .unwrap_or(crate::models::SamplingElicitationPolicy::Deny)
                } else {
                    group
                        .entries
                        .iter()
                        .filter_map(|e| self.cache.get_server_arc(&e.server_name))
                        .map(|s| s.sampling_elicitation_policy.clone())
                        .find(|p| *p != crate::models::SamplingElicitationPolicy::Deny)
                        .unwrap_or(crate::models::SamplingElicitationPolicy::Deny)
                };
                let resp = crate::extension::omni_mcp::apply_sampling_elicitation_policy(
                    req_id.unwrap_or(JsonValue::Null),
                    rpc_method,
                    &policy,
                    target_srv,
                    Some(&group.slug),
                    &params,
                );
                return self.json_response_with_headers(200, resp.to_json(), group_headers());
            }
            return self.json_response_with_headers(
                200,
                json!({"jsonrpc": "2.0", "id": req_id, "error": {"code": -32601, "message": format!("Method '{}' not found", rpc_method)}}),
                group_headers(),
            );
        }

        // 1. Dynamic Virtual Server MCP Route: `/<service-name>/mcp`
        if path.ends_with("/mcp") && path != "/omni/mcp" {
            let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
            if parts.len() == 2 && parts[1] == "mcp" {
                let service_name = percent_decode(parts[0]);
                let service_name = if self.cache.get_server_arc(&service_name).is_some()
                    || service_name.starts_with("external-mcp:")
                {
                    service_name
                } else if self
                    .cache
                    .get_server_arc(&format!("external-mcp:{}", service_name))
                    .is_some()
                {
                    format!("external-mcp:{}", service_name)
                } else {
                    service_name
                };
                let json_val = match parsed_body {
                    Ok(v) => v,
                    Err(e) => {
                        self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                        return self.json_response(
                            400,
                            json!({
                                "jsonrpc": "2.0",
                                "error": {"code": PARSE_ERROR, "message": e}
                            }),
                        );
                    }
                };

                let hdr_method = get_header(headers, "mcp-method")
                    .or_else(|| get_header(headers, "x-mcp-method"));
                let hdr_name =
                    get_header(headers, "mcp-name").or_else(|| get_header(headers, "x-mcp-name"));

                let req_method = json_val.get("method").and_then(|m| m.as_str());
                let req_name = json_val
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str());

                if let Some(hm) = hdr_method {
                    if req_method != Some(hm) {
                        self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                        return self.json_response(
                            400,
                            json!({
                                "jsonrpc": "2.0",
                                "error": {
                                    "code": HEADER_MISMATCH,
                                    "message": format!("Header mismatch: Mcp-Method '{}' does not match request method '{:?}'", hm, req_method)
                                }
                            }),
                        );
                    }
                }
                if let Some(hn) = hdr_name {
                    if req_name != Some(hn) {
                        self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                        return self.json_response(
                            400,
                            json!({
                                "jsonrpc": "2.0",
                                "error": {
                                    "code": HEADER_MISMATCH,
                                    "message": format!("Header mismatch: Mcp-Name '{}' does not match target name '{:?}'", hn, req_name)
                                }
                            }),
                        );
                    }
                }

                if req_method == Some("notifications/cancelled") {
                    let req_id_num = json_val
                        .get("params")
                        .and_then(|p| p.get("requestId"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let reason = json_val
                        .get("params")
                        .and_then(|p| p.get("reason"))
                        .and_then(|v| v.as_str());
                    let cancelled = self.upstream_manager.cancel_upstream_request(
                        &service_name,
                        req_id_num,
                        reason,
                    );
                    self.event_bus.publish(
                        "mcp.cancelled",
                        json!({
                            "server_name": service_name,
                            "request_id": req_id_num as i64,
                            "cancelled": cancelled,
                        }),
                    );
                    return self
                        .json_response(202, json!({"accepted": true, "cancelled": cancelled}));
                }

                if matches!(
                    req_method,
                    Some("sampling/createMessage") | Some("elicitation/create")
                ) {
                    let policy = self
                        .cache
                        .get_server_arc(&service_name)
                        .map(|s| s.sampling_elicitation_policy.clone())
                        .unwrap_or(crate::models::SamplingElicitationPolicy::Deny);
                    let req_id = json_val.get("id").cloned().unwrap_or(JsonValue::Null);
                    let params = json_val
                        .get("params")
                        .cloned()
                        .unwrap_or_else(JsonValue::object);
                    let resp = crate::extension::omni_mcp::apply_sampling_elicitation_policy(
                        req_id,
                        req_method.unwrap_or(""),
                        &policy,
                        Some(&service_name),
                        None,
                        &params,
                    );
                    return self.json_response(200, resp.to_json());
                }

                if req_method == Some("tasks/update") {
                    let cb_id_opt = json_val
                        .get("params")
                        .and_then(|p| {
                            p.get("callback_id")
                                .or_else(|| p.get("taskId"))
                                .and_then(|v| v.as_str())
                        })
                        .filter(|s| s.starts_with("cb_"));
                    if let Some(cb_id) = cb_id_opt {
                        let params = json_val
                            .get("params")
                            .cloned()
                            .unwrap_or_else(JsonValue::object);
                        let approved = params
                            .get("approved")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(true);
                        let result_val = params
                            .get("result")
                            .or_else(|| params.get("response"))
                            .cloned();
                        let err_msg = params.get("error").and_then(|v| v.as_str());
                        let rpc_id = json_val.get("id").cloned().unwrap_or(JsonValue::Null);
                        return match self
                            .upstream_manager
                            .callback_hub
                            .resolve_callback(cb_id, approved, result_val, err_msg)
                        {
                            Ok(resolved) => self.json_response(
                                200,
                                json!({
                                    "jsonrpc": "2.0",
                                    "id": rpc_id,
                                    "result": {
                                        "status": resolved.status,
                                        "callback_id": resolved.callback_id
                                    }
                                }),
                            ),
                            Err(e) => self.json_response(
                                200,
                                json!({
                                    "jsonrpc": "2.0",
                                    "id": rpc_id,
                                    "error": {"code": -32002, "message": e}
                                }),
                            ),
                        };
                    }
                }

                match RPCBatchOrSingle::from_json(&json_val) {
                    Ok(batch_or_single) => {
                        let resp_json = self.gateway.handle_batch_or_single_with_trace(
                            &service_name,
                            batch_or_single,
                            actor,
                            Some(token),
                            traceparent,
                        );
                        let mut resp_headers = Vec::new();
                        if let Some(rm) = req_method {
                            resp_headers.push(("Mcp-Method".to_string(), rm.to_string()));
                        }
                        if let Some(rn) = req_name {
                            resp_headers.push(("Mcp-Name".to_string(), rn.to_string()));
                        }
                        return self.json_response_with_headers(200, resp_json, resp_headers);
                    }
                    Err(e) => {
                        self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                        return self.json_response(
                            400,
                            json!({
                                "jsonrpc": "2.0",
                                "error": {"code": PARSE_ERROR, "message": e}
                            }),
                        );
                    }
                }
            }
        }

        // 2. Omni MCP Proxy Route: `/omni/mcp`
        if path == "/omni/mcp" {
            let json_val = match parsed_body {
                Ok(v) => v,
                Err(e) => {
                    self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                    return self.json_response(
                        400,
                        json!({
                            "jsonrpc": "2.0",
                            "error": {"code": PARSE_ERROR, "message": e}
                        }),
                    );
                }
            };

            let hdr_method =
                get_header(headers, "mcp-method").or_else(|| get_header(headers, "x-mcp-method"));
            let hdr_name =
                get_header(headers, "mcp-name").or_else(|| get_header(headers, "x-mcp-name"));

            let req_method = json_val.get("method").and_then(|m| m.as_str());
            let req_name = json_val
                .get("params")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str());

            if let Some(hm) = hdr_method {
                if req_method != Some(hm) {
                    self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                    return self.json_response(
                        400,
                        json!({
                            "jsonrpc": "2.0",
                            "error": {
                                "code": HEADER_MISMATCH,
                                "message": format!("Header mismatch: Mcp-Method '{}' does not match request method '{:?}'", hm, req_method)
                            }
                        }),
                    );
                }
            }
            if let Some(hn) = hdr_name {
                if req_name != Some(hn) {
                    self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                    return self.json_response(
                        400,
                        json!({
                            "jsonrpc": "2.0",
                            "error": {
                                "code": HEADER_MISMATCH,
                                "message": format!("Header mismatch: Mcp-Name '{}' does not match target name '{:?}'", hn, req_name)
                            }
                        }),
                    );
                }
            }

            if req_method == Some("notifications/cancelled") {
                let req_id_num = json_val
                    .get("params")
                    .and_then(|p| p.get("requestId"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let target_srv = json_val
                    .get("params")
                    .and_then(|p| p.get("serverName").or_else(|| p.get("server_name")))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let reason = json_val
                    .get("params")
                    .and_then(|p| p.get("reason"))
                    .and_then(|v| v.as_str());
                let cancelled = if !target_srv.is_empty() {
                    self.upstream_manager
                        .cancel_upstream_request(target_srv, req_id_num, reason)
                } else {
                    false
                };
                return self.json_response(202, json!({"accepted": true, "cancelled": cancelled}));
            }

            if req_method == Some("tasks/update") {
                let cb_id_opt = json_val
                    .get("params")
                    .and_then(|p| {
                        p.get("callback_id")
                            .or_else(|| p.get("taskId"))
                            .and_then(|v| v.as_str())
                    })
                    .filter(|s| s.starts_with("cb_"));
                if let Some(cb_id) = cb_id_opt {
                    let params = json_val
                        .get("params")
                        .cloned()
                        .unwrap_or_else(JsonValue::object);
                    let approved = params
                        .get("approved")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true);
                    let result_val = params
                        .get("result")
                        .or_else(|| params.get("response"))
                        .cloned();
                    let err_msg = params.get("error").and_then(|v| v.as_str());
                    let rpc_id = json_val.get("id").cloned().unwrap_or(JsonValue::Null);
                    return match self
                        .upstream_manager
                        .callback_hub
                        .resolve_callback(cb_id, approved, result_val, err_msg)
                    {
                        Ok(resolved) => self.json_response(
                            200,
                            json!({
                                "jsonrpc": "2.0",
                                "id": rpc_id,
                                "result": {
                                    "status": resolved.status,
                                    "callback_id": resolved.callback_id
                                }
                            }),
                        ),
                        Err(e) => self.json_response(
                            200,
                            json!({
                                "jsonrpc": "2.0",
                                "id": rpc_id,
                                "error": {"code": -32002, "message": e}
                            }),
                        ),
                    };
                }
            }

            match RPCBatchOrSingle::from_json(&json_val) {
                Ok(batch_or_single) => {
                    let resp_json =
                        self.omni_mcp
                            .handle_batch_or_single(batch_or_single, actor, Some(token));
                    let mut resp_headers = Vec::new();
                    if let Some(rm) = req_method {
                        resp_headers.push(("Mcp-Method".to_string(), rm.to_string()));
                    }
                    if let Some(rn) = req_name {
                        resp_headers.push(("Mcp-Name".to_string(), rn.to_string()));
                    }
                    return self.json_response_with_headers(200, resp_json, resp_headers);
                }
                Err(e) => {
                    self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                    return self.json_response(
                        400,
                        json!({
                            "jsonrpc": "2.0",
                            "error": {"code": PARSE_ERROR, "message": e}
                        }),
                    );
                }
            }
        }
        self.handle_rest_post(path, headers, actor, token, traceparent, parsed_body)
    }
}

pub fn run_stdio_server(specific_server: Option<&str>) -> i32 {
    let ctx = get_gateway_context();
    ctx.cache.start_background_refresh();
    let omni_mcp = &ctx.omni_mcp;
    let gateway = &ctx.gateway;

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let reader = stdin.lock();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let parsed = match JsonValue::parse(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let err_resp = json!({
                    "jsonrpc": "2.0",
                    "error": {
                        "code": PARSE_ERROR,
                        "message": format!("Invalid JSON: {}", e)
                    }
                });
                let _ = writeln!(stdout, "{}", err_resp.to_string_compact());
                let _ = stdout.flush();
                continue;
            }
        };

        match RPCBatchOrSingle::from_json(&parsed) {
            Ok(batch_or_single) => {
                let is_notification = match &batch_or_single {
                    RPCBatchOrSingle::Single(req) => {
                        req.id.is_null() || req.method.starts_with("notifications/")
                    }
                    _ => false,
                };

                let resp_json = match specific_server {
                    Some(srv) => match batch_or_single {
                        RPCBatchOrSingle::Single(req) => gateway
                            .handle_request(srv, &req, "agent", Some("stdio-token"))
                            .to_json(),
                        RPCBatchOrSingle::Batch(reqs) => {
                            let results: Vec<JsonValue> = reqs
                                .iter()
                                .map(|r| {
                                    gateway
                                        .handle_request(srv, r, "agent", Some("stdio-token"))
                                        .to_json()
                                })
                                .collect();
                            JsonValue::Array(results)
                        }
                    },
                    None => omni_mcp.handle_batch_or_single(
                        batch_or_single,
                        "agent",
                        Some("stdio-token"),
                    ),
                };

                if !is_notification {
                    let _ = writeln!(stdout, "{}", resp_json.to_string_compact());
                    let _ = stdout.flush();
                }
            }
            Err(e) => {
                let err_resp = json!({
                    "jsonrpc": "2.0",
                    "error": {
                        "code": crate::models::INVALID_REQUEST,
                        "message": format!("Invalid JSON-RPC: {}", e)
                    }
                });
                let _ = writeln!(stdout, "{}", err_resp.to_string_compact());
                let _ = stdout.flush();
            }
        }
    }
    0
}
