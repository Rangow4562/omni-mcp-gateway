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
//! Full MCP Resources (`resources/list`, `resources/read`) and MCP Prompts (`prompts/list`, `prompts/get`).

use crate::data_plane::cache::InMemoryConfigCache;
use crate::data_plane::codemode::{CodeModeTypeGenerator, SandboxLimits};
use crate::data_plane::dst::FaultInjectionConfig;
use crate::data_plane::gateway::MCPProxyGateway;
use crate::json::JsonValue;
use crate::models::{
    RPCBatchOrSingle, RPCError, RPCRequest, RPCResponse, ToolStatus, INVALID_PARAMS,
    METHOD_NOT_FOUND,
};

#[derive(Clone)]
pub struct OmniMCPServer {
    pub cache: InMemoryConfigCache,
    pub gateway: MCPProxyGateway,
}

impl OmniMCPServer {
    pub fn new(cache: InMemoryConfigCache, gateway: MCPProxyGateway) -> Self {
        Self { cache, gateway }
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
                "description": "Omni Code Mode execution sandbox. Write a single JavaScript/TypeScript `async () => { ... }` or Rust/Rhai script to coordinate multi-step tool calls (`await codemode.<server>.<tool>(input)`), loops, filtering, and aggregations in < 1ms without bloating the LLM context window.\n\nAvailable TypeScript Declarations:\n{{types}}",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "code": {
                            "type": "string",
                            "description": "JavaScript/TypeScript `async () => { ... }` or Rust/Rhai script using `codemode.<server>.<tool>(args)` or `codemode.search(query)`"
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
            RPCBatchOrSingle::Single(req) => {
                self.handle_request(&req, caller_actor, user_token).to_json()
            }
            RPCBatchOrSingle::Batch(reqs) => {
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
        match request.method.as_str() {
            "initialize" => RPCResponse::ok(
                request.id.clone(),
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {"listChanged": true},
                        "resources": {"subscribe": false, "listChanged": true},
                        "prompts": {"listChanged": true},
                        "logging": {}
                    },
                    "serverInfo": {
                        "name": "uber-omni-mcp-gateway",
                        "version": "4.5.0",
                        "description": "Omni MCP Autonomous Mesh Gateway & Code Mode Runtime (Rust Engine v4.5)",
                        "runtime": "rust"
                    }
                }),
            ),
            "notifications/initialized" | "ping" => {
                RPCResponse::ok(request.id.clone(), JsonValue::object())
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
                RPCResponse::ok(
                    request.id.clone(),
                    json!({
                        "tools": JsonValue::Array(tools)
                    }),
                )
            }
            "resources/list" => self.handle_resources_list(request),
            "resources/read" => self.handle_resources_read(request),
            "prompts/list" => self.handle_prompts_list(request),
            "prompts/get" => self.handle_prompts_get(request),
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
                    other => RPCResponse::err(
                        request.id.clone(),
                        RPCError::new(
                            METHOD_NOT_FOUND,
                            format!("Unknown Omni MCP tool: {}", other),
                        ),
                    ),
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
        }

        RPCResponse::ok(
            request.id.clone(),
            json!({
                "resources": JsonValue::Array(resources)
            }),
        )
    }

    fn handle_resources_read(&self, request: &RPCRequest) -> RPCResponse {
        let uri = match request.params.get("uri").and_then(|u| u.as_str()) {
            Some(u) => u,
            None => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(INVALID_PARAMS, "Missing 'uri' parameter in resources/read"),
                )
            }
        };

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
                    ]
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
                    ]
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
                        ]
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
                        ]
                    }),
                );
            }
        }

        RPCResponse::err(
            request.id.clone(),
            RPCError::new(
                METHOD_NOT_FOUND,
                format!("Resource not found for URI: {}", uri),
            ),
        )
    }

    fn handle_prompts_list(&self, request: &RPCRequest) -> RPCResponse {
        let prompts = vec![
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

        RPCResponse::ok(
            request.id.clone(),
            json!({
                "prompts": JsonValue::Array(prompts)
            }),
        )
    }

    fn handle_prompts_get(&self, request: &RPCRequest) -> RPCResponse {
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

        let messages = match prompt_name {
            "incident-investigation" => {
                let srv = args.get("service_name").and_then(|x| x.as_str()).unwrap_or("target-service");
                vec![
                    json!({
                        "role": "user",
                        "content": {
                            "type": "text",
                            "text": format!("Please investigate service '{}'. Step 1: Call 'explain_error' or check circuit breaker status. Step 2: Query recent trace spans via '/api/traces'. Step 3: Check fallback tool availability.", srv)
                        }
                    })
                ]
            }
            "catalog-schema-audit" => {
                vec![
                    json!({
                        "role": "user",
                        "content": {
                            "type": "text",
                            "text": "Audit the entire Omni MCP catalog: identify all tools where status is 'PENDING_REVIEW' or 'DISABLED', inspect their schema completeness, and verify compliance."
                        }
                    })
                ]
            }
            "canary-rollout" => {
                let srv = args.get("server_name").and_then(|x| x.as_str()).unwrap_or("service");
                let tool = args.get("tool_name").and_then(|x| x.as_str()).unwrap_or("tool");
                vec![
                    json!({
                        "role": "user",
                        "content": {
                            "type": "text",
                            "text": format!("Perform a verified canary test for '{}/{}'. Send test payloads with synthetic tokens, verify that canary percentage routing executes without error, and verify the SHA-256 Merkle audit trail.", srv, tool)
                        }
                    })
                ]
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
                "messages": JsonValue::Array(messages)
            }),
        )
    }

    fn call_discover_server(&self, request: &RPCRequest, args: &JsonValue) -> RPCResponse {
        let query = args
            .get("query")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_lowercase();
        let limit = args.get("limit").and_then(|x| x.as_u64()).unwrap_or(5) as usize;

        let servers = self.cache.list_servers();
        let tokens = tokenize_words(&query);

        let mut scored: Vec<(f64, JsonValue)> = Vec::new();
        for s in &servers {
            let tool_names_and_descs: String = s
                .tools
                .values()
                .map(|t| format!("{} {}", t.name, t.description))
                .collect::<Vec<_>>()
                .join(" ");
            let doc_text = format!(
                "{} {} {} {}",
                s.name.to_lowercase(),
                s.display_name.to_lowercase(),
                s.description.to_lowercase(),
                tool_names_and_descs.to_lowercase()
            );

            let mut score = 0.0;
            for token in &tokens {
                if s.name.to_lowercase().contains(token) {
                    score += 5.0;
                }
                if s.display_name.to_lowercase().contains(token) {
                    score += 3.0;
                }
                if doc_text.contains(token) {
                    score += 1.0;
                }
            }

            if score > 0.0 || tokens.is_empty() {
                scored.push((
                    score,
                    json!({
                        "server_name": s.name.clone(),
                        "display_name": s.display_name.clone(),
                        "description": s.description.clone(),
                        "owning_team": s.owning_team.clone(),
                        "protocol": s.protocol.as_str(),
                        "version": s.version as i64,
                        "enabled_tools_count": s.enabled_tools_count() as i64,
                        "relevance_score": score
                    }),
                ));
            }
        }

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let results: Vec<JsonValue> = scored.into_iter().take(limit).map(|x| x.1).collect();

        let payload = json!({
            "servers": JsonValue::Array(results),
            "total_matches": servers.len() as i64
        });

        RPCResponse::ok(
            request.id.clone(),
            json!({
                "content": [
                    {
                        "type": "text",
                        "text": payload.to_string_pretty()
                    }
                ]
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
                ]
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
                ]
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

        if let Some(proj) = args.get("_projection").cloned() {
            tool_args.insert("_projection", proj);
        }

        let call_req = RPCRequest::new(
            request.id.clone(),
            "tools/call",
            json!({
                "name": tool_name,
                "arguments": tool_args,
            }),
        );
        self.gateway
            .handle_request(server_name, &call_req, caller_actor, user_token)
    }

    fn call_batch_invoke_tools(
        &self,
        request: &RPCRequest,
        args: &JsonValue,
        caller_actor: &str,
        user_token: Option<&str>,
    ) -> RPCResponse {
        let calls = match args.get("calls").and_then(|c| c.as_array()) {
            Some(arr) => arr,
            None => {
                return RPCResponse::err(
                    request.id.clone(),
                    RPCError::new(INVALID_PARAMS, "Missing 'calls' array in batch_invoke_tools"),
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
                ]
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

        let tokens = tokenize_words(&goal.to_lowercase());
        let servers = self.cache.list_servers();

        let mut candidates: Vec<(f64, String, String, String, bool, Vec<String>)> = Vec::new();

        for s in &servers {
            for tool in s.tools.values() {
                if tool.status != ToolStatus::Enabled {
                    continue;
                }
                let mut score = 0.0;
                let t_name_lower = tool.name.to_lowercase();
                let t_desc_lower = tool.description.to_lowercase();

                for tok in &tokens {
                    if t_name_lower.contains(tok) {
                        score += 5.0;
                    }
                    if t_desc_lower.contains(tok) {
                        score += 2.0;
                    }
                    if s.name.to_lowercase().contains(tok) {
                        score += 3.0;
                    }
                    for tag in &tool.tags {
                        if tag.to_lowercase().contains(tok) {
                            score += 4.0;
                        }
                    }
                }

                if score > 0.0 {
                    candidates.push((
                        score,
                        s.name.clone(),
                        tool.name.clone(),
                        tool.description.clone(),
                        tool.is_mutation,
                        tool.tags.clone(),
                    ));
                }
            }
        }

        candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        let mut pipeline = Vec::new();
        for (idx, (_, s_name, t_name, desc, is_mut, tags)) in
            candidates.into_iter().take(limit).enumerate()
        {
            pipeline.push(json!({
                "step": (idx + 1) as i64,
                "server_name": s_name,
                "tool_name": t_name,
                "description": desc,
                "is_mutation": is_mut,
                "tags": tags.iter().map(|s| JsonValue::from(s.as_str())).collect::<Vec<_>>()
            }));
        }

        let payload = json!({
            "goal": goal,
            "recommended_pipeline": JsonValue::Array(pipeline),
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
                ]
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

        let server_opt = self.cache.get_server(server_name);
        let cb_open = self.gateway.circuit_breaker.allow_request(server_name).is_err();

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

        let (root_cause, suggested_action) = if cb_open {
            (
                "Circuit breaker tripped open due to consecutive downstream microservice failures."
                    .to_string(),
                "Wait for the circuit breaker cooldown reset window (typically 30s) or invoke the configured fallback tool."
                    .to_string(),
            )
        } else if tool_status == "DISABLED" {
            (
                "Tool is discovered but disabled in the catalog ('discovery does not imply exposure')."
                    .to_string(),
                "Request tool enablement from the service owner via the control plane API `/api/tools/toggle_status`."
                    .to_string(),
            )
        } else if tool_status == "NOT_FOUND" || tool_status == "SERVER_NOT_FOUND" {
            (
                format!("Target '{}' is not registered in the active catalog.", tool_name),
                "Execute 'discover_server' and 'discover_tools' to locate valid active tool identifiers."
                    .to_string(),
            )
        } else if error_message.to_lowercase().contains("unauthorized") {
            (
                "ABAC security charter rejection for the caller role.".to_string(),
                "Verify x-caller-actor header or ensure proper OAuth token is provided in the Authorization header."
                    .to_string(),
            )
        } else {
            (
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
                ]
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

        let output = self.gateway.execute_codemode(
            code,
            Some(limits),
            require_mutation_approval,
            caller_actor,
            user_token,
            None,
        );

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
                "isError": is_err
            }),
        )
    }

    fn call_get_codemode_types(&self, request: &RPCRequest, args: &JsonValue) -> RPCResponse {
        if let Some(intent) = args.get("intent").and_then(|x| x.as_str()) {
            let top_k = args.get("top_k").and_then(|x| x.as_u64()).unwrap_or(4) as usize;
            let threshold = args
                .get("confidence_threshold")
                .and_then(|x| x.as_f64())
                .unwrap_or(0.82);
            let spec =
                self.gateway
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
                    ]
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
                ]
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

        let output =
            self.gateway
                .resume_codemode(execution_id, approver, None, user_token, None);
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
        let report =
            self.gateway
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
                "isError": false
            }),
        )
    }
}

fn tokenize_words(query: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    for ch in query.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else if !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}
