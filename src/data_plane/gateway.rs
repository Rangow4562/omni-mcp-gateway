//! MCP Proxy Gateway - Core runtime service materializing dynamic virtual MCP servers at `/<service-name>/mcp`.
//! Supports single and batch JSON-RPC 2.0 requests, Circuit Breaking, Response Projection, ABAC, and PII Redaction.

use crate::data_plane::cache::InMemoryConfigCache;
use crate::data_plane::circuit_breaker::{CircuitBreakerRegistry, SemanticResponseCache};
use crate::data_plane::projection::ResponseProjectionEngine;
use crate::data_plane::security::{PIIRedactor, SecurityCharterEngine};
use crate::data_plane::translators::ProtocolTranslator;
use crate::json::JsonValue;
use crate::models::{
    RPCBatchOrSingle, RPCError, RPCRequest, RPCResponse, ServerRecord, ToolStatus, CIRCUIT_OPEN,
    INTERNAL_ERROR, INVALID_PARAMS, METHOD_NOT_FOUND, TOOL_DISABLED, UNAUTHORIZED,
};
use std::time::Instant;

#[derive(Clone)]
pub struct MCPProxyGateway {
    pub cache: InMemoryConfigCache,
    pub translator: ProtocolTranslator,
    pub circuit_breaker: CircuitBreakerRegistry,
    pub semantic_cache: SemanticResponseCache,
}

impl MCPProxyGateway {
    pub fn new(cache: InMemoryConfigCache, translator: Option<ProtocolTranslator>) -> Self {
        Self {
            cache,
            translator: translator.unwrap_or_default(),
            circuit_breaker: CircuitBreakerRegistry::default(),
            semantic_cache: SemanticResponseCache::new(0.0), // opt-in or per-request `_cache`
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
        }
    }

    /// Processes a single or batch JSON-RPC 2.0 request dispatched to `/<service-name>/mcp`.
    pub fn handle_batch_or_single(
        &self,
        service_name: &str,
        batch_or_single: RPCBatchOrSingle,
        caller_actor: &str,
        user_token: Option<&str>,
    ) -> JsonValue {
        match batch_or_single {
            RPCBatchOrSingle::Single(req) => self
                .handle_request(service_name, &req, caller_actor, user_token)
                .to_json(),
            RPCBatchOrSingle::Batch(reqs) => {
                let responses: Vec<JsonValue> = reqs
                    .iter()
                    .map(|r| {
                        self.handle_request(service_name, r, caller_actor, user_token)
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
            "tools/call" => self.handle_tools_call(&server, request, caller_actor, user_token),
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
    ) -> RPCResponse {
        let t0 = Instant::now();
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

        // 5. Protocol Translation & Downstream Execution via Muttley (with optional semantic cache)
        let cache_key = SemanticResponseCache::make_key(&server.name, &tool.name, &arguments);
        let (raw_result, cache_hit) = if let Some(cached) = self.semantic_cache.get(&cache_key) {
            (cached, true)
        } else {
            match self
                .translator
                .execute_tool_call(&server.name, tool, &arguments, user_token, None)
            {
                Ok(res) => {
                    self.circuit_breaker.record_success(&server.name);
                    if tool.idempotent_cacheable {
                        self.semantic_cache.put(cache_key, res.clone());
                    }
                    (res, false)
                }
                Err(ex) => {
                    self.circuit_breaker.record_failure(&server.name);
                    return RPCResponse::err(
                        request.id.clone(),
                        RPCError::new(INTERNAL_ERROR, format!("Execution error downstream: {}", ex)),
                    );
                }
            }
        };

        // 6. Apply Response Projection (GraphQL-like field trimming)
        let projected_result = ResponseProjectionEngine::apply_projection(
            &raw_result,
            projection_paths.as_deref(),
        );

        // 7. Apply PII Redaction
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
                "_metadata": {
                    "server": server.name.clone(),
                    "tool": tool.name.clone(),
                    "projected": projection_paths.is_some(),
                    "cache_hit": cache_hit,
                    "latency_us": elapsed_us
                }
            }),
        )
    }
}
