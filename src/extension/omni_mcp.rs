//! Omni MCP Proxy Server - Centralized incremental discovery proxy for hundreds of servers.
//! Uses BM25 + prefix-weighted semantic scoring for fast, high-precision discovery across 800+ servers.
//! Exposes:
//! - `discover_server`
//! - `discover_tools`
//! - `get_tool_schema`
//! - `invoke_tool`
//! - `batch_invoke_tools`
//! - `recommend_tools` (Autonomous Planning)
//! - `explain_error` (Self-Healing Diagnostics)
//!
//! Full MCP Resources (`resources/list`, `resources/read`) and MCP Prompts (`prompts/list`, `prompts/get`).

use crate::control_plane::{SkillAccessScope, SkillEngine};
use crate::data_plane::cache::InMemoryConfigCache;
use crate::data_plane::codemode::{CodeModeTypeGenerator, SandboxLimits};
use crate::data_plane::dst::FaultInjectionConfig;
use crate::data_plane::gateway::MCPProxyGateway;
use crate::data_plane::task_runner::{TaskDispatchOutcome, TaskManager};
use crate::extension::file_uploads::{
    build_upload_required_mrtr, find_mcp_file_properties, resolve_file_argument,
};
use crate::json::JsonValue;
use crate::models::{
    build_server_capabilities, build_server_discover_result, is_required_capability_supported,
    negotiate_protocol_version, CapabilityScope, ClientCapabilitiesMeta, ElicitationPolicyRule,
    McpTaskRecord, McpTaskStatus, RPCBatchOrSingle, RPCError, RPCRequest, RPCResponse,
    SamplingElicitationPolicy, ToolStatus, APPROVAL_INVALID, APPROVAL_REQUIRED, CIRCUIT_OPEN,
    FUEL_EXHAUSTED, INTERNAL_ERROR, INVALID_PARAMS, MEMORY_EXHAUSTED, METHOD_NOT_FOUND,
    MISSING_REQUIRED_CLIENT_CAPABILITY, OUTBOUND_BLOCKED, SAGA_COMPENSATED,
    SCHEMA_PREFLIGHT_FAILED, TIMEOUT, TOOL_DISABLED, UNAUTHORIZED, UNSUPPORTED_PROTOCOL_VERSION,
};
use crate::storage::FileBlobStore;

pub fn apply_sampling_elicitation_policy(
    req_id: JsonValue,
    method: &str,
    policy: &SamplingElicitationPolicy,
    server_name: Option<&str>,
    group_slug: Option<&str>,
    params: &JsonValue,
) -> RPCResponse {
    match policy {
        SamplingElicitationPolicy::Deny => RPCResponse::err(
            req_id,
            RPCError::new(
                -32003,
                format!("Method '{}' denied by sampling_elicitation_policy", method),
            ),
        ),
        SamplingElicitationPolicy::RelayToClient => RPCResponse::ok(
            req_id,
            json!({
                "resultType": "relay_to_client",
                "relayPolicy": "relay_to_client",
                "method": method,
                "server": server_name.map(JsonValue::from).unwrap_or(JsonValue::Null),
                "group": group_slug.map(JsonValue::from).unwrap_or(JsonValue::Null),
                "params": params.clone(),
            }),
        ),
        SamplingElicitationPolicy::ApprovalGate => RPCResponse::ok(
            req_id,
            json!({
                "resultType": "input_required",
                "relayPolicy": "approval_gate",
                "method": method,
                "server": server_name.map(JsonValue::from).unwrap_or(JsonValue::Null),
                "group": group_slug.map(JsonValue::from).unwrap_or(JsonValue::Null),
                "params": params.clone(),
            }),
        ),
    }
}

#[derive(Clone)]
pub struct OmniMCPServer {
    pub cache: InMemoryConfigCache,
    pub gateway: MCPProxyGateway,
    pub skills: SkillEngine,
    pub task_manager: TaskManager,
    pub file_store: FileBlobStore,
}

impl OmniMCPServer {
    pub fn new(cache: InMemoryConfigCache, gateway: MCPProxyGateway) -> Self {
        let cfg = crate::config::get_config();
        let skills = SkillEngine::new(cache.registry.clone());
        let task_manager = TaskManager::new(
            cache.registry.clone(),
            cfg.pod_id.clone(),
            cfg.max_concurrent_tasks,
            cfg.task_sync_grace_ms,
            cfg.task_ttl_sec,
        );
        let file_store = FileBlobStore::from_config(&cfg, cache.registry.clone());
        Self {
            cache,
            gateway,
            skills,
            task_manager,
            file_store,
        }
    }

    pub fn with_runtime(
        cache: InMemoryConfigCache,
        gateway: MCPProxyGateway,
        skills: SkillEngine,
        task_manager: TaskManager,
        file_store: FileBlobStore,
    ) -> Self {
        Self {
            cache,
            gateway,
            skills,
            task_manager,
            file_store,
        }
    }

    pub fn omni_tools_spec() -> Vec<JsonValue> {
        vec![
            json!({
                "name": "discover_server",
                "description": "Discover MCP servers based on query intent or keyword search using BM25 semantic ranking.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Natural language query or intent (e.g. 'driver tracking', 'eats merchant', 'rider payments')"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max servers to return (default: 5)"
                        }
                    },
                    "required": ["query"]
                }
            }),
            json!({
                "name": "discover_tools",
                "description": "Lookup enabled tools for an identified MCP server.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "server_name": {
                            "type": "string",
                            "description": "Name of the target MCP server"
                        }
                    },
                    "required": ["server_name"]
                }
            }),
            json!({
                "name": "get_tool_schema",
                "description": "Get the precise JSON input schema and parameter requirements for a tool.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "server_name": {"type": "string"},
                        "tool_name": {"type": "string"}
                    },
                    "required": ["server_name", "tool_name"]
                }
            }),
            json!({
                "name": "invoke_tool",
                "description": "Invoke a discovered MCP tool through the gateway with optional field projection.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "server_name": {"type": "string"},
                        "tool_name": {"type": "string"},
                        "arguments": {
                            "type": "object",
                            "description": "Tool parameters matching the tool schema"
                        },
                        "_projection": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Optional field projections to return only required paths (e.g. ['driver.name', 'rating'])"
                        }
                    },
                    "required": ["server_name", "tool_name", "arguments"]
                }
            }),
            json!({
                "name": "batch_invoke_tools",
                "description": "Invoke multiple MCP tools concurrently for complex multi-step agentic workflows.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "calls": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "server_name": {"type": "string"},
                                    "tool_name": {"type": "string"},
                                    "arguments": {"type": "object"}
                                },
                                "required": ["server_name", "tool_name", "arguments"]
                            },
                            "description": "Array of tool invocation specifications"
                        }
                    },
                    "required": ["calls"]
                }
            }),
            json!({
                "name": "recommend_tools",
                "description": "Autonomous planning: analyzes an agent's workflow goal and recommends an ordered sequence of tools across registered enterprise microservices.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "goal": {
                            "type": "string",
                            "description": "Workflow goal or user objective (e.g. 'find driver location and calculate trip fare')"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of pipeline steps to suggest (default: 5)"
                        }
                    },
                    "required": ["goal"]
                }
            }),
            json!({
                "name": "explain_error",
                "description": "Self-healing diagnostics: diagnoses tool failure root cause, evaluates circuit breaker status, recommends remediation, and surfaces fallbacks.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "server_name": {
                            "type": "string",
                            "description": "Microservice server name where the error occurred"
                        },
                        "tool_name": {
                            "type": "string",
                            "description": "Tool name that failed"
                        },
                        "error_message": {
                            "type": "string",
                            "description": "Error message or error code returned by the gateway"
                        }
                    },
                    "required": ["server_name", "tool_name", "error_message"]
                }
            }),
            json!({
                "name": "codemode",
                "description": "Omni Code Mode execution sandbox. Write a single JavaScript/TypeScript script (supporting closure syntax `|x| expr` and service host calls) to coordinate multi-step tool calls (`await codemode.<server>.<tool>(input)`), loops, filtering, and aggregations in < 1ms without bloating the LLM context window.\n\nAvailable TypeScript Declarations:\n{{types}}",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "code": {
                            "type": "string",
                            "description": "JavaScript/TypeScript script (or closure syntax) using `codemode.<server>.<tool>(args)` or `codemode.search(query)`"
                        },
                        "require_mutation_approval": {
                            "type": "boolean",
                            "description": "When true, pauses execution and returns status='APPROVAL_REQUIRED' if the script attempts to invoke any state-mutating tool (is_mutation=true)"
                        },
                        "max_operations": {
                            "type": "integer",
                            "description": "Instruction fuel limit (default: 50000)"
                        }
                    },
                    "required": ["code"]
                }
            }),
            json!({
                "name": "get_codemode_types",
                "description": "Code Mode Search & Speculative Pre-Injection helper: generates compact TypeScript `.d.ts` declarations (`declare namespace codemode { ... }`) for registered OmniMCP servers/tools, or performs 0-turn BM25 speculative schema routing when `intent` is provided.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "server_name": {
                            "type": "string",
                            "description": "Optional server name filter"
                        },
                        "query": {
                            "type": "string",
                            "description": "Optional search keyword to filter tool declarations"
                        },
                        "intent": {
                            "type": "string",
                            "description": "Optional natural language user prompt for 0-turn speculative BM25 top-K schema pre-injection"
                        },
                        "top_k": {
                            "type": "integer",
                            "description": "Maximum number of high-confidence tool declarations to pre-inject (default: 4)"
                        }
                    }
                }
            }),
            json!({
                "name": "resume_codemode",
                "description": "Resumes a suspended CodeMode AST continuation (`SuspendedExecutionFrame`) from `WalRegistryStore` after human approval, verifying the SHA-256 script hash, executing the approved mutation, and continuing execution from `instruction_ptr + 1` without re-running prior read steps.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "execution_id": {
                            "type": "string",
                            "description": "26-character Crockford Base32 ULID of the suspended execution frame"
                        },
                        "approver": {
                            "type": "string",
                            "description": "Identity of the human operator or policy engine approving the mutation"
                        }
                    },
                    "required": ["execution_id"]
                }
            }),
            json!({
                "name": "simulate_codemode",
                "description": "Deterministic Simulation Testing (`dst`) for CodeMode scripts: runs a script across N seeded PRNG trajectories (`Xoshiro256StarStar`) with deterministic fault injection (network drops, circuit breakers, latency spikes, malformed payloads) and verifies LIFO Saga compensation rollbacks.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "code": {
                            "type": "string",
                            "description": "CodeMode script to simulate under chaos faults"
                        },
                        "seed": {
                            "type": "integer",
                            "description": "Base 64-bit PRNG seed (default: 42)"
                        },
                        "runs": {
                            "type": "integer",
                            "description": "Number of deterministic simulation runs (default: 25)"
                        },
                        "network_drop_prob": {
                            "type": "number",
                            "description": "Probability [0.0..1.0] of a simulated network drop per tool call"
                        },
                        "circuit_open_prob": {
                            "type": "number",
                            "description": "Probability [0.0..1.0] of a simulated open circuit breaker per tool call"
                        }
                    },
                    "required": ["code"]
                }
            }),
        ]
    }

    pub fn handle_batch_or_single(
        &self,
        batch_or_single: RPCBatchOrSingle,
        caller_actor: &str,
        user_token: Option<&str>,
    ) -> JsonValue {
        match batch_or_single {
            RPCBatchOrSingle::Single(req) => self
                .handle_request(&req, caller_actor, user_token)
                .to_json(),
            RPCBatchOrSingle::Batch(reqs) => {
                for r in &reqs {
                    if let Some(meta) = r.params.get("_meta") {
                        if let Some(ver) = meta
                            .get("io.modelcontextprotocol/protocolVersion")
                            .and_then(|v| v.as_str())
                        {
                            if let Some(caps) = crate::models::protocol_capabilities(ver) {
                                if !caps.allows_batching {
                                    return RPCResponse::err(
                                        r.id.clone(),
                                        RPCError::new(
                                            crate::models::INVALID_REQUEST,
                                            format!("JSON-RPC array batching is removed and not supported in protocol version {}", ver),
                                        ),
                                    ).to_json();
                                }
                            }
                        }
                    }
                }
                let responses: Vec<JsonValue> = reqs
                    .iter()
                    .map(|r| self.handle_request(r, caller_actor, user_token).to_json())
                    .collect();
                JsonValue::Array(responses)
            }
        }
    }

    pub fn handle_request(
        &self,
        request: &RPCRequest,
        caller_actor: &str,
        user_token: Option<&str>,
    ) -> RPCResponse {
        self.handle_request_scoped(request, caller_actor, user_token, None, None)
    }

    pub fn handle_request_scoped(
        &self,
        request: &RPCRequest,
        caller_actor: &str,
        user_token: Option<&str>,
        group_slug: Option<&str>,
        server_scope: Option<&str>,
    ) -> RPCResponse {
        let requested_proto = if let Some(meta) = request.params.get("_meta") {
            if let Some(proto) = meta
                .get("io.modelcontextprotocol/protocolVersion")
                .or_else(|| meta.get("protocolVersion"))
                .and_then(|v| v.as_str())
            {
                if crate::models::protocol_capabilities(proto).is_none() {
                    return RPCResponse::err(
                        request.id.clone(),
                        RPCError::new(
                            UNSUPPORTED_PROTOCOL_VERSION,
                            format!(
                                "UnsupportedProtocolVersionError: Unsupported MCP version '{}'",
                                proto
                            ),
                        ),
                    );
                }
                Some(proto)
            } else {
                None
            }
        } else {
            None
        };

        if requested_proto == Some("2026-07-28")
            && matches!(
                request.method.as_str(),
                "ping" | "logging/setLevel" | "roots/list_changed"
            )
        {
            return RPCResponse::err(
                request.id.clone(),
                RPCError::new(
                    METHOD_NOT_FOUND,
                    format!(
                        "Method '{}' was removed in MCP 2026-07-28 stateless revision",
                        request.method
                    ),
                ),
            );
        }

        let cfg = crate::config::get_config();
        let cap_scope = if let Some(g) = group_slug {
            CapabilityScope::ToolGroup(g.to_string())
        } else if let Some(s) = server_scope {
            CapabilityScope::VirtualService(s.to_string())
        } else {
            CapabilityScope::Global
        };

        match request.method.as_str() {
            "server/discover" => {
                let result = build_server_discover_result(
                    &cfg,
                    cap_scope,
                    "omni-mcp-gateway",
                    env!("CARGO_PKG_VERSION"),
                    concat!(
                        "Omni MCP Autonomous Mesh Gateway & Code Mode Runtime (Rust Engine v",
                        env!("CARGO_PKG_VERSION"),
                        ")"
                    ),
                );
                RPCResponse::ok(request.id.clone(), result)
            }
            "subscriptions/listen" => RPCResponse::ok(
                request.id.clone(),
                json!({"status": "ok", "resultType": "complete"}),
            ),
            "initialize" => {
                let client_ver = request
                    .params
                    .get("protocolVersion")
                    .and_then(|v| v.as_str());
                let negotiated = negotiate_protocol_version(client_ver);

                if let Some(client_caps) = request.params.get("clientCapabilities") {
                    if let Some(req_arr) = client_caps.get("required").and_then(|a| a.as_array()) {
                        for req_cap in req_arr {
                            let cap_str = req_cap.as_str().unwrap_or("");
                            if !is_required_capability_supported(cap_str, &cfg) {
                                return RPCResponse::err(
                                    request.id.clone(),
                                    RPCError::new(
                                        MISSING_REQUIRED_CLIENT_CAPABILITY,
                                        format!(
                                            "MissingRequiredClientCapability: '{}' is required by client but unsupported by server",
                                            cap_str
                                        ),
                                    ),
                                );
                            }
                        }
                    }
                }

                let capabilities = build_server_capabilities(&cfg, cap_scope);
                RPCResponse::ok(
                    request.id.clone(),
                    json!({
                        "protocolVersion": negotiated,
                        "capabilities": capabilities,
                        "serverInfo": {
                            "name": "omni-mcp-gateway",
                            "version": env!("CARGO_PKG_VERSION"),
                            "description": concat!(
                                "Omni MCP Autonomous Mesh Gateway & Code Mode Runtime (Rust Engine v",
                                env!("CARGO_PKG_VERSION"),
                                ")"
                            ),
                            "runtime": "rust"
                        },
                        "resultType": "complete"
                    }),
                )
            }
            "notifications/initialized" | "ping" => {
                RPCResponse::ok(request.id.clone(), json!({"resultType": "complete"}))
            }
            "tools/list" => {
                let intent_opt = request.params.get("intent").and_then(|x| x.as_str());
                let top_k = request
                    .params
                    .get("top_k")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(4) as usize;
                let decls = if let Some(intent) = intent_opt {
                    self.gateway
                        .generate_speculative_codemode_types(intent, top_k, 0.82)
                        .typescript_declarations
                } else {
                    self.gateway
                        .generate_codemode_types(None, None)
                        .typescript_declarations
                };
                let mut tools = Self::omni_tools_spec();
                for t in &mut tools {
                    if t.get("name").and_then(|n| n.as_str()) == Some("codemode") {
                        if let Some(desc) = t.get("description").and_then(|d| d.as_str()) {
                            let injected =
                                CodeModeTypeGenerator::inject_types_template(desc, &decls);
                            t.insert("description", JsonValue::from(injected));
                        }
                    }
                }
                tools.sort_by(|a, b| {
                    a.get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .cmp(b.get("name").and_then(|n| n.as_str()).unwrap_or(""))
                });
                RPCResponse::ok(
                    request.id.clone(),
                    json!({
                        "tools": JsonValue::Array(tools),
                        "resultType": "complete",
                        "ttlMs": 300000i64,
                        "cacheScope": "workspace",
                        "_meta": {
                            "serverInfo": {
                                "name": "omni-mcp-gateway",
                                "version": env!("CARGO_PKG_VERSION")
                            }
                        }
                    }),
                )
            }
            "resources/list" => self.handle_resources_list(request),
            "resources/templates/list" => self.handle_resources_templates_list(request),
            "resources/read" => {
                self.handle_resources_read(request, caller_actor, group_slug, server_scope)
            }
            "resources/directory/read" => {
                self.handle_resources_directory_read(request, group_slug, server_scope)
            }
            "skills/list" => self.handle_skills_list(request, group_slug, server_scope),
            "skills/get" => self.handle_skills_get(request, group_slug, server_scope),
            "prompts/list" => self.handle_prompts_list(request),
            "prompts/get" => self.handle_prompts_get(request, caller_actor),
            "tasks/get" => self.handle_tasks_get(request, group_slug, server_scope),
            "tasks/update" => self.handle_tasks_update(
                request,
                caller_actor,
                user_token,
                group_slug,
                server_scope,
            ),
            "tasks/cancel" => self.handle_tasks_cancel(request, group_slug, server_scope),
            "sampling/createMessage" | "elicitation/create" => {
                let server_name = request.params.get("server").and_then(|s| s.as_str());
                let policy = server_name
                    .and_then(|name| self.cache.get_server_arc(name))
                    .map(|srv| srv.sampling_elicitation_policy.clone())
                    .unwrap_or(SamplingElicitationPolicy::Deny);
                apply_sampling_elicitation_policy(
                    request.id.clone(),
                    &request.method,
                    &policy,
                    server_name,
                    None,
                    &request.params,
                )
            }
            "tools/call" => {
                let tool_name = request
                    .params
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let arguments = request
                    .params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(JsonValue::object);

                match tool_name {
                    "discover_server" => self.call_discover_server(request, &arguments),
                    "discover_tools" => self.call_discover_tools(request, &arguments),
                    "get_tool_schema" => self.call_get_tool_schema(request, &arguments),
                    "invoke_tool" => {
                        self.call_invoke_tool(request, &arguments, caller_actor, user_token)
                    }
                    "batch_invoke_tools" => {
                        self.call_batch_invoke_tools(request, &arguments, caller_actor, user_token)
                    }
                    "recommend_tools" => self.call_recommend_tools(request, &arguments),
                    "explain_error" => self.call_explain_error(request, &arguments),
                    "codemode" => self.call_codemode(request, &arguments, caller_actor, user_token),
                    "get_codemode_types" => self.call_get_codemode_types(request, &arguments),
                    "resume_codemode" => {
                        self.call_resume_codemode(request, &arguments, caller_actor, user_token)
                    }
                    "simulate_codemode" => {
                        self.call_simulate_codemode(request, &arguments, caller_actor, user_token)
                    }
                    other => {
                        let mut found_srv = None;
                        for srv in self.cache.list_servers() {
                            if srv.tools.contains_key(other) {
                                found_srv = Some(srv.name.clone());
                                break;
                            }
                        }

                        if let Some(srv_name) = found_srv {
                            self.gateway.handle_request_with_trace(
                                &srv_name,
                                request,
                                caller_actor,
                                user_token,
                                None,
                            )
                        } else {
                            RPCResponse::err(
                                request.id.clone(),
                                RPCError::new(
                                    METHOD_NOT_FOUND,
                                    format!("Unknown Omni MCP tool: {}", other),
                                ),
                            )
                        }
                    }
                }
            }
            other => RPCResponse::err(
                request.id.clone(),
                RPCError::new(
                    METHOD_NOT_FOUND,
                    format!("Method '{}' not recognized", other),
                ),
            ),
        }
    }

    fn handle_resources_list(&self, request: &RPCRequest) -> RPCResponse {
        let servers = self.cache.list_servers();
        let mut resources = Vec::new();

        // 1. Audit trail & Code Mode TypeScript declaration resources
        resources.push(json!({
            "uri": "omni://audit/merkle-head",
            "name": "Cryptographic Merkle Audit Head",
            "mimeType": "application/json",
            "description": "Current SHA-256 Merkle audit chain root hash and compliance verification status."
        }));
        resources.push(json!({
            "uri": "omni://codemode/types.d.ts",
            "name": "Omni Code Mode TypeScript Declarations",
            "mimeType": "application/typescript",
            "description": "Auto-generated `declare namespace codemode` TypeScript definitions for all enabled OmniMCP tools."
        }));

        // 2. Per-server IDL and schema resources
        for s in servers {
            resources.push(json!({
                "uri": format!("omni://services/{}/idl", s.name),
                "name": format!("{} IDL Specification", s.display_name),
                "mimeType": "text/plain",
                "description": format!("Raw interface definition specification for {}", s.name)
            }));
            resources.push(json!({
                "uri": format!("omni://services/{}/schema", s.name),
                "name": format!("{} Tool Schemas", s.display_name),
                "mimeType": "application/json",
                "description": format!("Consolidated JSON schemas for all tools provided by {}", s.name)
            }));
            for ext_res in &s.external_resources {
                resources.push(json!({
                    "uri": format!("ext://{}/{}", s.name, ext_res.uri),
                    "name": format!("{}: {}", s.display_name, ext_res.name),
                    "mimeType": ext_res.mime_type.clone().unwrap_or_else(|| "text/plain".to_string()),
                    "description": ext_res.description.clone().unwrap_or_default()
                }));
            }
        }
        resources.sort_by(|a, b| {
            a.get("uri")
                .and_then(|u| u.as_str())
                .unwrap_or("")
                .cmp(b.get("uri").and_then(|u| u.as_str()).unwrap_or(""))
        });

        RPCResponse::ok(
            request.id.clone(),
            json!({
                "resources": JsonValue::Array(resources),
                "resultType": "complete",
                "ttlMs": 300000i64,
                "cacheScope": "workspace"
            }),
        )
    }

    fn handle_resources_templates_list(&self, request: &RPCRequest) -> RPCResponse {
        let mut templates = vec![
            json!({
                "uriTemplate": "omni://services/{server_name}/idl",
                "name": "Service IDL Specification Template",
                "mimeType": "text/plain",
                "description": "Raw interface definition specification for a named microservice"
            }),
            json!({
                "uriTemplate": "omni://services/{server_name}/schema",
                "name": "Service Tool Schemas Template",
                "mimeType": "application/json",
                "description": "Consolidated JSON schemas for all tools provided by a named microservice"
            }),
        ];
        for s in self.cache.list_servers() {
            if s.protocol != crate::models::ProtocolType::ExternalMcp {
                continue;
            }
            for ext_res in &s.external_resources {
                if ext_res.uri.contains('{') && ext_res.uri.contains('}') {
                    templates.push(json!({
                        "uriTemplate": format!("ext://{}/{}", s.name, ext_res.uri),
                        "name": ext_res.name.clone(),
                        "mimeType": ext_res.mime_type.clone().unwrap_or_else(|| "application/json".to_string()),
                        "description": ext_res.description.clone()
                    }));
                }
            }
            if let Some(ref pool) = self.gateway.translator.upstream_manager {
                if let Ok(resp) =
                    pool.call_rpc_for_server(&s, "resources/templates/list", json!({}))
                {
                    if let Some(arr) = resp.get("resourceTemplates").and_then(|v| v.as_array()) {
                        for tpl in arr {
                            if let Some(raw_uri) = tpl.get("uriTemplate").and_then(|u| u.as_str()) {
                                let namespaced = format!("ext://{}/{}", s.name, raw_uri);
                                if !templates.iter().any(|existing| {
                                    existing.get("uriTemplate").and_then(|u| u.as_str())
                                        == Some(namespaced.as_str())
                                }) {
                                    let mut obj = tpl.clone();
                                    obj.insert("uriTemplate", JsonValue::Str(namespaced));
                                    templates.push(obj);
                                }
                            }
                        }
                    }
                }
            }
        }
        RPCResponse::ok(
            request.id.clone(),
            json!({
                "resourceTemplates": JsonValue::Array(templates),
                "resultType": "complete",
                "ttlMs": 300000i64,
                "cacheScope": "workspace"
            }),
        )
    }

    fn handle_skills_list(
        &self,
        request: &RPCRequest,
        group_slug: Option<&str>,
        server_scope: Option<&str>,
    ) -> RPCResponse {
        if !crate::config::get_config().enable_skills_extension {
            return RPCResponse::err(
                request.id.clone(),
                RPCError::new(
                    METHOD_NOT_FOUND,
                    "Skills extension (io.modelcontextprotocol/skills) is disabled",
                ),
            );
        }
        let scope = SkillAccessScope {
            group_slug: group_slug.map(String::from),
            server_scope: server_scope.map(String::from),
        };
        let skills: Vec<JsonValue> = self
            .skills
            .list_skills(&scope)
            .iter()
            .map(SkillEngine::to_mcp_summary)
            .collect();
        RPCResponse::ok(
            request.id.clone(),
            json!({
                "skills": JsonValue::Array(skills),
                "resultType": "complete"
            }),
        )
    }

    fn handle_skills_get(
        &self,
        request: &RPCRequest,
        group_slug: Option<&str>,
        server_scope: Option<&str>,
    ) -> RPCResponse {
        if !crate::config::get_config().enable_skills_extension {
            return RPCResponse::err(
                request.id.clone(),
                RPCError::new(
                    METHOD_NOT_FOUND,
                    "Skills extension (io.modelcontextprotocol/skills) is disabled",
                ),
            );
        }
        let slug = request
            .params
            .get("slug")
            .or_else(|| request.params.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if slug.is_empty() {
            return RPCResponse::err(
                request.id.clone(),
                RPCError::new(INVALID_PARAMS, "Missing 'slug' parameter in skills/get"),
            );
        }
        let scope = SkillAccessScope {
            group_slug: group_slug.map(String::from),
            server_scope: server_scope.map(String::from),
        };
        match self.skills.get_skill(slug, &scope) {
            Some(skill) => RPCResponse::ok(request.id.clone(), SkillEngine::to_mcp_detail(&skill)),
            None => RPCResponse::err(
                request.id.clone(),
                RPCError::new(
                    INVALID_PARAMS,
                    format!(
                        "Skill '{}' not found or not accessible in current scope",
                        slug
                    ),
                ),
            ),
        }
    }

    fn handle_resources_directory_read(
        &self,
        request: &RPCRequest,
        group_slug: Option<&str>,
        server_scope: Option<&str>,
    ) -> RPCResponse {
        let uri = match request.params.get("uri").and_then(|u| u.as_str()) {
            Some(u) => u,
            None => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(
                        INVALID_PARAMS,
                        "Missing 'uri' parameter in resources/directory/read",
                    ),
                )
            }
        };
        let scope = SkillAccessScope {
            group_slug: group_slug.map(String::from),
            server_scope: server_scope.map(String::from),
        };
        match self.skills.read_skill_directory(uri, &scope) {
            Ok(entries) => RPCResponse::ok(
                request.id.clone(),
                json!({
                    "uri": uri,
                    "entries": JsonValue::Array(entries),
                    "resultType": "complete"
                }),
            ),
            Err(e) => RPCResponse::err(request.id.clone(), RPCError::new(INVALID_PARAMS, e)),
        }
    }

    fn check_suspended_frame_scope(
        &self,
        frame: &crate::models::SuspendedExecutionFrame,
        group_slug: Option<&str>,
        server_scope: Option<&str>,
    ) -> Result<(), String> {
        if let Some(req_grp) = group_slug {
            let allowed_in_group = self
                .cache
                .registry
                .get_tool_group(req_grp)
                .map(|g| {
                    g.entries
                        .iter()
                        .any(|e| e.server_name == frame.pending_server)
                })
                .unwrap_or(false);
            if !allowed_in_group {
                return Err(format!(
                    "Task '{}' (server '{}') is not accessible from tool group '{}'",
                    frame.execution_id, frame.pending_server, req_grp
                ));
            }
        }
        if let Some(req_srv) = server_scope {
            if frame.pending_server != req_srv {
                return Err(format!(
                    "Task '{}' (server '{}') is not accessible from service '{}'",
                    frame.execution_id, frame.pending_server, req_srv
                ));
            }
        }
        Ok(())
    }

    fn handle_tasks_get(
        &self,
        request: &RPCRequest,
        group_slug: Option<&str>,
        server_scope: Option<&str>,
    ) -> RPCResponse {
        let task_id = request
            .params
            .get("taskId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if task_id.is_empty() {
            return RPCResponse::err(
                request.id.clone(),
                RPCError::new(INVALID_PARAMS, "Missing 'taskId' parameter in tasks/get"),
            );
        }

        // 1. Check durable HA MCP Task record with strict group/service scope isolation
        if let Some(task) = self.cache.registry.get_mcp_task(task_id) {
            if let Err(err_msg) = TaskManager::check_scope(&task, group_slug, server_scope) {
                return RPCResponse::err(request.id.clone(), RPCError::new(UNAUTHORIZED, err_msg));
            }
            return RPCResponse::ok(request.id.clone(), task.to_mcp_wire_json());
        }

        // 2. Fallback to legacy v6.5 SuspendedExecutionFrame (enforcing group/service scope isolation)
        if let Some(frame) = self.gateway.get_suspended_codemode_frame(task_id) {
            if let Err(err_msg) = self.check_suspended_frame_scope(&frame, group_slug, server_scope)
            {
                return RPCResponse::err(request.id.clone(), RPCError::new(UNAUTHORIZED, err_msg));
            }
            RPCResponse::ok(
                request.id.clone(),
                json!({
                    "taskId": frame.execution_id.clone(),
                    "status": if frame.is_consumed { "completed" } else { "suspended" },
                    "toolName": frame.pending_tool.clone(),
                    "serverName": frame.pending_server.clone(),
                    "requiresApproval": !frame.is_consumed,
                    "createdAtEpochMs": (frame.created_at * 1000.0) as i64,
                    "resultType": "complete"
                }),
            )
        } else {
            RPCResponse::err(
                request.id.clone(),
                RPCError::new(INVALID_PARAMS, format!("Task '{}' not found", task_id)),
            )
        }
    }

    fn handle_tasks_cancel(
        &self,
        request: &RPCRequest,
        group_slug: Option<&str>,
        server_scope: Option<&str>,
    ) -> RPCResponse {
        let task_id = request
            .params
            .get("taskId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if task_id.is_empty() {
            return RPCResponse::err(
                request.id.clone(),
                RPCError::new(INVALID_PARAMS, "Missing 'taskId' parameter in tasks/cancel"),
            );
        }

        match self
            .task_manager
            .cancel_task_scoped(task_id, group_slug, server_scope)
        {
            Ok(_task) => {
                // Also delete any paired suspended frame so it cannot be resumed later
                let _ = self.gateway.delete_suspended_codemode_frame(task_id);
                // Spec-compliant GA `tasks/cancel` returns `{}` acknowledgement
                RPCResponse::ok(request.id.clone(), JsonValue::object())
            }
            Err(e) => {
                if e.contains("does not belong") {
                    return RPCResponse::err(request.id.clone(), RPCError::new(UNAUTHORIZED, e));
                }
                // Legacy suspended frame cancel fallback (strictly enforcing group/service scope)
                if let Some(frame) = self.gateway.get_suspended_codemode_frame(task_id) {
                    if let Err(err_msg) =
                        self.check_suspended_frame_scope(&frame, group_slug, server_scope)
                    {
                        return RPCResponse::err(
                            request.id.clone(),
                            RPCError::new(UNAUTHORIZED, err_msg),
                        );
                    }
                    let _ = self.gateway.delete_suspended_codemode_frame(task_id);
                    RPCResponse::ok(request.id.clone(), JsonValue::object())
                } else {
                    RPCResponse::err(request.id.clone(), RPCError::new(INVALID_PARAMS, e))
                }
            }
        }
    }

    fn handle_tasks_update(
        &self,
        request: &RPCRequest,
        caller_actor: &str,
        user_token: Option<&str>,
        group_slug: Option<&str>,
        server_scope: Option<&str>,
    ) -> RPCResponse {
        let task_id = request
            .params
            .get("taskId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if task_id.is_empty() {
            return RPCResponse::err(
                request.id.clone(),
                RPCError::new(INVALID_PARAMS, "Missing 'taskId' parameter in tasks/update"),
            );
        }

        // Enforce scope isolation on tracked HA MCP task OR legacy suspended frame
        if let Some(existing_task) = self.cache.registry.get_mcp_task(task_id) {
            if let Err(err_msg) = TaskManager::check_scope(&existing_task, group_slug, server_scope)
            {
                return RPCResponse::err(request.id.clone(), RPCError::new(UNAUTHORIZED, err_msg));
            }
        } else if let Some(existing_frame) = self.gateway.get_suspended_codemode_frame(task_id) {
            if let Err(err_msg) =
                self.check_suspended_frame_scope(&existing_frame, group_slug, server_scope)
            {
                return RPCResponse::err(request.id.clone(), RPCError::new(UNAUTHORIZED, err_msg));
            }
        }

        // 1. July 2026 GA `io.modelcontextprotocol/tasks` wire path: `inputResponses` -> `{}` ack
        if let Some(input_responses) = request.params.get("inputResponses") {
            let first_resp = input_responses
                .as_array()
                .and_then(|a| a.first())
                .unwrap_or(input_responses);
            let elicit_action = first_resp
                .get("action")
                .or_else(|| first_resp.get("result").and_then(|r| r.get("action")))
                .and_then(|v| v.as_str())
                .unwrap_or("accept");
            let content = first_resp
                .get("content")
                .or_else(|| first_resp.get("result").and_then(|r| r.get("content")))
                .cloned()
                .unwrap_or_else(JsonValue::object);
            let justification = content
                .get("justification")
                .or_else(|| content.get("reason"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let approver = first_resp
                .get("approver")
                .and_then(|v| v.as_str())
                .unwrap_or(caller_actor);
            let digest = first_resp.get("digest").and_then(|v| v.as_str());

            // Submit input responses on HA task record (if present)
            if self.cache.registry.get_mcp_task(task_id).is_some() {
                if let Err(e) = self.task_manager.submit_input_responses_scoped(
                    task_id,
                    input_responses.clone(),
                    group_slug,
                    server_scope,
                ) {
                    return RPCResponse::err(request.id.clone(), RPCError::new(INVALID_PARAMS, e));
                }
            }

            if elicit_action.eq_ignore_ascii_case("accept") {
                if let Some(ref audit) = self.gateway.audit_store {
                    let _ = audit.record_invocation(
                        approver,
                        "codemode",
                        "elicitation_accept",
                        &json!({"task_id": task_id, "justification": justification}),
                        "APPROVED:elicitation_accepted",
                        0,
                    );
                }
                if self.gateway.get_suspended_codemode_frame(task_id).is_some() {
                    let out = self
                        .gateway
                        .resume_codemode(task_id, approver, None, user_token, None, digest);
                    if let Some(mut task) = self.cache.registry.get_mcp_task(task_id) {
                        task.updated_at = crate::logging::now_epoch_secs();
                        if out.status == "OK" {
                            task.status = McpTaskStatus::Completed;
                            task.progress_pct = 100;
                            task.result_payload = Some(out.to_json());
                        } else {
                            task.status = McpTaskStatus::Failed;
                            task.error_payload = Some(json!({
                                "code": out.error_code.unwrap_or(INTERNAL_ERROR),
                                "message": out.error.clone().unwrap_or_else(|| "Resume failed".to_string())
                            }));
                        }
                        self.cache.registry.save_mcp_task(&task);
                    }
                }
            } else {
                // Decline or Cancel -> record Merkle audit & run LIFO Saga rollback
                if let Some(ref audit) = self.gateway.audit_store {
                    let _ = audit.record_invocation(
                        approver,
                        "codemode",
                        "elicitation_decline",
                        &json!({"task_id": task_id, "action": elicit_action, "justification": justification}),
                        "REJECTED:elicitation_declined",
                        0,
                    );
                }
                if let Some(mut frame) = self.gateway.delete_suspended_codemode_frame(task_id) {
                    if !frame.completed_mutations.is_empty() {
                        let _ = self.gateway.unwind_saga_compensations(
                            &mut frame.completed_mutations,
                            &frame.caller_actor,
                            user_token,
                            "",
                        );
                    }
                }
                if let Some(mut task) = self.cache.registry.get_mcp_task(task_id) {
                    task.status = McpTaskStatus::Cancelled;
                    task.status_message = Some(format!("Elicitation {}", elicit_action));
                    task.updated_at = crate::logging::now_epoch_secs();
                    self.cache.registry.save_mcp_task(&task);
                }
            }

            // Spec-compliant GA `tasks/update` returns `{}` acknowledgement
            return RPCResponse::ok(request.id.clone(), JsonValue::object());
        }

        // 2. Backward-compatible v6.5 `action: "resume"|"cancel"` fallback
        let action = request
            .params
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("resume");
        let approver = request
            .params
            .get("approver")
            .and_then(|v| v.as_str())
            .unwrap_or(caller_actor);
        let digest = request.params.get("digest").and_then(|v| v.as_str());

        if action == "resume" {
            let out = self
                .gateway
                .resume_codemode(task_id, approver, None, user_token, None, digest);
            if out.status == "OK" {
                RPCResponse::ok(
                    request.id.clone(),
                    json!({
                        "taskId": task_id,
                        "status": "completed",
                        "result": out.result.clone(),
                        "resultType": "complete"
                    }),
                )
            } else {
                RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(
                        out.error_code.unwrap_or(INTERNAL_ERROR),
                        out.error
                            .unwrap_or_else(|| "Failed to resume task".to_string()),
                    ),
                )
            }
        } else if action == "cancel" {
            if self
                .gateway
                .delete_suspended_codemode_frame(task_id)
                .is_some()
            {
                RPCResponse::ok(
                    request.id.clone(),
                    json!({
                        "taskId": task_id,
                        "status": "cancelled",
                        "resultType": "complete"
                    }),
                )
            } else {
                RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(
                        INVALID_PARAMS,
                        format!("Task '{}' not found to cancel", task_id),
                    ),
                )
            }
        } else {
            RPCResponse::err(
                request.id.clone(),
                RPCError::new(
                    INVALID_PARAMS,
                    format!(
                        "Unsupported task action '{}'; supported actions: resume, cancel",
                        action
                    ),
                ),
            )
        }
    }

    fn handle_resources_read(
        &self,
        request: &RPCRequest,
        caller_actor: &str,
        group_slug: Option<&str>,
        server_scope: Option<&str>,
    ) -> RPCResponse {
        let uri = match request.params.get("uri").and_then(|u| u.as_str()) {
            Some(u) => u,
            None => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(INVALID_PARAMS, "Missing 'uri' parameter in resources/read"),
                )
            }
        };

        if uri.starts_with("skill://") {
            let scope = SkillAccessScope {
                group_slug: group_slug.map(String::from),
                server_scope: server_scope.map(String::from),
            };
            return match self.skills.read_skill_resource(uri, &scope) {
                Ok((resolved_uri, mime, text)) => RPCResponse::ok(
                    request.id.clone(),
                    json!({
                        "contents": [
                            {
                                "uri": resolved_uri,
                                "mimeType": mime,
                                "text": text
                            }
                        ],
                        "resultType": "complete"
                    }),
                ),
                Err(e) => RPCResponse::err(request.id.clone(), RPCError::new(INVALID_PARAMS, e)),
            };
        }

        if uri == "omni://codemode/types.d.ts" {
            let ts = self.gateway.generate_codemode_types(None, None);
            return RPCResponse::ok(
                request.id.clone(),
                json!({
                    "contents": [
                        {
                            "uri": uri,
                            "mimeType": "application/typescript",
                            "text": ts.typescript_declarations
                        }
                    ],
                    "resultType": "complete",
                    "ttlMs": 60000i64,
                    "cacheScope": "workspace"
                }),
            );
        }

        if uri == "omni://audit/merkle-head" {
            let (head_hash, count, verified) = if let Some(store) = &self.gateway.audit_store {
                let h = store.get_head_hash();
                let c = store.list_records(Some(1)).len();
                let (v, _) = store.verify_chain();
                (h, c, v)
            } else {
                (
                    "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
                    0,
                    true,
                )
            };

            let payload = json!({
                "head_hash": head_hash,
                "record_count": count as i64,
                "chain_verified": verified,
                "algorithm": "SHA-256 FIPS 180-4"
            });

            return RPCResponse::ok(
                request.id.clone(),
                json!({
                    "contents": [
                        {
                            "uri": uri,
                            "mimeType": "application/json",
                            "text": payload.to_string_pretty()
                        }
                    ],
                    "resultType": "complete",
                    "ttlMs": 60000i64,
                    "cacheScope": "workspace"
                }),
            );
        }

        if let Some(srv_name) = uri
            .strip_prefix("omni://services/")
            .and_then(|s| s.strip_suffix("/idl"))
        {
            if let Some(server) = self.cache.get_server(srv_name) {
                let idl_text = format!(
                    "// IDL Interface for {}\n// Protocol: {}\n// Version: {}\n// Owning Team: {}\n",
                    server.name,
                    server.protocol.as_str(),
                    server.version,
                    server.owning_team
                );
                return RPCResponse::ok(
                    request.id.clone(),
                    json!({
                        "contents": [
                            {
                                "uri": uri,
                                "mimeType": "text/plain",
                                "text": idl_text
                            }
                        ],
                        "resultType": "complete",
                        "ttlMs": 60000i64,
                        "cacheScope": "workspace"
                    }),
                );
            }
        }

        if let Some(srv_name) = uri
            .strip_prefix("omni://services/")
            .and_then(|s| s.strip_suffix("/schema"))
        {
            if let Some(server) = self.cache.get_server(srv_name) {
                let mut tools_schema = JsonValue::object();
                for (name, tool) in &server.tools {
                    tools_schema.insert(name.clone(), tool.input_schema.clone());
                }
                return RPCResponse::ok(
                    request.id.clone(),
                    json!({
                        "contents": [
                            {
                                "uri": uri,
                                "mimeType": "application/json",
                                "text": tools_schema.to_string_pretty()
                            }
                        ],
                        "resultType": "complete",
                        "ttlMs": 60000i64,
                        "cacheScope": "workspace"
                    }),
                );
            }
        }

        if let Some(rest) = uri.strip_prefix("ext://") {
            if let Some((srv_name, upstream_uri)) = rest.split_once('/') {
                if let Some(server) = self.cache.get_server(srv_name) {
                    if !server.charter_policy.is_actor_allowed(caller_actor) {
                        return RPCResponse::err(
                            request.id.clone(),
                            RPCError::new(
                                crate::models::UNAUTHORIZED,
                                format!(
                                    "Actor '{}' is not authorized to read external resources on server '{}'",
                                    caller_actor, srv_name
                                ),
                            ),
                        );
                    }
                    if let Some(ref pool) = self.gateway.translator.upstream_manager {
                        let rpc_res = match pool.call_rpc_for_server(
                            &server,
                            "resources/read",
                            json!({"uri": upstream_uri}),
                        ) {
                            Ok(r) => {
                                if server.charter_policy.require_redaction {
                                    crate::data_plane::PIIRedactor::redact_payload(&r)
                                } else {
                                    r
                                }
                            }
                            Err(e) => {
                                return RPCResponse::err(
                                    request.id.clone(),
                                    RPCError::new(crate::models::INTERNAL_ERROR, e),
                                );
                            }
                        };
                        return RPCResponse::ok(request.id.clone(), rpc_res);
                    }
                }
            }
        }

        let err_code = if let Some(meta) = request.params.get("_meta") {
            if let Some(proto) = meta
                .get("io.modelcontextprotocol/protocolVersion")
                .or_else(|| meta.get("protocolVersion"))
                .and_then(|v| v.as_str())
            {
                if let Some(caps) = crate::models::protocol_capabilities(proto) {
                    if caps.resource_not_found_is_invalid_params {
                        crate::models::INVALID_PARAMS
                    } else {
                        METHOD_NOT_FOUND
                    }
                } else {
                    METHOD_NOT_FOUND
                }
            } else {
                METHOD_NOT_FOUND
            }
        } else {
            METHOD_NOT_FOUND
        };

        RPCResponse::err(
            request.id.clone(),
            RPCError::new(err_code, format!("Resource not found for URI: {}", uri)),
        )
    }

    fn handle_prompts_list(&self, request: &RPCRequest) -> RPCResponse {
        let mut prompts = vec![
            json!({
                "name": "incident-investigation",
                "description": "Diagnostic workflow for identifying failing downstream microservices and evaluating circuit breaker states.",
                "arguments": [
                    {
                        "name": "service_name",
                        "description": "Microservice identifier to investigate",
                        "required": true
                    }
                ]
            }),
            json!({
                "name": "catalog-schema-audit",
                "description": "Enterprise audit runbook to identify tools with pending review or disabled exposure.",
                "arguments": []
            }),
            json!({
                "name": "canary-rollout",
                "description": "Step-by-step guidance for testing and verifying canary tool deployments.",
                "arguments": [
                    {
                        "name": "server_name",
                        "description": "Server name hosting the canary",
                        "required": true
                    },
                    {
                        "name": "tool_name",
                        "description": "Tool name with canary version enabled",
                        "required": true
                    }
                ]
            }),
        ];

        for s in self.cache.list_servers() {
            for p in &s.external_prompts {
                prompts.push(json!({
                    "name": format!("{}:{}", s.name, p.name),
                    "description": p.description.clone().unwrap_or_default(),
                    "arguments": p.arguments.clone()
                }));
            }
        }

        prompts.sort_by(|a, b| {
            a.get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .cmp(b.get("name").and_then(|n| n.as_str()).unwrap_or(""))
        });

        RPCResponse::ok(
            request.id.clone(),
            json!({
                "prompts": JsonValue::Array(prompts),
                "resultType": "complete",
                "ttlMs": 300000i64,
                "cacheScope": "workspace"
            }),
        )
    }

    fn handle_prompts_get(&self, request: &RPCRequest, caller_actor: &str) -> RPCResponse {
        let prompt_name = match request.params.get("name").and_then(|n| n.as_str()) {
            Some(n) => n,
            None => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(INVALID_PARAMS, "Missing prompt 'name'"),
                )
            }
        };

        let args = request
            .params
            .get("arguments")
            .cloned()
            .unwrap_or_else(JsonValue::object);

        if let Some((srv_name, upstream_prompt)) = prompt_name.split_once(':') {
            if let Some(server) = self.cache.get_server(srv_name) {
                if !server.charter_policy.is_actor_allowed(caller_actor) {
                    return RPCResponse::err(
                        request.id.clone(),
                        RPCError::new(
                            crate::models::UNAUTHORIZED,
                            format!(
                                "Actor '{}' is not authorized to invoke prompts on server '{}'",
                                caller_actor, srv_name
                            ),
                        ),
                    );
                }
                if let Some(ref pool) = self.gateway.translator.upstream_manager {
                    match pool.call_rpc_for_server(
                        &server,
                        "prompts/get",
                        json!({
                            "name": upstream_prompt,
                            "arguments": args
                        }),
                    ) {
                        Ok(r) => {
                            let final_res = if server.charter_policy.require_redaction {
                                crate::data_plane::PIIRedactor::redact_payload(&r)
                            } else {
                                r
                            };
                            return RPCResponse::ok(request.id.clone(), final_res);
                        }
                        Err(e) => {
                            return RPCResponse::err(
                                request.id.clone(),
                                RPCError::new(crate::models::INTERNAL_ERROR, e),
                            )
                        }
                    }
                }
            }
        }

        let messages = match prompt_name {
            "incident-investigation" => {
                let srv = args
                    .get("service_name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("target-service");
                vec![json!({
                    "role": "user",
                    "content": {
                        "type": "text",
                        "text": format!("Please investigate service '{}'. Step 1: Call 'explain_error' or check circuit breaker status. Step 2: Query recent trace spans via '/api/traces'. Step 3: Check fallback tool availability.", srv)
                    }
                })]
            }
            "catalog-schema-audit" => {
                vec![json!({
                    "role": "user",
                    "content": {
                        "type": "text",
                        "text": "Audit the entire Omni MCP catalog: identify all tools where status is 'PENDING_REVIEW' or 'DISABLED', inspect their schema completeness, and verify compliance."
                    }
                })]
            }
            "canary-rollout" => {
                let srv = args
                    .get("server_name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("service");
                let tool = args
                    .get("tool_name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("tool");
                vec![json!({
                    "role": "user",
                    "content": {
                        "type": "text",
                        "text": format!("Perform a verified canary test for '{}/{}'. Send test payloads with synthetic tokens, verify that canary percentage routing executes without error, and verify the SHA-256 Merkle audit trail.", srv, tool)
                    }
                })]
            }
            other => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(METHOD_NOT_FOUND, format!("Prompt '{}' not found", other)),
                )
            }
        };

        RPCResponse::ok(
            request.id.clone(),
            json!({
                "description": format!("Prompt instructions for {}", prompt_name),
                "messages": JsonValue::Array(messages),
                "resultType": "complete"
            }),
        )
    }

    fn call_discover_server(&self, request: &RPCRequest, args: &JsonValue) -> RPCResponse {
        let query = args
            .get("query")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let limit = args.get("limit").and_then(|x| x.as_u64()).unwrap_or(5) as usize;

        let servers = self.cache.list_servers();
        let mut corpus_fields: Vec<Vec<(f64, String)>> = Vec::with_capacity(servers.len());

        for s in &servers {
            let tool_names_and_descs: String = s
                .tools
                .values()
                .map(|t| format!("{} {} {}", t.name, t.description, t.tags.join(" ")))
                .collect::<Vec<_>>()
                .join(" ");
            corpus_fields.push(vec![
                (3.5, s.name.clone()),
                (3.0, s.display_name.clone()),
                (2.0, s.description.clone()),
                (1.5, tool_names_and_descs),
            ]);
        }

        let bm25_scores = CodeModeTypeGenerator::bm25_score_corpus(&query, &corpus_fields);
        let results: Vec<JsonValue> = if query.is_empty() {
            servers
                .iter()
                .take(limit)
                .map(|s| {
                    json!({
                        "server_name": s.name.clone(),
                        "display_name": s.display_name.clone(),
                        "description": s.description.clone(),
                        "owning_team": s.owning_team.clone(),
                        "protocol": s.protocol.as_str(),
                        "version": s.version as i64,
                        "enabled_tools_count": s.enabled_tools_count() as i64,
                        "relevance_score": 0.0
                    })
                })
                .collect()
        } else {
            let hybrid_docs: Vec<crate::data_plane::HybridDocument> = servers
                .iter()
                .zip(bm25_scores.iter())
                .enumerate()
                .map(|(idx, (s, &bm25))| {
                    let text = corpus_fields[idx]
                        .iter()
                        .map(|(_, f)| f.as_str())
                        .collect::<Vec<_>>()
                        .join(" ");
                    crate::data_plane::HybridDocument {
                        id: s.name.clone(),
                        text,
                        bm25_score: bm25,
                    }
                })
                .collect();
            let embedding_url = crate::config::get_config().embedding_url;
            let ranked = crate::data_plane::rank_hybrid_rrf(
                &query,
                &hybrid_docs,
                1.0,
                1.0,
                0.22,
                embedding_url.as_deref(),
            );
            let server_map: std::collections::HashMap<&str, &crate::models::ServerRecord> =
                servers.iter().map(|s| (s.name.as_str(), s)).collect();
            ranked
                .into_iter()
                .take(limit)
                .filter_map(|item| {
                    let s = server_map.get(item.id.as_str())?;
                    let score_display = if item.bm25_score > 0.0 {
                        (item.bm25_score * 100.0).round() / 100.0
                    } else {
                        ((item.vector_similarity * 10.0) * 100.0).round() / 100.0
                    };
                    Some(json!({
                        "server_name": s.name.clone(),
                        "display_name": s.display_name.clone(),
                        "description": s.description.clone(),
                        "owning_team": s.owning_team.clone(),
                        "protocol": s.protocol.as_str(),
                        "version": s.version as i64,
                        "enabled_tools_count": s.enabled_tools_count() as i64,
                        "relevance_score": score_display,
                        "rrf_score": (item.rrf_score * 10000.0).round() / 10000.0,
                        "vector_similarity": (item.vector_similarity * 1000.0).round() / 1000.0
                    }))
                })
                .collect()
        };

        let payload = json!({
            "servers": JsonValue::Array(results),
            "total_matches": servers.len() as i64,
            "ranking_engine": "hybrid_bm25_vector_rrf"
        });

        RPCResponse::ok(
            request.id.clone(),
            json!({
                "content": [
                    {
                        "type": "text",
                        "text": payload.to_string_pretty()
                    }
                ],
                "resultType": "complete"
            }),
        )
    }

    fn call_discover_tools(&self, request: &RPCRequest, args: &JsonValue) -> RPCResponse {
        let server_name = match args.get("server_name").and_then(|x| x.as_str()) {
            Some(n) => n,
            None => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(INVALID_PARAMS, "Missing 'server_name'"),
                )
            }
        };

        let server = match self.cache.get_server(server_name) {
            Some(s) => s,
            None => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(
                        METHOD_NOT_FOUND,
                        format!("Server '{}' not found", server_name),
                    ),
                )
            }
        };

        let mut tools = Vec::new();
        for tool in server.tools.values() {
            if tool.status == ToolStatus::Enabled {
                tools.push(json!({
                    "name": tool.name.clone(),
                    "description": tool.description.clone(),
                    "protocol": tool.protocol.as_str(),
                    "version": tool.version as i64,
                    "idempotent_cacheable": tool.idempotent_cacheable,
                    "tags": tool.tags.iter().map(|s| JsonValue::from(s.as_str())).collect::<Vec<_>>(),
                    "is_mutation": tool.is_mutation
                }));
            }
        }

        let payload = json!({
            "server_name": server_name,
            "tools": JsonValue::Array(tools)
        });

        RPCResponse::ok(
            request.id.clone(),
            json!({
                "content": [
                    {
                        "type": "text",
                        "text": payload.to_string_pretty()
                    }
                ],
                "resultType": "complete"
            }),
        )
    }

    fn call_get_tool_schema(&self, request: &RPCRequest, args: &JsonValue) -> RPCResponse {
        let server_name = match args.get("server_name").and_then(|x| x.as_str()) {
            Some(n) => n,
            None => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(INVALID_PARAMS, "Missing 'server_name'"),
                )
            }
        };

        let tool_name = match args.get("tool_name").and_then(|x| x.as_str()) {
            Some(n) => n,
            None => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(INVALID_PARAMS, "Missing 'tool_name'"),
                )
            }
        };

        let server = match self.cache.get_server(server_name) {
            Some(s) => s,
            None => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(
                        METHOD_NOT_FOUND,
                        format!("Server '{}' not found", server_name),
                    ),
                )
            }
        };

        let tool = match server.tools.get(tool_name) {
            Some(t) => t,
            None => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(
                        METHOD_NOT_FOUND,
                        format!("Tool '{}' not found on server '{}'", tool_name, server_name),
                    ),
                )
            }
        };

        let payload = json!({
            "server_name": server_name,
            "tool_name": tool_name,
            "description": tool.description.clone(),
            "inputSchema": tool.input_schema.clone(),
            "outputSchema": tool.output_schema.clone(),
            "status": tool.status.as_str(),
            "canary_version": tool.canary_version.map(|v| v as i64),
            "fallback_tool": tool.fallback_tool.clone()
        });

        RPCResponse::ok(
            request.id.clone(),
            json!({
                "content": [
                    {
                        "type": "text",
                        "text": payload.to_string_pretty()
                    }
                ],
                "resultType": "complete"
            }),
        )
    }

    fn call_invoke_tool(
        &self,
        request: &RPCRequest,
        args: &JsonValue,
        caller_actor: &str,
        user_token: Option<&str>,
    ) -> RPCResponse {
        let server_name = match args.get("server_name").and_then(|x| x.as_str()) {
            Some(n) => n,
            None => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(INVALID_PARAMS, "Missing 'server_name'"),
                )
            }
        };

        let tool_name = match args.get("tool_name").and_then(|x| x.as_str()) {
            Some(n) => n,
            None => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(INVALID_PARAMS, "Missing 'tool_name'"),
                )
            }
        };

        let mut tool_args = args
            .get("arguments")
            .cloned()
            .unwrap_or_else(JsonValue::object);

        // v6.8.0 Experimental File Uploads (io.modelcontextprotocol/files):
        // Resolve `x-mcp-file` properties (inline <= 256 KiB or existing fileId), or emit
        // SEP-2356 `resultType: "upload_required"` MRTR if a required file argument is missing.
        let cfg = crate::config::get_config();
        if cfg.enable_file_uploads_extension {
            if let Some(srv) = self.cache.get_server_arc(server_name) {
                if let Some(tool) = srv.tools.get(tool_name) {
                    let file_props = find_mcp_file_properties(&tool.input_schema);
                    if !file_props.is_empty() {
                        let required_fields: Vec<&str> = tool
                            .input_schema
                            .get("required")
                            .and_then(|r| r.as_array())
                            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                            .unwrap_or_default();
                        for field in file_props {
                            match tool_args.get(&field).cloned() {
                                Some(val) if !val.is_null() => {
                                    match resolve_file_argument(
                                        &self.file_store,
                                        &val,
                                        caller_actor,
                                    ) {
                                        Ok(resolved) => {
                                            tool_args.insert(field, resolved);
                                        }
                                        Err(e) => {
                                            return RPCResponse::err(
                                                request.id.clone(),
                                                RPCError::new(INVALID_PARAMS, e),
                                            );
                                        }
                                    }
                                }
                                _ if required_fields.contains(&field.as_str()) => {
                                    return RPCResponse::ok(
                                        request.id.clone(),
                                        build_upload_required_mrtr(
                                            &cfg.public_base_url,
                                            &field,
                                            cfg.file_upload_max_bytes,
                                        ),
                                    );
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }

        if let Some(proj) = args.get("_projection").cloned() {
            tool_args.insert("_projection", proj);
        }

        let traceparent = args
            .get("_meta")
            .and_then(|m| m.get("traceparent"))
            .or_else(|| {
                request
                    .params
                    .get("_meta")
                    .and_then(|m| m.get("traceparent"))
            })
            .and_then(|tp| tp.as_str());

        let client_caps = ClientCapabilitiesMeta::from_params(&request.params);
        let call_req = RPCRequest::new(
            request.id.clone(),
            "tools/call",
            json!({
                "name": tool_name,
                "arguments": tool_args,
            }),
        );

        // v6.8.0 URL-mode Elicitation (Milestone 4):
        // If the target server is an external StreamableHttp server using OAuth (no static bearer_ref)
        // and no token exists in the shared multi-replica OAuth store, emit `elicitation/create`
        // (`mode: "url"`) when the client declared `elicitation.url`.
        if client_caps.elicitation_url {
            if let Some(srv) = self.cache.get_server_arc(server_name) {
                if let Some(crate::models::UpstreamTransportConfig::StreamableHttp {
                    url: upstream_url,
                    bearer_ref: None,
                    ..
                }) = &srv.upstream_config
                {
                    let tok_store =
                        crate::upstream::UpstreamOAuthTokenStore::new(&cfg.secrets_dir, None)
                            .with_registry(self.cache.registry.clone());
                    if tok_store.load_token(server_name).is_err() {
                        return self.build_url_elicitation_response(
                            request.id.clone(),
                            server_name,
                            upstream_url,
                            &cfg,
                        );
                    }
                }
            }
        }

        let is_long_running = self
            .cache
            .get_server_arc(server_name)
            .and_then(|srv| srv.tools.get(tool_name).cloned())
            .map(|t| {
                t.metadata
                    .get("long_running")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                    || t.metadata
                        .get("taskSupport")
                        .and_then(|v| v.as_str())
                        .map(|s| s == "optional" || s == "required")
                        .unwrap_or(false)
                    || t.tags
                        .iter()
                        .any(|tag| tag == "long_running" || tag == "long-running")
            })
            .unwrap_or(false);
        let force_async_tool = request.params.get("task").is_some();
        let resp = if cfg.enable_tasks_extension
            && (force_async_tool || (client_caps.tasks && is_long_running))
        {
            let mut gw = self.gateway.clone();
            gw.task_manager = None;
            let srv_owned = server_name.to_string();
            let req_owned = call_req.clone();
            let actor_owned = caller_actor.to_string();
            let tok_owned = user_token.map(String::from);
            let tp_owned = traceparent.map(String::from);
            let grace_ms = if force_async_tool {
                Some(0)
            } else {
                Some(cfg.task_sync_grace_ms)
            };
            match self.task_manager.run_with_grace(
                caller_actor,
                None,
                Some(server_name.to_string()),
                force_async_tool,
                grace_ms,
                move |_cancel_flag| {
                    let r = gw.handle_request_with_trace(
                        &srv_owned,
                        &req_owned,
                        &actor_owned,
                        tok_owned.as_deref(),
                        tp_owned.as_deref(),
                    );
                    if let Some(err) = r.error {
                        Err((err.code, err.message))
                    } else {
                        Ok(r.result.unwrap_or_else(JsonValue::object))
                    }
                },
            ) {
                Ok(TaskDispatchOutcome::PromotedAsync(task)) => {
                    return RPCResponse::ok(
                        request.id.clone(),
                        json!({
                            "resultType": "task",
                            "taskId": task.task_id,
                            "status": task.status.as_str(),
                            "pollInterval": task.poll_interval_ms as i64,
                            "statusMessage": task.status_message.unwrap_or_else(|| "Task running".to_string())
                        }),
                    );
                }
                Ok(TaskDispatchOutcome::CompletedSync(val)) => {
                    RPCResponse::ok(request.id.clone(), val)
                }
                Err(e) => RPCResponse::err(request.id.clone(), RPCError::new(INTERNAL_ERROR, e)),
            }
        } else {
            self.gateway.handle_request_with_trace(
                server_name,
                &call_req,
                caller_actor,
                user_token,
                traceparent,
            )
        };

        // Fallback URL-mode elicitation if upstream returned HTTP 401 / UNAUTHORIZED
        if client_caps.elicitation_url {
            if let Some(ref err) = resp.error {
                let is_oauth_401 = err.code == UNAUTHORIZED
                    || err.message.contains("401")
                    || err.message.to_lowercase().contains("unauthorized");
                if is_oauth_401 {
                    if let Some(srv) = self.cache.get_server_arc(server_name) {
                        if let Some(crate::models::UpstreamTransportConfig::StreamableHttp {
                            url: upstream_url,
                            ..
                        }) = &srv.upstream_config
                        {
                            return self.build_url_elicitation_response(
                                request.id.clone(),
                                server_name,
                                upstream_url,
                                &cfg,
                            );
                        }
                    }
                }
            }
        }
        resp
    }

    fn build_url_elicitation_response(
        &self,
        req_id: JsonValue,
        server_name: &str,
        upstream_url: &str,
        cfg: &crate::config::OmniConfig,
    ) -> RPCResponse {
        let (verifier, _challenge) = crate::upstream::UpstreamOAuthTokenStore::generate_pkce_pair();
        let state = format!("st_{}", crate::models::generate_ulid());
        let elicitation_id = format!("elicit_{}", crate::models::generate_ulid());
        let base = cfg.public_base_url.trim_end_matches('/');
        let redirect_uri = format!("{}/api/v1/oauth/callback", base);
        let now_ts = crate::logging::now_epoch_secs();
        self.cache
            .registry
            .save_oauth_flow(&crate::models::OAuthFlowRecord {
                state: state.clone(),
                server_name: server_name.to_string(),
                elicitation_id: Some(elicitation_id.clone()),
                code_verifier: verifier,
                token_url: format!("{}/oauth/token", upstream_url.trim_end_matches('/')),
                client_id: "omni-mcp-gateway".to_string(),
                redirect_uri,
                created_at: now_ts,
                expires_at: now_ts + 600.0,
            });
        let elicit_url = format!(
            "{}/api/v1/oauth/start?server={}&state={}&elicitation_id={}",
            base, server_name, state, elicitation_id
        );
        let input_requests = json!([
            {
                "method": "elicitation/create",
                "params": {
                    "mode": "url",
                    "elicitationId": elicitation_id.clone(),
                    "url": elicit_url,
                    "message": format!(
                        "OAuth authorization required for '{}'. Complete sign-in in your browser and retry.",
                        server_name
                    )
                }
            }
        ]);
        RPCResponse::ok(
            req_id,
            json!({
                "resultType": "input_required",
                "requestState": elicitation_id,
                "inputRequests": input_requests,
                "isError": false
            }),
        )
    }

    fn call_batch_invoke_tools(
        &self,
        request: &RPCRequest,
        args: &JsonValue,
        caller_actor: &str,
        user_token: Option<&str>,
    ) -> RPCResponse {
        if let Some(meta) = request.params.get("_meta").or_else(|| args.get("_meta")) {
            if let Some(ver) = meta
                .get("io.modelcontextprotocol/protocolVersion")
                .or_else(|| meta.get("protocolVersion"))
                .and_then(|v| v.as_str())
            {
                if let Some(caps) = crate::models::protocol_capabilities(ver) {
                    if !caps.allows_batching {
                        return RPCResponse::err(
                            request.id.clone(),
                            RPCError::new(
                                crate::models::INVALID_REQUEST,
                                format!(
                                    "Batch tool execution is removed and not supported in protocol version {}",
                                    ver
                                ),
                            ),
                        );
                    }
                }
            }
        }
        let calls = match args.get("calls").and_then(|c| c.as_array()) {
            Some(arr) => arr,
            None => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(
                        INVALID_PARAMS,
                        "Missing 'calls' array in batch_invoke_tools",
                    ),
                )
            }
        };

        let mut results = Vec::with_capacity(calls.len());
        for (idx, call_item) in calls.iter().enumerate() {
            let sub_req = RPCRequest::new((idx + 1) as i64, "tools/call", call_item.clone());
            let resp = self.call_invoke_tool(&sub_req, call_item, caller_actor, user_token);
            results.push(resp.to_json());
        }

        let payload = json!({
            "batch_results": JsonValue::Array(results),
        });

        RPCResponse::ok(
            request.id.clone(),
            json!({
                "content": [
                    {
                        "type": "text",
                        "text": payload.to_string_pretty()
                    }
                ],
                "resultType": "complete"
            }),
        )
    }

    fn call_recommend_tools(&self, request: &RPCRequest, args: &JsonValue) -> RPCResponse {
        let goal = match args.get("goal").and_then(|x| x.as_str()) {
            Some(g) if !g.is_empty() => g,
            _ => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(INVALID_PARAMS, "Missing 'goal' in recommend_tools"),
                )
            }
        };
        let limit = args.get("limit").and_then(|x| x.as_u64()).unwrap_or(5) as usize;

        let servers = self.cache.list_servers();
        let mut active_tools: Vec<(String, String, String, bool, Vec<String>)> = Vec::new();
        let mut corpus_fields: Vec<Vec<(f64, String)>> = Vec::new();

        for s in &servers {
            for tool in s.tools.values() {
                if tool.status != ToolStatus::Enabled {
                    continue;
                }
                let schema_keys: String = tool
                    .input_schema
                    .get("properties")
                    .and_then(|p| p.as_object())
                    .map(|m| {
                        m.iter()
                            .map(|(k, _)| k.clone())
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();

                corpus_fields.push(vec![
                    (3.5, tool.name.clone()),
                    (2.5, s.name.clone()),
                    (3.0, tool.tags.join(" ")),
                    (1.8, tool.description.clone()),
                    (1.2, schema_keys),
                ]);
                active_tools.push((
                    s.name.clone(),
                    tool.name.clone(),
                    tool.description.clone(),
                    tool.is_mutation,
                    tool.tags.clone(),
                ));
            }
        }

        let bm25_scores = CodeModeTypeGenerator::bm25_score_corpus(goal, &corpus_fields);
        let hybrid_docs: Vec<crate::data_plane::HybridDocument> = active_tools
            .iter()
            .zip(bm25_scores.iter())
            .enumerate()
            .map(|(idx, ((s_name, t_name, _, _, _), &bm25))| {
                let text = corpus_fields[idx]
                    .iter()
                    .map(|(_, f)| f.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                crate::data_plane::HybridDocument {
                    id: format!("{}::{}", s_name, t_name),
                    text,
                    bm25_score: bm25,
                }
            })
            .collect();
        let embedding_url = crate::config::get_config().embedding_url;
        let ranked = crate::data_plane::rank_hybrid_rrf(
            goal,
            &hybrid_docs,
            1.0,
            1.0,
            0.22,
            embedding_url.as_deref(),
        );
        let tool_lookup: std::collections::HashMap<
            String,
            (String, String, String, bool, Vec<String>),
        > = active_tools
            .into_iter()
            .map(|item| (format!("{}::{}", item.0, item.1), item))
            .collect();

        let mut pipeline = Vec::new();
        for (idx, item) in ranked.into_iter().take(limit).enumerate() {
            if let Some((s_name, t_name, desc, is_mut, tags)) = tool_lookup.get(&item.id) {
                pipeline.push(json!({
                    "step": (idx + 1) as i64,
                    "server_name": s_name.clone(),
                    "tool_name": t_name.clone(),
                    "description": desc.clone(),
                    "is_mutation": *is_mut,
                    "bm25_score": (item.bm25_score * 100.0).round() / 100.0,
                    "vector_similarity": (item.vector_similarity * 1000.0).round() / 1000.0,
                    "rrf_score": (item.rrf_score * 10000.0).round() / 10000.0,
                    "tags": tags.iter().map(|s| JsonValue::from(s.as_str())).collect::<Vec<_>>()
                }));
            }
        }

        let payload = json!({
            "goal": goal,
            "recommended_pipeline": JsonValue::Array(pipeline),
            "ranking_engine": "hybrid_bm25_vector_rrf",
            "confidence": 0.95
        });

        RPCResponse::ok(
            request.id.clone(),
            json!({
                "content": [
                    {
                        "type": "text",
                        "text": payload.to_string_pretty()
                    }
                ],
                "resultType": "complete"
            }),
        )
    }

    fn call_explain_error(&self, request: &RPCRequest, args: &JsonValue) -> RPCResponse {
        let server_name = match args.get("server_name").and_then(|x| x.as_str()) {
            Some(s) => s,
            None => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(INVALID_PARAMS, "Missing 'server_name'"),
                )
            }
        };
        let tool_name = match args.get("tool_name").and_then(|x| x.as_str()) {
            Some(t) => t,
            None => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(INVALID_PARAMS, "Missing 'tool_name'"),
                )
            }
        };
        let error_message = args
            .get("error_message")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let err_lower = error_message.to_lowercase();

        let server_opt = self.cache.get_server(server_name);
        let cb_open = self
            .gateway
            .circuit_breaker
            .allow_request(server_name)
            .is_err();

        let (tool_status, fallback_tool, is_canary) = if let Some(ref s) = server_opt {
            if let Some(t) = s.tools.get(tool_name) {
                (
                    t.status.as_str(),
                    t.fallback_tool.clone(),
                    t.canary_version.is_some(),
                )
            } else {
                ("NOT_FOUND", None, false)
            }
        } else {
            ("SERVER_NOT_FOUND", None, false)
        };

        let (error_code, root_cause, suggested_action) = if cb_open
            || err_lower.contains("circuit_open")
            || err_lower.contains("-32030")
        {
            (
                CIRCUIT_OPEN,
                "Circuit breaker tripped open (-32030 CIRCUIT_OPEN) due to consecutive downstream microservice failures."
                    .to_string(),
                "Wait for the circuit breaker cooldown reset window (typically 30s) or invoke the configured fallback tool."
                    .to_string(),
            )
        } else if err_lower.contains("approval_required") || err_lower.contains("-32031") {
            (
                APPROVAL_REQUIRED,
                "Mutating tool call paused at a human-in-the-loop approval gate (-32031 APPROVAL_REQUIRED)."
                    .to_string(),
                "Approve and resume the suspended execution frame via `resume_codemode` or `aifx mcp resume <execution_id>`."
                    .to_string(),
            )
        } else if err_lower.contains("fuelexhausted")
            || err_lower.contains("fuel_exhausted")
            || err_lower.contains("-32032")
        {
            (
                FUEL_EXHAUSTED,
                "Code Mode script exceeded its deterministic instruction fuel budget (-32032 FUEL_EXHAUSTED)."
                    .to_string(),
                "Refactor unbounded loops, use vectorized `.filter()`/`.map()` pushdowns, or raise `max_operations`."
                    .to_string(),
            )
        } else if err_lower.contains("globaloutboundblocked")
            || err_lower.contains("outbound_blocked")
            || err_lower.contains("-32033")
        {
            (
                OUTBOUND_BLOCKED,
                "Direct network or filesystem call blocked by `globalOutbound: null` sandbox isolation (-32033 OUTBOUND_BLOCKED)."
                    .to_string(),
                "Route all external I/O exclusively through governed `codemode.<server>.<tool>()` bindings."
                    .to_string(),
            )
        } else if err_lower.contains("saga_rolled_back")
            || err_lower.contains("saga_compensated")
            || err_lower.contains("-32034")
        {
            (
                SAGA_COMPENSATED,
                "Multi-step script failed mid-flight; prior completed mutations were automatically unwound in LIFO order (-32034 SAGA_COMPENSATED)."
                    .to_string(),
                "Inspect `saga_compensations` receipts and fix the failing downstream call before retrying."
                    .to_string(),
            )
        } else if err_lower.contains("schema_preflight_failed") || err_lower.contains("-32035") {
            (
                SCHEMA_PREFLIGHT_FAILED,
                "Tool invocation arguments failed schema preflight validation (-32035 SCHEMA_PREFLIGHT_FAILED)."
                    .to_string(),
                "Call `get_tool_schema` or `codemode.describe()` to verify required parameter names and types."
                    .to_string(),
            )
        } else if err_lower.contains("memory_exhausted") || err_lower.contains("-32036") {
            (
                MEMORY_EXHAUSTED,
                "Code Mode script exceeded maximum memory allocation budget (-32036 MEMORY_EXHAUSTED)."
                    .to_string(),
                "Reduce array/object allocations, process items in smaller batches, or raise max_memory_bytes."
                    .to_string(),
            )
        } else if err_lower.contains("timeout") || err_lower.contains("-32037") {
            (
                TIMEOUT,
                "Code Mode script exceeded wall-clock execution time limit (-32037 TIMEOUT)."
                    .to_string(),
                "Optimize long-running loops, parallelize tool calls with ParallelWave, or raise max_execution_time_ms."
                    .to_string(),
            )
        } else if err_lower.contains("approval_invalid") || err_lower.contains("-32038") {
            (
                APPROVAL_INVALID,
                "Approval continuation token verification failed or expired (-32038 APPROVAL_INVALID)."
                    .to_string(),
                "Ensure HMAC-SHA256 signature is valid, approver is authenticated, arguments have not been altered, and token has not expired."
                    .to_string(),
            )
        } else if tool_status == "DISABLED" {
            (
                TOOL_DISABLED,
                "Tool is discovered but disabled in the catalog ('discovery does not imply exposure')."
                    .to_string(),
                "Request tool enablement from the service owner via the control plane API `/api/tools/toggle_status`."
                    .to_string(),
            )
        } else if tool_status == "NOT_FOUND" || tool_status == "SERVER_NOT_FOUND" {
            (
                METHOD_NOT_FOUND,
                format!("Target '{}' is not registered in the active catalog.", tool_name),
                "Execute 'discover_server' and 'discover_tools' to locate valid active tool identifiers."
                    .to_string(),
            )
        } else if err_lower.contains("unauthorized") {
            (
                UNAUTHORIZED,
                "ABAC security charter rejection for the caller role.".to_string(),
                "Verify x-caller-actor header or ensure proper OAuth token is provided in the Authorization header."
                    .to_string(),
            )
        } else {
            (
                -32003,
                format!("Downstream execution failure: {}", error_message),
                if fallback_tool.is_some() {
                    "Resilient fallback tool is available; retry with fallback or check downstream logs.".to_string()
                } else {
                    "Inspect downstream service metrics or verify request payload arguments against tool schema.".to_string()
                },
            )
        };

        let payload = json!({
            "server_name": server_name,
            "tool_name": tool_name,
            "tool_status": tool_status,
            "error_code": error_code as i64,
            "circuit_breaker_open": cb_open,
            "canary_active": is_canary,
            "fallback_tool": fallback_tool.map(JsonValue::from).unwrap_or(JsonValue::Null),
            "root_cause_analysis": root_cause,
            "suggested_action": suggested_action
        });

        RPCResponse::ok(
            request.id.clone(),
            json!({
                "content": [
                    {
                        "type": "text",
                        "text": payload.to_string_pretty()
                    }
                ],
                "resultType": "complete"
            }),
        )
    }

    fn call_codemode(
        &self,
        request: &RPCRequest,
        args: &JsonValue,
        caller_actor: &str,
        user_token: Option<&str>,
    ) -> RPCResponse {
        let code = match args.get("code").and_then(|x| x.as_str()) {
            Some(c) if !c.trim().is_empty() => c,
            _ => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(INVALID_PARAMS, "Missing 'code' string in codemode"),
                )
            }
        };

        let require_mutation_approval = args
            .get("require_mutation_approval")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);

        let mut limits = SandboxLimits::default();
        if let Some(max_ops) = args.get("max_operations").and_then(|x| x.as_u64()) {
            limits.max_operations = max_ops.clamp(100, 500_000);
        }

        let client_caps = ClientCapabilitiesMeta::from_params(&request.params);
        let cfg = crate::config::get_config();
        let force_async = cfg.enable_tasks_extension
            && (request.params.get("task").is_some()
                || args.get("async").and_then(|v| v.as_bool()).unwrap_or(false));
        let use_tasks_grace = cfg.enable_tasks_extension && (force_async || client_caps.tasks);

        if use_tasks_grace {
            let gw = self.gateway.clone();
            let code_owned = code.to_string();
            let actor_owned = caller_actor.to_string();
            let token_owned = user_token.map(String::from);
            let grace_ms = if force_async {
                Some(0)
            } else {
                Some(cfg.task_sync_grace_ms)
            };
            match self.task_manager.run_with_grace(
                caller_actor,
                None,
                None,
                force_async,
                grace_ms,
                move |cancel_flag| {
                    let out = gw.execute_codemode_with_cancel(
                        &code_owned,
                        Some(limits),
                        require_mutation_approval,
                        &actor_owned,
                        token_owned.as_deref(),
                        None,
                        Some(cancel_flag),
                    );
                    if out.status == "ERROR" {
                        Err((
                            out.error_code.unwrap_or(INTERNAL_ERROR),
                            out.error
                                .unwrap_or_else(|| "Code Mode execution failed".to_string()),
                        ))
                    } else {
                        Ok(out.to_json())
                    }
                },
            ) {
                Ok(TaskDispatchOutcome::PromotedAsync(task)) => {
                    return RPCResponse::ok(
                        request.id.clone(),
                        json!({
                            "resultType": "task",
                            "taskId": task.task_id,
                            "status": task.status.as_str(),
                            "pollInterval": task.poll_interval_ms as i64,
                            "statusMessage": task.status_message.unwrap_or_else(|| "Task running".to_string())
                        }),
                    );
                }
                Ok(TaskDispatchOutcome::CompletedSync(val)) => {
                    let is_approval =
                        val.get("status").and_then(|s| s.as_str()) == Some("APPROVAL_REQUIRED");
                    if !is_approval {
                        return RPCResponse::ok(
                            request.id.clone(),
                            json!({
                                "content": [{"type": "text", "text": val.to_string_pretty()}],
                                "resultType": "complete",
                                "isError": false
                            }),
                        );
                    }
                    let exec_id = val
                        .get("execution_id")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    return RPCResponse::ok(
                        request.id.clone(),
                        json!({
                            "content": [{"type": "text", "text": val.to_string_pretty()}],
                            "resultType": "input_required",
                            "requestState": exec_id.map(JsonValue::from).unwrap_or(JsonValue::Null),
                            "isError": false
                        }),
                    );
                }
                Err(e) => {
                    return RPCResponse::err(request.id.clone(), RPCError::new(INTERNAL_ERROR, e));
                }
            }
        }

        let output = self.gateway.execute_codemode(
            code,
            Some(limits),
            require_mutation_approval,
            caller_actor,
            user_token,
            None,
        );

        let is_err = output.status == "ERROR";
        let is_approval = output.status == "APPROVAL_REQUIRED";
        let mut payload = output.to_json();
        let mut input_requests: Option<JsonValue> = None;

        if is_approval {
            payload.insert("resultType", "input_required");
            if let Some(ref frame) = output.suspended_frame {
                payload.insert("requestState", frame.execution_id.clone());
                if client_caps.elicitation_form {
                    let schema = json!({
                        "type": "object",
                        "properties": {
                            "approved": {
                                "type": "boolean",
                                "description": "Confirm whether to approve this mutating operation"
                            },
                            "justification": {
                                "type": "string",
                                "description": "Audit justification recorded in Merkle audit log"
                            }
                        },
                        "required": ["approved"]
                    });
                    if ElicitationPolicyRule::validate_elicitation_schema(&schema).is_ok() {
                        let reqs = json!([
                            {
                                "method": "elicitation/create",
                                "params": {
                                    "mode": "form",
                                    "message": format!(
                                        "Approve mutating tool call '{}.{}'? ({})",
                                        frame.pending_server,
                                        frame.pending_tool,
                                        frame.pending_reason
                                    ),
                                    "requestedSchema": schema
                                }
                            }
                        ]);
                        let mut task = McpTaskRecord::new(
                            frame.execution_id.clone(),
                            cfg.pod_id.clone(),
                            caller_actor,
                            None,
                            Some(frame.pending_server.clone()),
                            cfg.task_ttl_sec,
                        );
                        task.status = McpTaskStatus::InputRequired;
                        task.status_message = Some(frame.pending_reason.clone());
                        task.input_requests = Some(reqs.clone());
                        self.task_manager.record_task(&task);
                        input_requests = Some(reqs);
                    }
                }
            }
        } else {
            payload.insert("resultType", "complete");
        }

        let mut resp_obj = json!({
            "content": [
                {
                    "type": "text",
                    "text": payload.to_string_pretty()
                }
            ],
            "resultType": if is_approval { "input_required" } else { "complete" },
            "requestState": if is_approval { output.suspended_frame.as_ref().map(|f| JsonValue::from(f.execution_id.as_str())).unwrap_or(JsonValue::Null) } else { JsonValue::Null },
            "isError": is_err
        });
        if let Some(reqs) = input_requests {
            resp_obj.insert("inputRequests", reqs);
            if let Some(ref frame) = output.suspended_frame {
                resp_obj.insert("taskId", JsonValue::from(frame.execution_id.as_str()));
            }
        }

        RPCResponse::ok(request.id.clone(), resp_obj)
    }

    fn call_get_codemode_types(&self, request: &RPCRequest, args: &JsonValue) -> RPCResponse {
        if let Some(intent) = args.get("intent").and_then(|x| x.as_str()) {
            let top_k = args.get("top_k").and_then(|x| x.as_u64()).unwrap_or(4) as usize;
            let threshold = args
                .get("confidence_threshold")
                .and_then(|x| x.as_f64())
                .unwrap_or(0.82);
            let spec = self
                .gateway
                .generate_speculative_codemode_types(intent, top_k, threshold);
            let payload = spec.to_json();
            return RPCResponse::ok(
                request.id.clone(),
                json!({
                    "content": [
                        {
                            "type": "text",
                            "text": payload.to_string_pretty()
                        }
                    ],
                    "resultType": "complete"
                }),
            );
        }

        let server_filter = args.get("server_name").and_then(|x| x.as_str());
        let search_query = args.get("query").and_then(|x| x.as_str());
        let gen = self
            .gateway
            .generate_codemode_types(server_filter, search_query);
        let payload = gen.to_json();

        RPCResponse::ok(
            request.id.clone(),
            json!({
                "content": [
                    {
                        "type": "text",
                        "text": payload.to_string_pretty()
                    }
                ],
                "resultType": "complete"
            }),
        )
    }

    fn call_resume_codemode(
        &self,
        request: &RPCRequest,
        args: &JsonValue,
        caller_actor: &str,
        user_token: Option<&str>,
    ) -> RPCResponse {
        let execution_id = match args.get("execution_id").and_then(|x| x.as_str()) {
            Some(id) if !id.trim().is_empty() => id,
            _ => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(
                        INVALID_PARAMS,
                        "Missing 'execution_id' ULID in resume_codemode",
                    ),
                )
            }
        };

        let approver = args
            .get("approver")
            .and_then(|x| x.as_str())
            .unwrap_or(caller_actor);

        let digest = args
            .get("arguments_digest")
            .or_else(|| args.get("pending_arguments_hash"))
            .and_then(|x| x.as_str());

        let output =
            self.gateway
                .resume_codemode(execution_id, approver, None, user_token, None, digest);
        let is_err = output.status == "ERROR";
        let payload = output.to_json();

        RPCResponse::ok(
            request.id.clone(),
            json!({
                "content": [
                    {
                        "type": "text",
                        "text": payload.to_string_pretty()
                    }
                ],
                "resultType": "complete",
                "isError": is_err
            }),
        )
    }

    fn call_simulate_codemode(
        &self,
        request: &RPCRequest,
        args: &JsonValue,
        caller_actor: &str,
        user_token: Option<&str>,
    ) -> RPCResponse {
        let code = match args.get("code").and_then(|x| x.as_str()) {
            Some(c) if !c.trim().is_empty() => c,
            _ => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(INVALID_PARAMS, "Missing 'code' string in simulate_codemode"),
                )
            }
        };

        let cfg = FaultInjectionConfig::from_json(args);
        let report = self
            .gateway
            .simulate_codemode(code, None, cfg, caller_actor, user_token);
        let payload = report.to_json();

        RPCResponse::ok(
            request.id.clone(),
            json!({
                "content": [
                    {
                        "type": "text",
                        "text": payload.to_string_pretty()
                    }
                ],
                "resultType": "complete",
                "isError": false
            }),
        )
    }
}
