//! MCP Proxy Gateway - Core runtime service materializing dynamic virtual MCP servers at `/<service-name>/mcp`.
//! Supports single and batch JSON-RPC 2.0 requests, Circuit Breaking, Response Projection, ABAC,
//! PII & Secret Redaction, OpenTelemetry W3C Distributed Tracing, Canary Weighted Routing,
//! Resilient Fallback, and SHA-256 Merkle Cryptographic Audit Logging.

use crate::data_plane::cache::InMemoryConfigCache;
use crate::data_plane::circuit_breaker::{CircuitBreakerRegistry, SemanticResponseCache};
use crate::data_plane::codemode::{
    CodeModeError, CodeModeExecutor, CodeModeTypeGenerator, CodeOutput, HostCallbacks,
    PendingApproval, SandboxLimits, SpeculativeInjectionResult, TypeGenerationResult,
};
use crate::data_plane::dst::{DeterministicSimulator, FaultInjectionConfig, SimulationReport};
use crate::data_plane::projection::ResponseProjectionEngine;
use crate::data_plane::security::{PIIRedactor, SecurityCharterEngine};
use crate::data_plane::translators::ProtocolTranslator;
use crate::json::JsonValue;
use crate::logging::now_epoch_micros;
use crate::models::{
    format_traceparent, generate_span_id, generate_trace_id, negotiate_protocol_version,
    parse_traceparent, validate_tool_schema, CompletedMutationRecord, FeatureStatus,
    RPCBatchOrSingle, RPCError, RPCRequest, RPCResponse, SagaCompensationRecord, ServerRecord,
    SuspendedExecutionFrame, ToolStatus, TraceSpan, APPROVAL_INVALID, CIRCUIT_OPEN, INTERNAL_ERROR,
    INVALID_PARAMS, METHOD_NOT_FOUND, SAGA_COMPENSATED, SCHEMA_PREFLIGHT_FAILED, TOOL_DISABLED,
    UNAUTHORIZED,
};
use crate::storage::audit_log::sha256_hex;
use crate::storage::MerkleAuditStore;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

/// Merkle audit-log-only status tag recorded when a completed mutation is unwound by a Saga compensation tool.
pub const AUDIT_STATUS_SAGA_COMPENSATED: &str = "COMPENSATED:saga_rollback";
/// Merkle audit-log-only status tag recorded when a completed mutation has no registered `MutationCompensationSpec`.
pub const AUDIT_STATUS_SAGA_UNCOMPENSATED: &str = "UNCOMPENSATED:no_compensation_spec";
/// Merkle audit-log-only status tag recorded when a human approves and resumes a `SuspendedExecutionFrame`.
pub const AUDIT_STATUS_CONTINUATION_RESUMED: &str = "APPROVED:continuation_resume";

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
                RPCError::new(
                    METHOD_NOT_FOUND,
                    format!("Method '{}' not supported.", other),
                ),
            ),
        }
    }

    fn handle_initialize(&self, server: &ServerRecord, request: &RPCRequest) -> RPCResponse {
        let client_ver = request
            .params
            .get("protocolVersion")
            .and_then(|v| v.as_str());
        let negotiated = negotiate_protocol_version(client_ver);
        RPCResponse::ok(
            request.id.clone(),
            json!({
                "protocolVersion": negotiated,
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
                        format!(
                            "Tool '{}' not found on server '{}'.",
                            tool_name, server.name
                        ),
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

        // 1.5. Schema Preflight Validation (-32035 SCHEMA_PREFLIGHT_FAILED)
        if !tool.input_schema.is_null() && tool.input_schema.get("properties").is_some() {
            if let Err(violations) = validate_tool_schema(&tool.input_schema, &arguments) {
                let violation_jsons: Vec<JsonValue> =
                    violations.iter().map(|v| v.to_json()).collect();
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::with_data(
                        SCHEMA_PREFLIGHT_FAILED,
                        format!(
                            "Schema preflight validation failed for tool '{}.{}': {} violations detected.",
                            server.name,
                            tool_name,
                            violations.len()
                        ),
                        json!({
                            "tool": tool_name,
                            "server": server.name.clone(),
                            "violations": JsonValue::Array(violation_jsons),
                        }),
                    ),
                );
            }
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
            downstream_headers.insert("x-omni-version".to_string(), effective_version.to_string());
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
                "structuredContent": final_result,
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

    /// Generates Omni Code Mode TypeScript declarations (`declare namespace codemode { ... }`)
    /// from the active OmniMCP catalog.
    pub fn generate_codemode_types(
        &self,
        server_filter: Option<&str>,
        search_query: Option<&str>,
    ) -> TypeGenerationResult {
        let servers = self.cache.list_servers();
        CodeModeTypeGenerator::generate_declarations(&servers, server_filter, search_query)
    }

    /// Generates speculative 0-turn pre-injected TypeScript declarations (`declare namespace codemode`)
    /// for the top-K highest-confidence tools matching `user_intent` via sub-millisecond BM25 routing.
    pub fn generate_speculative_codemode_types(
        &self,
        user_intent: &str,
        top_k: usize,
        confidence_threshold: f64,
    ) -> SpeculativeInjectionResult {
        let servers = self.cache.list_servers();
        CodeModeTypeGenerator::speculative_pre_inject(
            &servers,
            user_intent,
            top_k,
            confidence_threshold,
        )
    }

    /// Internal helper to resolve and invoke a single tool call from inside CodeMode or Continuation Resume.
    #[allow(clippy::too_many_arguments)]
    fn invoke_codemode_single_tool(
        &self,
        servers_snapshot: &[ServerRecord],
        raw_srv: &str,
        raw_tool: &str,
        args_vec: Vec<JsonValue>,
        require_mutation_approval: bool,
        caller_actor: &str,
        user_token: Option<&str>,
        codemode_traceparent: &str,
    ) -> Result<(JsonValue, Option<CompletedMutationRecord>), CodeModeError> {
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

        let arg_obj = if args_vec.is_empty() {
            JsonValue::object()
        } else if args_vec.len() == 1 && args_vec[0].is_object() {
            args_vec[0].clone()
        } else {
            let mut obj = JsonValue::object();
            if let Some(props) = tool
                .input_schema
                .get("properties")
                .and_then(|p| p.as_object())
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

        // Schema Preflight Validation before approval gate (-32035)
        if !tool.input_schema.is_null() && tool.input_schema.get("properties").is_some() {
            if let Err(violations) = validate_tool_schema(&tool.input_schema, &arg_obj) {
                return Err(CodeModeError::SchemaPreflightFailed(violations));
            }
        }

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
                "arguments": arg_obj.clone()
            }),
        );

        let rpc_resp = self.handle_request_with_trace(
            &server.name,
            &rpc_req,
            caller_actor,
            user_token,
            Some(codemode_traceparent),
        );

        if let Some(err) = rpc_resp.error {
            return Err(CodeModeError::RuntimeError(err.message));
        }

        let parsed_res = if let Some(res) = rpc_resp.result {
            if let Some(text) = res
                .get("content")
                .and_then(|c| c.as_array())
                .and_then(|arr| arr.first())
                .and_then(|item| item.get("text"))
                .and_then(|t| t.as_str())
            {
                JsonValue::parse(text).unwrap_or(res)
            } else {
                res
            }
        } else {
            JsonValue::Null
        };

        let completed_mutation = if tool.is_mutation {
            Some(CompletedMutationRecord {
                server_name: server.name.clone(),
                tool_name: tool.name.clone(),
                input_args: arg_obj,
                output_val: parsed_res.clone(),
                compensation_spec: tool.compensation_spec.clone(),
            })
        } else {
            None
        };

        Ok((parsed_res, completed_mutation))
    }

    /// Unwinds completed mutations in reverse (`LIFO`) order when a multi-step script fails mid-flight,
    /// invoking each tool's declarative `MutationCompensationSpec` and logging `"COMPENSATED:saga_rollback"`
    /// in the cryptographic Merkle Audit Store.
    fn unwind_saga_compensations(
        &self,
        completed_mutations: &mut Vec<CompletedMutationRecord>,
        caller_actor: &str,
        user_token: Option<&str>,
        codemode_traceparent: &str,
    ) -> Vec<SagaCompensationRecord> {
        let mut records = Vec::new();
        while let Some(completed) = completed_mutations.pop() {
            let server_opt = self.cache.get_server(&completed.server_name);
            let comp_spec_opt = completed.compensation_spec.clone().or_else(|| {
                server_opt
                    .as_ref()
                    .and_then(|s| s.tools.get(&completed.tool_name))
                    .and_then(|t| t.compensation_spec.clone())
            });

            if let Some(spec) = comp_spec_opt {
                let target_srv = spec
                    .compensating_server
                    .clone()
                    .unwrap_or_else(|| completed.server_name.clone());
                let mut comp_args = JsonValue::object();
                for (comp_param, expr_path) in &spec.arg_mapping {
                    let resolved_val = if let Some(out_key) = expr_path
                        .strip_prefix("$output.")
                        .or_else(|| expr_path.strip_prefix("output."))
                    {
                        completed
                            .output_val
                            .get(out_key)
                            .cloned()
                            .unwrap_or_else(|| JsonValue::from(format!("saga_{}", out_key)))
                    } else if let Some(in_key) = expr_path
                        .strip_prefix("$input.")
                        .or_else(|| expr_path.strip_prefix("input."))
                    {
                        completed
                            .input_args
                            .get(in_key)
                            .cloned()
                            .unwrap_or(JsonValue::Null)
                    } else {
                        JsonValue::from(expr_path.as_str())
                    };
                    comp_args.insert(comp_param.clone(), resolved_val);
                }

                let rpc_req = RPCRequest::new(
                    1,
                    "tools/call",
                    json!({
                        "name": spec.compensating_tool.clone(),
                        "arguments": comp_args.clone()
                    }),
                );

                let t_comp = Instant::now();
                let resp = self.handle_request_with_trace(
                    &target_srv,
                    &rpc_req,
                    caller_actor,
                    user_token,
                    Some(codemode_traceparent),
                );
                let comp_us = t_comp.elapsed().as_micros() as u64;
                let status_str = if resp.error.is_none() {
                    "COMPENSATED"
                } else {
                    "COMPENSATION_FAILED"
                };
                let comp_out = resp.result.unwrap_or(JsonValue::Null);

                if let Some(store) = &self.audit_store {
                    store.record_invocation(
                        caller_actor,
                        &target_srv,
                        &spec.compensating_tool,
                        &comp_args,
                        AUDIT_STATUS_SAGA_COMPENSATED,
                        comp_us,
                    );
                }

                let key = format!("saga_{}_{}", completed.server_name, completed.tool_name);
                records.push(SagaCompensationRecord {
                    idempotency_key: key,
                    forward_server: completed.server_name,
                    forward_tool: completed.tool_name,
                    compensating_server: target_srv,
                    compensating_tool: spec.compensating_tool,
                    compensating_args: comp_args,
                    status: status_str.to_string(),
                    compensation_output: comp_out,
                });
            } else {
                if let Some(store) = &self.audit_store {
                    store.record_invocation(
                        caller_actor,
                        &completed.server_name,
                        &completed.tool_name,
                        &completed.input_args,
                        AUDIT_STATUS_SAGA_UNCOMPENSATED,
                        0,
                    );
                }
                let key = format!(
                    "saga_uncomp_{}_{}",
                    completed.server_name, completed.tool_name
                );
                records.push(SagaCompensationRecord {
                    idempotency_key: key,
                    forward_server: completed.server_name.clone(),
                    forward_tool: completed.tool_name.clone(),
                    compensating_server: completed.server_name,
                    compensating_tool: "none".to_string(),
                    compensating_args: JsonValue::object(),
                    status: "NO_COMPENSATION_SPEC".to_string(),
                    compensation_output: JsonValue::Null,
                });
            }
        }
        records
    }

    /// Executes an Omni Code Mode JS/TS (`async () => { ... }`) or Rust/Rhai script inside
    /// the sub-millisecond in-process sandbox, routing all `codemode.<server>.<tool>()` calls
    /// through the governed OmniMCP Data Plane with SSA Auto-Parallelization, Vectorized Pushdown,
    /// Resumable Continuation Snapshots, and Declarative Saga Rollbacks.
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
        let completed_mutations: Mutex<Vec<CompletedMutationRecord>> = Mutex::new(Vec::new());

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

        let mut output = {
            let mut invoke_cb = |raw_srv: &str,
                                 raw_tool: &str,
                                 args_vec: Vec<JsonValue>|
             -> Result<JsonValue, CodeModeError> {
                let (val, maybe_mut) = self.invoke_codemode_single_tool(
                    &servers_snapshot,
                    raw_srv,
                    raw_tool,
                    args_vec,
                    require_mutation_approval,
                    caller_actor,
                    user_token,
                    &codemode_traceparent,
                )?;
                if let Some(m) = maybe_mut {
                    completed_mutations.lock().unwrap().push(m);
                }
                Ok(val)
            };

            let mut invoke_parallel_cb =
                |calls: Vec<(String, String, Vec<JsonValue>)>| -> Result<Vec<JsonValue>, CodeModeError> {
                    let gw = self;
                    let snap = &servers_snapshot;
                    let tp = codemode_traceparent.as_str();
                    let outcomes: Vec<Result<(JsonValue, Option<CompletedMutationRecord>), CodeModeError>> =
                        std::thread::scope(|s| {
                            let handles: Vec<_> = calls
                                .into_iter()
                                .map(|(srv, tl, a)| {
                                    s.spawn(move || {
                                        gw.invoke_codemode_single_tool(
                                            snap,
                                            &srv,
                                            &tl,
                                            a,
                                            require_mutation_approval,
                                            caller_actor,
                                            user_token,
                                            tp,
                                        )
                                    })
                                })
                                .collect();
                            handles
                                .into_iter()
                                .map(|h| {
                                    h.join().unwrap_or_else(|_| {
                                        Err(CodeModeError::RuntimeError(
                                            "Parallel wave worker thread panicked".to_string(),
                                        ))
                                    })
                                })
                                .collect()
                        });

                    let mut results = Vec::with_capacity(outcomes.len());
                    let mut first_err: Option<CodeModeError> = None;
                    for res in outcomes {
                        match res {
                            Ok((val, maybe_mut)) => {
                                if let Some(m) = maybe_mut {
                                    completed_mutations.lock().unwrap().push(m);
                                }
                                results.push(val);
                            }
                            Err(e) => {
                                if first_err.is_none() {
                                    first_err = Some(e);
                                }
                            }
                        }
                    }
                    if let Some(err) = first_err {
                        return Err(err);
                    }
                    Ok(results)
                };

            let mut host = HostCallbacks {
                invoke_tool: &mut invoke_cb,
                invoke_parallel_batch: &mut invoke_parallel_cb,
                search_catalog: &search_cb,
                describe_types: &describe_cb,
            };

            let executor = CodeModeExecutor::new(limits.unwrap_or_default());
            executor.execute(code, &mut host, &codemode_traceparent)
        };

        let mut completed_vec = completed_mutations.into_inner().unwrap();

        // 1. If script suspended on an Approval Gate, persist `SuspendedExecutionFrame` into `WalRegistryStore`
        if output.feature_status == FeatureStatus::ApprovalRequired {
            if let Some(ref mut frame) = output.suspended_frame {
                frame.caller_actor = caller_actor.to_string();
                frame.completed_mutations = completed_vec.clone();
                self.cache.registry.save_suspended_frame(frame.clone());
            }
        }

        // 2. If script errored after completing one or more mutations, unwind in LIFO order via Saga Compensation
        if output.status == FeatureStatus::Error.as_str() && !completed_vec.is_empty() {
            let compensations = self.unwind_saga_compensations(
                &mut completed_vec,
                caller_actor,
                user_token,
                &codemode_traceparent,
            );
            output.status = FeatureStatus::SagaRolledBack.as_str().to_string();
            output.feature_status = FeatureStatus::SagaRolledBack;
            output.error_code = Some(SAGA_COMPENSATED);
            output.saga_compensations = compensations;
        }

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
                "require_mutation_approval": require_mutation_approval,
                "vectorized_pushdowns": output.vectorized_pushdowns as i64,
                "auto_parallel_waves": output.auto_parallel_waves as i64
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

    /// Statelessly resumes a suspended CodeMode AST continuation (`SuspendedExecutionFrame`) from
    /// `WalRegistryStore` after human approval, verifying the SHA-256 script hash, executing the
    /// approved mutation, and continuing execution from `instruction_ptr + 1` without re-running
    /// prior read steps.
    pub fn resume_codemode(
        &self,
        execution_id: &str,
        approver: &str,
        limits: Option<SandboxLimits>,
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

        let frame = match self.cache.registry.get_suspended_frame(execution_id) {
            Some(f) => f,
            None => {
                return CodeOutput {
                    status: FeatureStatus::ApprovalInvalid.as_str().to_string(),
                    feature_status: FeatureStatus::ApprovalInvalid,
                    error_code: Some(APPROVAL_INVALID),
                    result: JsonValue::Null,
                    logs: Vec::new(),
                    pending_approvals: Vec::new(),
                    error: Some(format!(
                        "Approval continuation frame '{}' not found or expired (-32038 APPROVAL_INVALID)",
                        execution_id
                    )),
                    boot_parse_us: 0,
                    eval_us: 0,
                    ops_consumed: 0,
                    peak_memory_bytes: 0,
                    tool_calls: 0,
                    intermediate_bytes_processed: 0,
                    output_bytes_returned: 0,
                    traceparent: codemode_traceparent,
                    execution_id: Some(execution_id.to_string()),
                    suspended_frame: None,
                    saga_compensations: Vec::new(),
                    vectorized_pushdowns: 0,
                    rows_scanned_vectorized: 0,
                    auto_parallel_waves: 0,
                    parallelized_tool_calls: 0,
                };
            }
        };

        // Verify approval signature, expiry, and single-use semantics (-32038 APPROVAL_INVALID)
        if let Err(reason) = frame.verify_approval(
            "omni-secret-key",
            approver,
            Some(&frame.pending_arguments),
            crate::logging::now_epoch_secs(),
        ) {
            return CodeOutput {
                status: FeatureStatus::ApprovalInvalid.as_str().to_string(),
                feature_status: FeatureStatus::ApprovalInvalid,
                error_code: Some(APPROVAL_INVALID),
                result: JsonValue::Null,
                logs: frame.logs,
                pending_approvals: Vec::new(),
                error: Some(format!(
                    "Approval verification failed (-32038 APPROVAL_INVALID): {}",
                    reason
                )),
                boot_parse_us: 0,
                eval_us: 0,
                ops_consumed: 0,
                peak_memory_bytes: 0,
                tool_calls: 0,
                intermediate_bytes_processed: 0,
                output_bytes_returned: 0,
                traceparent: codemode_traceparent,
                execution_id: Some(execution_id.to_string()),
                suspended_frame: None,
                saga_compensations: Vec::new(),
                vectorized_pushdowns: 0,
                rows_scanned_vectorized: 0,
                auto_parallel_waves: 0,
                parallelized_tool_calls: 0,
            };
        }

        // Verify SHA-256 script integrity before resuming
        let actual_hash = sha256_hex(frame.script_source.as_bytes());
        if actual_hash != frame.script_hash {
            return CodeOutput {
                status: FeatureStatus::Error.as_str().to_string(),
                feature_status: FeatureStatus::Error,
                error_code: Some(INTERNAL_ERROR),
                result: JsonValue::Null,
                logs: frame.logs,
                pending_approvals: Vec::new(),
                error: Some(format!(
                    "Continuation integrity check failed: expected script_hash {}, got {}",
                    frame.script_hash, actual_hash
                )),
                boot_parse_us: 0,
                eval_us: 0,
                ops_consumed: 0,
                peak_memory_bytes: 0,
                tool_calls: 0,
                intermediate_bytes_processed: 0,
                output_bytes_returned: 0,
                traceparent: codemode_traceparent,
                execution_id: Some(execution_id.to_string()),
                suspended_frame: None,
                saga_compensations: Vec::new(),
                vectorized_pushdowns: 0,
                rows_scanned_vectorized: 0,
                auto_parallel_waves: 0,
                parallelized_tool_calls: 0,
            };
        }

        // Record cryptographic approval event in Merkle Audit Store
        if let Some(store) = &self.audit_store {
            store.record_invocation(
                approver,
                &frame.pending_server,
                &frame.pending_tool,
                &frame.pending_arguments,
                AUDIT_STATUS_CONTINUATION_RESUMED,
                0,
            );
        }

        // Mark the frame as consumed in WAL so replay is rejected with -32038 APPROVAL_INVALID
        let mut consumed_frame = frame.clone();
        consumed_frame.is_consumed = true;
        consumed_frame.approver_identity = Some(approver.to_string());
        self.cache.registry.save_suspended_frame(consumed_frame);

        let servers_snapshot = self.cache.list_servers();
        let mut initial_completed = frame.completed_mutations.clone();

        // Execute the now-approved pending mutation
        let (approved_output, maybe_mut) = match self.invoke_codemode_single_tool(
            &servers_snapshot,
            &frame.pending_server,
            &frame.pending_tool,
            vec![frame.pending_arguments.clone()],
            false,
            &frame.caller_actor,
            user_token,
            &codemode_traceparent,
        ) {
            Ok(pair) => pair,
            Err(e) => {
                let mut err_out = CodeOutput {
                    status: FeatureStatus::Error.as_str().to_string(),
                    feature_status: e.feature_status(),
                    error_code: Some(e.error_code()),
                    result: JsonValue::Null,
                    logs: frame.logs,
                    pending_approvals: Vec::new(),
                    error: Some(e.message()),
                    boot_parse_us: 0,
                    eval_us: 0,
                    ops_consumed: 0,
                    peak_memory_bytes: 0,
                    tool_calls: 1,
                    intermediate_bytes_processed: 0,
                    output_bytes_returned: 0,
                    traceparent: codemode_traceparent.clone(),
                    execution_id: Some(execution_id.to_string()),
                    suspended_frame: None,
                    saga_compensations: Vec::new(),
                    vectorized_pushdowns: 0,
                    rows_scanned_vectorized: 0,
                    auto_parallel_waves: 0,
                    parallelized_tool_calls: 0,
                };
                if !initial_completed.is_empty() {
                    let comps = self.unwind_saga_compensations(
                        &mut initial_completed,
                        &frame.caller_actor,
                        user_token,
                        &codemode_traceparent,
                    );
                    err_out.status = FeatureStatus::SagaRolledBack.as_str().to_string();
                    err_out.feature_status = FeatureStatus::SagaRolledBack;
                    err_out.error_code = Some(SAGA_COMPENSATED);
                    err_out.saga_compensations = comps;
                }
                return err_out;
            }
        };

        if let Some(m) = maybe_mut {
            initial_completed.push(m);
        }
        let completed_mutations = Mutex::new(initial_completed);

        let search_cb = |_query: &str| -> JsonValue { JsonValue::Array(Vec::new()) };
        let describe_cb = |_srv: &str, _tool_opt: Option<&str>| -> String { String::new() };

        let mut output = {
            let mut invoke_cb = |raw_srv: &str,
                                 raw_tool: &str,
                                 args_vec: Vec<JsonValue>|
             -> Result<JsonValue, CodeModeError> {
                let (val, maybe_m) = self.invoke_codemode_single_tool(
                    &servers_snapshot,
                    raw_srv,
                    raw_tool,
                    args_vec,
                    true,
                    &frame.caller_actor,
                    user_token,
                    &codemode_traceparent,
                )?;
                if let Some(m) = maybe_m {
                    completed_mutations.lock().unwrap().push(m);
                }
                Ok(val)
            };

            let mut invoke_parallel_cb =
                |calls: Vec<(String, String, Vec<JsonValue>)>| -> Result<Vec<JsonValue>, CodeModeError> {
                    let mut results = Vec::with_capacity(calls.len());
                    for (srv, tl, a) in calls {
                        let (val, maybe_m) = self.invoke_codemode_single_tool(
                            &servers_snapshot,
                            &srv,
                            &tl,
                            a,
                            true,
                            &frame.caller_actor,
                            user_token,
                            &codemode_traceparent,
                        )?;
                        if let Some(m) = maybe_m {
                            completed_mutations.lock().unwrap().push(m);
                        }
                        results.push(val);
                    }
                    Ok(results)
                };

            let mut host = HostCallbacks {
                invoke_tool: &mut invoke_cb,
                invoke_parallel_batch: &mut invoke_parallel_cb,
                search_catalog: &search_cb,
                describe_types: &describe_cb,
            };

            let executor = CodeModeExecutor::new(limits.unwrap_or_default());
            executor.resume_frame(&frame, approved_output, &mut host, &codemode_traceparent)
        };

        let mut completed_vec = completed_mutations.into_inner().unwrap();

        if output.feature_status == FeatureStatus::ApprovalRequired {
            if let Some(ref mut next_frame) = output.suspended_frame {
                next_frame.caller_actor = frame.caller_actor.clone();
                next_frame.completed_mutations = completed_vec.clone();
                self.cache.registry.save_suspended_frame(next_frame.clone());
            }
        } else if output.status == FeatureStatus::Error.as_str() && !completed_vec.is_empty() {
            let comps = self.unwind_saga_compensations(
                &mut completed_vec,
                &frame.caller_actor,
                user_token,
                &codemode_traceparent,
            );
            output.status = FeatureStatus::SagaRolledBack.as_str().to_string();
            output.feature_status = FeatureStatus::SagaRolledBack;
            output.error_code = Some(SAGA_COMPENSATED);
            output.saga_compensations = comps;
        }

        output.result = PIIRedactor::redact_payload(&output.result);

        let total_us = output.boot_parse_us + output.eval_us;
        self.record_telemetry_and_audit(
            &trace_id,
            &span_id,
            parent_span_id.as_deref(),
            "omni-codemode",
            "resume_codemode",
            approver,
            &json!({
                "execution_id": execution_id,
                "resumed_from_instruction_ptr": frame.instruction_ptr as i64,
                "tool_calls": output.tool_calls as i64
            }),
            &format!("{}:resumed", output.status),
            start_micros,
            total_us,
            false,
            false,
            false,
        );

        output
    }

    /// Runs Deterministic Simulation Testing (`dst`) on a CodeMode script across `config.num_runs`
    /// seeded PRNG trajectories (`Xoshiro256StarStar`), injecting deterministic network drops,
    /// circuit breakers, latency spikes, and malformed payloads while verifying Saga rollbacks.
    pub fn simulate_codemode(
        &self,
        code: &str,
        limits: Option<SandboxLimits>,
        config: FaultInjectionConfig,
        caller_actor: &str,
        user_token: Option<&str>,
    ) -> SimulationReport {
        let servers_snapshot = self.cache.list_servers();
        let sim_limits = limits.unwrap_or_default();

        DeterministicSimulator::simulate_script(
            code,
            sim_limits,
            config,
            |raw_srv, raw_tool, args_slice| {
                self.invoke_codemode_single_tool(
                    &servers_snapshot,
                    raw_srv,
                    raw_tool,
                    args_slice.to_vec(),
                    false,
                    caller_actor,
                    user_token,
                    "00-00000000000000000000000000000001-0000000000000001-01",
                )
            },
            |completed| {
                let mut single_vec = vec![completed.clone()];
                let mut comps = self.unwind_saga_compensations(
                    &mut single_vec,
                    caller_actor,
                    user_token,
                    "00-00000000000000000000000000000001-0000000000000001-01",
                );
                let key = format!("saga_dst_{}_{}", completed.server_name, completed.tool_name);
                comps.pop().unwrap_or(SagaCompensationRecord {
                    idempotency_key: key,
                    forward_server: completed.server_name.clone(),
                    forward_tool: completed.tool_name.clone(),
                    compensating_server: completed.server_name.clone(),
                    compensating_tool: "none".to_string(),
                    compensating_args: JsonValue::object(),
                    status: "NO_COMPENSATION_SPEC".to_string(),
                    compensation_output: JsonValue::Null,
                })
            },
        )
    }

    /// Lists all currently suspended execution frames in the durable `WalRegistryStore`.
    pub fn list_suspended_codemode_frames(&self) -> Vec<SuspendedExecutionFrame> {
        self.cache.registry.list_suspended_frames()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_plane::MCPRegistry;
    use crate::models::{MutationCompensationSpec, ProtocolType, ToolRecord};
    use crate::storage::WalRegistryStore;

    #[test]
    fn test_saga_lifo_unwinding_order_and_32034_error_code() {
        let store = Arc::new(WalRegistryStore::new(":memory:"));
        let registry = MCPRegistry::new(Some(store));
        let mut srv = ServerRecord::new(
            "ledger",
            "Ledger Service",
            "Transactional ledger",
            "fintech",
        );
        srv.protocol = ProtocolType::Grpc;
        srv.status = ToolStatus::Enabled;

        for step in &["step_1", "step_2", "step_3"] {
            let undo_name = format!("undo_{}", step);
            let mut fwd = ToolRecord::new(
                *step,
                "Forward mutation",
                json!({"type": "object", "properties": {"tx_id": {"type": "string"}}}),
            );
            fwd.status = ToolStatus::Enabled;
            fwd.is_mutation = true;
            let mut arg_map = BTreeMap::new();
            arg_map.insert("tx_id".to_string(), "$input.tx_id".to_string());
            fwd.compensation_spec = Some(MutationCompensationSpec {
                forward_tool: (*step).to_string(),
                compensating_server: Some("ledger".to_string()),
                compensating_tool: undo_name.clone(),
                arg_mapping: arg_map,
            });
            srv.add_tool(fwd);

            let mut undo = ToolRecord::new(
                undo_name,
                "Compensating undo tool",
                json!({"type": "object", "properties": {"tx_id": {"type": "string"}}}),
            );
            undo.status = ToolStatus::Enabled;
            srv.add_tool(undo);
        }
        registry.register_server(srv);
        let cache = InMemoryConfigCache::new(registry, 60.0);
        let gw = MCPProxyGateway::new(cache, None);

        let script = r#"
        async () => {
            await codemode.ledger.step_1({ tx_id: "tx_A" });
            await codemode.ledger.step_2({ tx_id: "tx_B" });
            await codemode.ledger.step_3({ tx_id: "tx_C" });
            await codemode.ledger.non_existent_commit({ tx_id: "tx_FAIL" });
            return { ok: true };
        }
        "#;
        let out = gw.execute_codemode(script, None, false, "agent", None, None);
        assert_eq!(out.status, "SAGA_ROLLED_BACK");
        assert_eq!(out.feature_status, FeatureStatus::SagaRolledBack);
        assert_eq!(out.error_code, Some(SAGA_COMPENSATED));
        assert_eq!(out.error_code, Some(-32034));
        assert_eq!(out.saga_compensations.len(), 3);
        // Verify strict LIFO unwinding order: step_3 -> step_2 -> step_1
        assert_eq!(out.saga_compensations[0].forward_tool, "step_3");
        assert_eq!(out.saga_compensations[0].compensating_tool, "undo_step_3");
        assert_eq!(
            out.saga_compensations[0].compensating_args["tx_id"].as_str(),
            Some("tx_C")
        );
        assert_eq!(out.saga_compensations[1].forward_tool, "step_2");
        assert_eq!(out.saga_compensations[1].compensating_tool, "undo_step_2");
        assert_eq!(
            out.saga_compensations[1].compensating_args["tx_id"].as_str(),
            Some("tx_B")
        );
        assert_eq!(out.saga_compensations[2].forward_tool, "step_1");
        assert_eq!(out.saga_compensations[2].compensating_tool, "undo_step_1");
        assert_eq!(
            out.saga_compensations[2].compensating_args["tx_id"].as_str(),
            Some("tx_A")
        );
    }

    #[test]
    fn test_version_single_sourcing_drift_guard() {
        let cargo_ver = env!("CARGO_PKG_VERSION");
        let root_pkg =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/package.json"))
                .expect("Must read package.json");
        let root_json = JsonValue::parse(&root_pkg).expect("Valid package.json");
        assert_eq!(
            root_json["version"].as_str(),
            Some(cargo_ver),
            "package.json version must match Cargo.toml version"
        );

        let ui_pkg =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/package.json"))
                .expect("Must read ui/package.json");
        let ui_json = JsonValue::parse(&ui_pkg).expect("Valid ui/package.json");
        assert_eq!(
            ui_json["version"].as_str(),
            Some(cargo_ver),
            "ui/package.json version must match Cargo.toml version"
        );

        let readme = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))
            .expect("Must read README.md");
        let major_minor: String = cargo_ver.split('.').take(2).collect::<Vec<_>>().join(".");
        assert!(
            readme.contains(&format!("v{}", major_minor)),
            "README.md must reference current major.minor version v{}",
            major_minor
        );

        // v2: every pinned release download URL in README must be the current version.
        let needle = "/releases/download/v";
        let mut seen = 0;
        for (idx, _) in readme.match_indices(needle) {
            let tail = &readme[idx + needle.len()..];
            let ver: String = tail
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            assert_eq!(
                ver, cargo_ver,
                "README.md download link pins {} but Cargo.toml is {}",
                ver, cargo_ver
            );
            seen += 1;
        }
        assert!(seen > 0, "README.md must contain release download links");

        // v2: Dockerfile image label.
        let dockerfile =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Dockerfile"))
                .expect("Must read Dockerfile");
        assert!(
            dockerfile.contains(&format!(
                "org.opencontainers.image.version=\"{}\"",
                cargo_ver
            )),
            "Dockerfile image version label must match Cargo.toml version {}",
            cargo_ver
        );

        // v2: npm wrapper must single-source its version from package.json.
        let wrapper =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/bin/omni-mcp.js"))
                .expect("Must read bin/omni-mcp.js");
        assert!(
            wrapper.contains("require('../package.json').version"),
            "npm wrapper must derive VERSION from package.json"
        );

        // v2: Cargo.lock root package entry.
        let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.lock"))
            .expect("Must read Cargo.lock")
            .replace("\r\n", "\n");
        assert!(
            lock.contains(&format!("name = \"omni-mcp\"\nversion = \"{}\"", cargo_ver)),
            "Cargo.lock root package version must match Cargo.toml"
        );

        // v2: docs/ must carry release notes for the current version.
        let notes = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs")
            .join(format!("RELEASE_v{}.md", cargo_ver));
        assert!(
            notes.exists(),
            "docs/RELEASE_v{}.md must exist for the current version",
            cargo_ver
        );
    }
}
