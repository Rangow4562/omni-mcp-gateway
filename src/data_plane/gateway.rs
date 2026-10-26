//! MCP Proxy Gateway - Core runtime service materializing dynamic virtual MCP servers at `/<service-name>/mcp`.
//! Supports single and batch JSON-RPC 2.0 requests, Circuit Breaking, Response Projection, ABAC,
//! PII & Secret Redaction, OpenTelemetry W3C Distributed Tracing, Canary Weighted Routing,
//! Resilient Fallback, and SHA-256 Merkle Cryptographic Audit Logging.

use crate::data_plane::cache::InMemoryConfigCache;
use crate::data_plane::circuit_breaker::{CircuitBreakerRegistry, SemanticResponseCache};
use crate::data_plane::codemode::{
    CodeModeError, CodeModeExecutor, CodeModeTypeGenerator, CodeOutput, HostCallbacks,
    PendingApproval, SandboxLimits, TypeGenerationResult,
};
use crate::data_plane::projection::ResponseProjectionEngine;
use crate::data_plane::security::{PIIRedactor, SecurityCharterEngine};
use crate::data_plane::translators::ProtocolTranslator;
use crate::json::JsonValue;
use crate::logging::now_epoch_micros;
use crate::models::{
    format_traceparent, generate_span_id, generate_trace_id, parse_traceparent, RPCBatchOrSingle,
    RPCError, RPCRequest, RPCResponse, ServerRecord, ToolStatus, TraceSpan, CIRCUIT_OPEN,
    INTERNAL_ERROR, INVALID_PARAMS, METHOD_NOT_FOUND, TOOL_DISABLED, UNAUTHORIZED,
};
use crate::storage::MerkleAuditStore;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};
use std::time::Instant;

#[derive(Clone)]
pub struct MCPProxyGateway {
    pub cache: InMemoryConfigCache,
    pub translator: ProtocolTranslator,
    pub circuit_breaker: CircuitBreakerRegistry,
    pub semantic_cache: SemanticResponseCache,
    pub audit_store: Option<Arc<MerkleAuditStore>>,
    pub traces: Option<Arc<RwLock<Vec<TraceSpan>>>>,
}

impl MCPProxyGateway {
    pub fn new(cache: InMemoryConfigCache, translator: Option<ProtocolTranslator>) -> Self {
        Self {
            cache,
            translator: translator.unwrap_or_default(),
            circuit_breaker: CircuitBreakerRegistry::default(),
            semantic_cache: SemanticResponseCache::new(0.0),
            audit_store: None,
            traces: None,
        }
    }

    pub fn with_resilience(
        cache: InMemoryConfigCache,
        translator: ProtocolTranslator,
        circuit_breaker: CircuitBreakerRegistry,
        semantic_cache: SemanticResponseCache,
    ) -> Self {
        Self {
            cache,
            translator,
            circuit_breaker,
            semantic_cache,
            audit_store: None,
            traces: None,
        }
    }

    pub fn with_observability(
        mut self,
        audit_store: Arc<MerkleAuditStore>,
        traces: Arc<RwLock<Vec<TraceSpan>>>,
    ) -> Self {
        self.audit_store = Some(audit_store);
        self.traces = Some(traces);
        self
    }

    /// Processes a single or batch JSON-RPC 2.0 request dispatched to `/<service-name>/mcp`.
    pub fn handle_batch_or_single(
        &self,
        service_name: &str,
        batch_or_single: RPCBatchOrSingle,
        caller_actor: &str,
        user_token: Option<&str>,
    ) -> JsonValue {
        self.handle_batch_or_single_with_trace(
            service_name,
            batch_or_single,
            caller_actor,
            user_token,
            None,
        )
    }

    /// Processes a single or batch JSON-RPC 2.0 request with an incoming W3C `traceparent`.
    pub fn handle_batch_or_single_with_trace(
        &self,
        service_name: &str,
        batch_or_single: RPCBatchOrSingle,
        caller_actor: &str,
        user_token: Option<&str>,
        traceparent: Option<&str>,
    ) -> JsonValue {
        match batch_or_single {
            RPCBatchOrSingle::Single(req) => self
                .handle_request_with_trace(
                    service_name,
                    &req,
                    caller_actor,
                    user_token,
                    traceparent,
                )
                .to_json(),
            RPCBatchOrSingle::Batch(reqs) => {
                let responses: Vec<JsonValue> = reqs
                    .iter()
                    .map(|r| {
                        self.handle_request_with_trace(
                            service_name,
                            r,
                            caller_actor,
                            user_token,
                            traceparent,
                        )
                        .to_json()
                    })
                    .collect();
                JsonValue::Array(responses)
            }
        }
    }

    /// Processes an incoming JSON-RPC 2.0 request dispatched to `/<service-name>/mcp`.
    pub fn handle_request(
        &self,
        service_name: &str,
        request: &RPCRequest,
        caller_actor: &str,
        user_token: Option<&str>,
    ) -> RPCResponse {
        self.handle_request_with_trace(service_name, request, caller_actor, user_token, None)
    }

    /// Processes an incoming JSON-RPC 2.0 request with an optional incoming W3C `traceparent`.
    pub fn handle_request_with_trace(
        &self,
        service_name: &str,
        request: &RPCRequest,
        caller_actor: &str,
        user_token: Option<&str>,
        traceparent: Option<&str>,
    ) -> RPCResponse {
        let server = match self.cache.get_server_arc(service_name) {
            Some(s) => s,
            None => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(
                        METHOD_NOT_FOUND,
                        format!(
                            "Virtual MCP server '{}' not found or not registered.",
                            service_name
                        ),
                    ),
                )
            }
        };

        match request.method.as_str() {
            "initialize" => self.handle_initialize(&server, request),
            "notifications/initialized" | "ping" => {
                RPCResponse::ok(request.id.clone(), JsonValue::object())
            }
            "tools/list" => self.handle_tools_list(&server, request),
            "tools/call" => {
                self.handle_tools_call(&server, request, caller_actor, user_token, traceparent)
            }
            "resources/list" => RPCResponse::ok(request.id.clone(), json!({"resources": []})),
            "prompts/list" => RPCResponse::ok(request.id.clone(), json!({"prompts": []})),
            other => RPCResponse::err(
                request.id.clone(),
                RPCError::new(METHOD_NOT_FOUND, format!("Method '{}' not supported.", other)),
            ),
        }
    }

    fn handle_initialize(&self, server: &ServerRecord, request: &RPCRequest) -> RPCResponse {
        RPCResponse::ok(
            request.id.clone(),
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {"listChanged": true},
                    "logging": {}
                },
                "serverInfo": {
                    "name": format!("omni-mcp/{}", server.name),
                    "version": server.version.to_string(),
                    "description": server.description.clone(),
                    "runtime": "rust"
                }
            }),
        )
    }

    fn handle_tools_list(&self, server: &ServerRecord, request: &RPCRequest) -> RPCResponse {
        let mut tools_list = Vec::new();
        for tool in server.tools.values() {
            if tool.status == ToolStatus::Enabled {
                tools_list.push(tool.to_mcp_spec(true));
            }
        }
        RPCResponse::ok(
            request.id.clone(),
            json!({
                "tools": JsonValue::Array(tools_list)
            }),
        )
    }

    fn handle_tools_call(
        &self,
        server: &ServerRecord,
        request: &RPCRequest,
        caller_actor: &str,
        user_token: Option<&str>,
        traceparent: Option<&str>,
    ) -> RPCResponse {
        let t0 = Instant::now();
        let start_micros = now_epoch_micros();

        // 0. Extract or generate OpenTelemetry W3C trace context
        let (trace_id, parent_span_id) = if let Some(tp) = traceparent {
            parse_traceparent(tp)
                .map(|(tid, sid)| (tid, Some(sid)))
                .unwrap_or_else(|| (generate_trace_id(), None))
        } else {
            (generate_trace_id(), None)
        };
        let span_id = generate_span_id();
        let downstream_traceparent = format_traceparent(&trace_id, &span_id);

        let tool_name = match request.params.get("name").and_then(|x| x.as_str()) {
            Some(n) if !n.is_empty() => n,
            _ => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(INVALID_PARAMS, "Missing 'name' in tools/call parameters."),
                )
            }
        };

        let mut arguments = request
            .params
            .get("arguments")
            .cloned()
            .unwrap_or_else(JsonValue::object);

        let tool = match server.tools.get(tool_name) {
            Some(t) => t,
            None => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(
                        METHOD_NOT_FOUND,
                        format!("Tool '{}' not found on server '{}'.", tool_name, server.name),
                    ),
                )
            }
        };

        // 1. Check Tool Status (Must be ENABLED - "discovery does not imply exposure")
        if tool.status != ToolStatus::Enabled {
            return RPCResponse::err(
                request.id.clone(),
                RPCError::new(
                    TOOL_DISABLED,
                    format!(
                        "Tool '{}' is currently {}. Discovery does not imply exposure; owner must enable tool.",
                        tool_name,
                        tool.status.as_str()
                    ),
                ),
            );
        }

        // 2. Security Charter & ABAC Policy Check
        let (is_allowed, reason) =
            SecurityCharterEngine::evaluate_access(server, tool, caller_actor, user_token);
        if !is_allowed {
            return RPCResponse::err(
                request.id.clone(),
                RPCError::new(
                    UNAUTHORIZED,
                    reason.unwrap_or_else(|| "Unauthorized by Charter Policy.".to_string()),
                ),
            );
        }

        // 3. Check Circuit Breaker for Downstream Service
        if let Err(cb_err) = self.circuit_breaker.allow_request(&server.name) {
            return RPCResponse::err(request.id.clone(), RPCError::new(CIRCUIT_OPEN, cb_err));
        }

        // 4. Extract Response Projection paths if provided by agent/caller
        let projection_paths: Option<Vec<String>> = arguments
            .remove("_projection")
            .and_then(|v| {
                v.as_array().map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect::<Vec<_>>()
                })
            })
            .filter(|v| !v.is_empty());

        // 5. Evaluate Canary Weighted Routing
        let mut is_canary = false;
        let mut effective_version = tool.version;
        if let Some(canary_ver) = tool.canary_version {
            if tool.canary_weight_pct > 0 {
                let canary_key = format!(
                    "{}:{}:{}:{}",
                    caller_actor,
                    user_token.unwrap_or("anon"),
                    tool.name,
                    if user_token.is_none() {
                        start_micros / 60_000_000
                    } else {
                        0
                    }
                );
                let h = crate::storage::audit_log::sha256(canary_key.as_bytes());
                let bucket = (h[0] as u16 * 100 / 256) as u8;
                if bucket < tool.canary_weight_pct {
                    is_canary = true;
                    effective_version = canary_ver;
                }
            }
        }

        // Prepare downstream propagation headers
        let mut downstream_headers = HashMap::new();
        downstream_headers.insert("traceparent".to_string(), downstream_traceparent);
        if is_canary {
            downstream_headers.insert("x-omni-canary".to_string(), "true".to_string());
            downstream_headers
                .insert("x-omni-version".to_string(), effective_version.to_string());
        }

        // 6. Protocol Translation & Downstream Execution via Muttley (with semantic cache & resilient fallback)
        let cache_key = SemanticResponseCache::make_key(&server.name, &tool.name, &arguments);
        let mut fallback_invoked = false;
        let mut fallback_tool_name = String::new();

        let (raw_result, cache_hit) = if let Some(cached) = self.semantic_cache.get(&cache_key) {
            (cached, true)
        } else {
            match self.translator.execute_tool_call(
                &server.name,
                tool,
                &arguments,
                user_token,
                Some(downstream_headers.clone()),
            ) {
                Ok(res) => {
                    self.circuit_breaker.record_success(&server.name);
                    if tool.idempotent_cacheable && !is_canary {
                        self.semantic_cache.put(cache_key, res.clone());
                    }
                    (res, false)
                }
                Err(primary_err) => {
                    // Resilient Fallback Evaluation
                    let mut fallback_succeeded = false;
                    let mut fallback_response = JsonValue::Null;

                    if let Some(fb_name) = &tool.fallback_tool {
                        if let Some(fb_tool) = server.tools.get(fb_name) {
                            if fb_tool.status == ToolStatus::Enabled {
                                if let Ok(fb_res) = self.translator.execute_tool_call(
                                    &server.name,
                                    fb_tool,
                                    &arguments,
                                    user_token,
                                    Some(downstream_headers),
                                ) {
                                    fallback_succeeded = true;
                                    fallback_response = fb_res;
                                    fallback_tool_name = fb_name.clone();
                                }
                            }
                        }
                    }

                    if fallback_succeeded {
                        fallback_invoked = true;
                        (fallback_response, false)
                    } else {
                        self.circuit_breaker.record_failure(&server.name);
                        let elapsed_us = t0.elapsed().as_micros() as u64;
                        self.record_telemetry_and_audit(
                            &trace_id,
                            &span_id,
                            parent_span_id.as_deref(),
                            &server.name,
                            &tool.name,
                            caller_actor,
                            &arguments,
                            "ERROR",
                            start_micros,
                            elapsed_us,
                            false,
                            is_canary,
                            false,
                        );
                        return RPCResponse::err(
                            request.id.clone(),
                            RPCError::new(
                                INTERNAL_ERROR,
                                format!("Execution error downstream: {}", primary_err),
                            ),
                        );
                    }
                }
            }
        };

        // 7. Apply Response Projection (GraphQL-like field trimming)
        let projected_result =
            ResponseProjectionEngine::apply_projection(&raw_result, projection_paths.as_deref());

        // 8. Apply PII & Secret Redaction
        let effective_policy = tool
            .charter_policy
            .as_ref()
            .unwrap_or(&server.charter_policy);
        let final_result = if effective_policy.require_redaction {
            PIIRedactor::redact_payload(&projected_result)
        } else {
            projected_result
        };

        let elapsed_us = t0.elapsed().as_micros() as u64;

        // 9. Record Observability Telemetry & Cryptographic Merkle Audit Log
        let active_tool_name = if fallback_invoked {
            &fallback_tool_name
        } else {
            &tool.name
        };
        self.record_telemetry_and_audit(
            &trace_id,
            &span_id,
            parent_span_id.as_deref(),
            &server.name,
            active_tool_name,
            caller_actor,
            &arguments,
            "OK",
            start_micros,
            elapsed_us,
            cache_hit,
            is_canary,
            fallback_invoked,
        );

        let mut meta = json!({
            "server": server.name.clone(),
            "tool": tool.name.clone(),
            "version": effective_version as i64,
            "projected": projection_paths.is_some(),
            "cache_hit": cache_hit,
            "latency_us": elapsed_us as i64,
            "trace_id": trace_id,
            "span_id": span_id,
            "canary": is_canary,
            "fallback_invoked": fallback_invoked
        });

        if fallback_invoked {
            meta.insert("fallback_tool", fallback_tool_name);
        }

        RPCResponse::ok(
            request.id.clone(),
            json!({
                "content": [
                    {
                        "type": "text",
                        "text": final_result.to_string_pretty()
                    }
                ],
                "isError": false,
                "_metadata": meta
            }),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn record_telemetry_and_audit(
        &self,
        trace_id: &str,
        span_id: &str,
        parent_span_id: Option<&str>,
        server_name: &str,
        tool_name: &str,
        caller_actor: &str,
        arguments: &JsonValue,
        status: &str,
        start_micros: u64,
        elapsed_us: u64,
        cache_hit: bool,
        canary: bool,
        fallback: bool,
    ) {
        // Record Cryptographic Merkle Audit Log
        if let Some(store) = &self.audit_store {
            let audit_status = if canary {
                format!("{}:canary", status)
            } else if fallback {
                format!("{}:fallback", status)
            } else {
                status.to_string()
            };
            store.record_invocation(
                caller_actor,
                server_name,
                tool_name,
                arguments,
                &audit_status,
                elapsed_us,
            );
        }

        // Record OpenTelemetry Distributed Trace Span
        if let Some(traces_lock) = &self.traces {
            let mut attrs = BTreeMap::new();
            attrs.insert(
                "rpc.system".to_string(),
                JsonValue::from("omni-mcp".to_string()),
            );
            attrs.insert(
                "rpc.service".to_string(),
                JsonValue::from(server_name.to_string()),
            );
            attrs.insert(
                "rpc.method".to_string(),
                JsonValue::from(tool_name.to_string()),
            );
            attrs.insert(
                "actor".to_string(),
                JsonValue::from(caller_actor.to_string()),
            );
            attrs.insert("mcp.cache_hit".to_string(), JsonValue::Bool(cache_hit));
            attrs.insert("mcp.canary".to_string(), JsonValue::Bool(canary));
            attrs.insert("mcp.fallback".to_string(), JsonValue::Bool(fallback));

            let span = TraceSpan {
                trace_id: trace_id.to_string(),
                span_id: span_id.to_string(),
                parent_span_id: parent_span_id.map(String::from),
                name: format!("{}.{}", server_name, tool_name),
                service_name: server_name.to_string(),
                tool_name: tool_name.to_string(),
                start_time_us: start_micros,
                duration_us: elapsed_us,
                status: status.to_string(),
                attributes: attrs,
            };

            if let Ok(mut g) = traces_lock.write() {
                if g.len() >= 5000 {
                    g.remove(0);
                }
                g.push(span);
            }
        }
    }

    /// Generates Cloudflare Code Mode TypeScript declarations (`declare namespace codemode { ... }`)
    /// from the active OmniMCP catalog.
    pub fn generate_codemode_types(
        &self,
        server_filter: Option<&str>,
        search_query: Option<&str>,
    ) -> TypeGenerationResult {
        let servers = self.cache.list_servers();
        CodeModeTypeGenerator::generate_declarations(&servers, server_filter, search_query)
    }

    /// Executes a Cloudflare Code Mode JS/TS (`async () => { ... }`) or Rust/Rhai script inside
    /// the sub-millisecond in-process sandbox, routing all `codemode.<server>.<tool>()` calls
    /// through the governed OmniMCP Data Plane.
    pub fn execute_codemode(
        &self,
        code: &str,
        limits: Option<SandboxLimits>,
        require_mutation_approval: bool,
        caller_actor: &str,
        user_token: Option<&str>,
        traceparent: Option<&str>,
    ) -> CodeOutput {
        let start_micros = now_epoch_micros();
        let (trace_id, parent_span_id) = if let Some(tp) = traceparent {
            parse_traceparent(tp)
                .map(|(tid, sid)| (tid, Some(sid)))
                .unwrap_or_else(|| (generate_trace_id(), None))
        } else {
            (generate_trace_id(), None)
        };
        let span_id = generate_span_id();
        let codemode_traceparent = format_traceparent(&trace_id, &span_id);

        let servers_snapshot = self.cache.list_servers();

        let mut invoke_cb =
            |raw_srv: &str, raw_tool: &str, args_vec: Vec<JsonValue>| -> Result<JsonValue, CodeModeError> {
                // Resolve server by exact name or sanitized TypeScript identifier
                let server_opt = self.cache.get_server(raw_srv).or_else(|| {
                    servers_snapshot
                        .iter()
                        .find(|s| {
                            s.name.eq_ignore_ascii_case(raw_srv)
                                || CodeModeTypeGenerator::sanitize_ident(&s.name)
                                    .eq_ignore_ascii_case(raw_srv)
                        })
                        .cloned()
                });

                let server = match server_opt {
                    Some(s) => s,
                    None => {
                        return Err(CodeModeError::RuntimeError(format!(
                            "Virtual MCP server '{}' not found in catalog",
                            raw_srv
                        )))
                    }
                };

                let tool = server
                    .tools
                    .get(raw_tool)
                    .or_else(|| {
                        server.tools.values().find(|t| {
                            t.name.eq_ignore_ascii_case(raw_tool)
                                || CodeModeTypeGenerator::sanitize_ident(&t.name)
                                    .eq_ignore_ascii_case(raw_tool)
                        })
                    })
                    .ok_or_else(|| {
                        CodeModeError::RuntimeError(format!(
                            "Tool '{}' not found on server '{}'",
                            raw_tool, server.name
                        ))
                    })?;

                // Convert positional arguments into named JSON object if needed
                let arg_obj = if args_vec.is_empty() {
                    JsonValue::object()
                } else if args_vec.len() == 1 && args_vec[0].is_object() {
                    args_vec[0].clone()
                } else {
                    let mut obj = JsonValue::object();
                    if let Some(props) =
                        tool.input_schema.get("properties").and_then(|p| p.as_object())
                    {
                        for (idx, (prop_key, _)) in props.iter().enumerate() {
                            if let Some(val) = args_vec.get(idx) {
                                obj.insert(prop_key.clone(), val.clone());
                            }
                        }
                    }
                    for (idx, val) in args_vec.iter().enumerate() {
                        let k = format!("arg{}", idx);
                        if !obj.contains_key(&k) {
                            obj.insert(k, val.clone());
                        }
                    }
                    obj
                };

                // Human-in-the-loop Mutation Approval Gate
                if require_mutation_approval && tool.is_mutation {
                    return Err(CodeModeError::ApprovalRequired(PendingApproval {
                        server_name: server.name.clone(),
                        tool_name: tool.name.clone(),
                        arguments: arg_obj,
                        reason: format!(
                            "Tool '{}.{}' is a state-mutating operation (is_mutation=true) and requires approval before execution.",
                            server.name, tool.name
                        ),
                    }));
                }

                let rpc_req = RPCRequest::new(
                    1,
                    "tools/call",
                    json!({
                        "name": tool.name.clone(),
                        "arguments": arg_obj
                    }),
                );

                let rpc_resp = self.handle_request_with_trace(
                    &server.name,
                    &rpc_req,
                    caller_actor,
                    user_token,
                    Some(&codemode_traceparent),
                );

                if let Some(err) = rpc_resp.error {
                    return Err(CodeModeError::RuntimeError(err.message));
                }

                if let Some(res) = rpc_resp.result {
                    if let Some(text) = res
                        .get("content")
                        .and_then(|c| c.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|item| item.get("text"))
                        .and_then(|t| t.as_str())
                    {
                        if let Ok(parsed) = JsonValue::parse(text) {
                            return Ok(parsed);
                        }
                    }
                    return Ok(res);
                }

                Ok(JsonValue::Null)
            };

        let search_cb = |query: &str| -> JsonValue {
            let q = query.trim().to_lowercase();
            let mut matches = Vec::new();
            for s in &servers_snapshot {
                if s.status != ToolStatus::Enabled {
                    continue;
                }
                for t in s.tools.values() {
                    if t.status != ToolStatus::Enabled {
                        continue;
                    }
                    let haystack = format!(
                        "{} {} {} {}",
                        s.name,
                        t.name,
                        t.description,
                        t.tags.join(" ")
                    )
                    .to_lowercase();
                    if q.is_empty() || q.split_whitespace().any(|tok| haystack.contains(tok)) {
                        matches.push(json!({
                            "server": CodeModeTypeGenerator::sanitize_ident(&s.name),
                            "tool": CodeModeTypeGenerator::sanitize_ident(&t.name),
                            "description": t.description.clone(),
                            "is_mutation": t.is_mutation
                        }));
                    }
                }
            }
            JsonValue::Array(matches)
        };

        let describe_cb = |srv: &str, tool_opt: Option<&str>| -> String {
            CodeModeTypeGenerator::generate_declarations(&servers_snapshot, Some(srv), tool_opt)
                .typescript_declarations
        };

        let mut host = HostCallbacks {
            invoke_tool: &mut invoke_cb,
            search_catalog: &search_cb,
            describe_types: &describe_cb,
        };

        let executor = CodeModeExecutor::new(limits.unwrap_or_default());
        let mut output = executor.execute(code, &mut host, &codemode_traceparent);

        // Apply PII & Secret Redaction on the final sandbox result
        output.result = PIIRedactor::redact_payload(&output.result);

        let total_us = output.boot_parse_us + output.eval_us;
        self.record_telemetry_and_audit(
            &trace_id,
            &span_id,
            parent_span_id.as_deref(),
            "omni-codemode",
            "codemode",
            caller_actor,
            &json!({
                "ops_consumed": output.ops_consumed as i64,
                "tool_calls": output.tool_calls as i64,
                "require_mutation_approval": require_mutation_approval
            }),
            &format!("{}:codemode", output.status),
            start_micros,
            total_us,
            false,
            false,
            false,
        );

        output
    }
}
