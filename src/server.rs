//! Production-grade Multi-Threaded HTTP/1.1 & JSON-RPC 2.0 Gateway Server for Omni MCP Platform.
//! Includes Health Checks (`/healthz`, `/readyz`), Prometheus Metrics (`/metrics`), Sharded Rate Limiting,
//! Circuit Breaking, Semantic Caching, WAL Persistence, and Static UI Delivery.

use crate::config::{get_config, OmniConfig};
use crate::control_plane::{
    AutoCrawler, MCPFxHeartbeatMonitor, MCPRegistry, ThirdPartyAuthExchange,
};
use crate::data_plane::{
    CircuitBreakerRegistry, InMemoryConfigCache, MCPProxyGateway, ProtocolTranslator, RateLimiter,
    SandboxLimits, SemanticResponseCache,
};
use crate::extension::OmniMCPServer;
use crate::json::JsonValue;
use crate::logging::now_epoch_secs;
use crate::models::{
    ProtocolType, RPCBatchOrSingle, ServerRecord, ToolRecord, ToolStatus, TraceSpan, PARSE_ERROR,
    RATE_LIMITED,
};
use crate::storage::{MerkleAuditStore, WalRegistryStore};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, mpsc};
use std::thread;
use std::time::Duration;

pub struct GatewayMetrics {
    pub requests_total: AtomicU64,
    pub errors_total: AtomicU64,
    pub rate_limit_hits_total: AtomicU64,
    pub start_time: f64,
}

impl GatewayMetrics {
    pub fn new() -> Self {
        Self {
            requests_total: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
            rate_limit_hits_total: AtomicU64::new(0),
            start_time: now_epoch_secs(),
        }
    }
}

impl Default for GatewayMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct ProductionGatewayContext {
    pub config: OmniConfig,
    pub db_path: String,
    pub store: Arc<WalRegistryStore>,
    pub registry: MCPRegistry,
    pub rate_limiter: RateLimiter,
    pub native_monitor: MCPFxHeartbeatMonitor,
    pub auth_exchange: ThirdPartyAuthExchange,
    pub crawler: AutoCrawler,
    pub cache: InMemoryConfigCache,
    pub gateway: MCPProxyGateway,
    pub omni_mcp: OmniMCPServer,
    pub metrics: Arc<GatewayMetrics>,
    pub audit_store: Arc<MerkleAuditStore>,
    pub traces: Arc<RwLock<Vec<TraceSpan>>>,
}

impl ProductionGatewayContext {
    pub fn new(config: Option<OmniConfig>) -> Self {
        let config = config.unwrap_or_else(get_config);
        let _ = fs::create_dir_all(&config.data_dir);
        let mut db_path_buf = PathBuf::from(&config.data_dir);
        db_path_buf.push("omni_registry.db");
        let db_path = db_path_buf.to_string_lossy().into_owned();

        let store = Arc::new(WalRegistryStore::new(&db_path));
        let registry = MCPRegistry::new(Some(store.clone()));
        let rate_limiter = RateLimiter::new(config.default_rate_limit_rpm);
        let native_monitor = MCPFxHeartbeatMonitor::new();
        let auth_exchange = ThirdPartyAuthExchange::new();
        let crawler = AutoCrawler::new(registry.clone(), Some(native_monitor.clone()), None);
        let cache = InMemoryConfigCache::new(registry.clone(), config.cache_refresh_interval_sec);
        let translator = ProtocolTranslator::new(None, Some(auth_exchange.clone()));
        let cb = CircuitBreakerRegistry::new(
            config.circuit_breaker_threshold,
            config.circuit_breaker_reset_sec,
        );
        let sem_cache = SemanticResponseCache::new(config.semantic_cache_ttl_sec);

        let mut audit_path_buf = PathBuf::from(&config.data_dir);
        audit_path_buf.push("omni_audit.log");
        let audit_store = Arc::new(MerkleAuditStore::new(Some(
            audit_path_buf.to_string_lossy().into_owned(),
        )));
        let traces = Arc::new(RwLock::new(Vec::new()));

        let gateway = MCPProxyGateway::with_resilience(cache.clone(), translator, cb, sem_cache)
            .with_observability(audit_store.clone(), traces.clone());
        let omni_mcp = OmniMCPServer::new(cache.clone(), gateway.clone());

        let ctx = Self {
            config,
            db_path,
            store,
            registry,
            rate_limiter,
            native_monitor,
            auth_exchange,
            crawler,
            cache,
            gateway,
            omni_mcp,
            metrics: Arc::new(GatewayMetrics::new()),
            audit_store,
            traces,
        };

        if ctx.registry.list_servers(None).is_empty()
            || ctx.registry.get_server("uber_rides").is_none()
        {
            ctx.seed_sample_catalog();
        }
        ctx
    }

    pub fn seed_sample_catalog(&self) {
        // 1. Driver Location & Dispatch Service (gRPC)
        let driver_proto = r#"
        syntax = "proto3";
        package uber.driver;
        message DriverLocationRequest {
            string driver_id = 1;
            bool include_telemetry = 2;
        }
        message DriverLocationResponse {
            string driver_id = 1;
            double latitude = 2;
            double longitude = 3;
            string status = 4;
        }
        service DriverDispatchService {
            // Retrieve real-time driver coordinates and dispatch status
            rpc GetDriverLocation (DriverLocationRequest) returns (DriverLocationResponse);
        }
        "#;
        self.crawler
            .register_idl_source("driver-dispatch", driver_proto);

        // 2. Trip Billing Service (Thrift / TChannel)
        let trip_thrift = r#"
        namespace py uber.trip
        service TripBillingService {
            // Query fare estimates and receipt line items for rider trip
            TripFare getTripFare(1: string trip_id, 2: bool include_discounts)
        }
        "#;
        self.crawler.register_idl_source("trip-billing", trip_thrift);

        // 3. Third-party integrations (Jira)
        let jira_srv = self
            .auth_exchange
            .create_third_party_server("jira", "eng-productivity");
        self.registry.register_server(jira_srv);

        // 4. OpenAPI 3.1 Payments & Billing Service (REST with Canary & Fallback)
        let payments_openapi = r#"
        {
          "openapi": "3.1.0",
          "info": {
            "title": "Payments & Billing API",
            "version": "3.0.0",
            "description": "Enterprise payment processing, charge authorisations, and instant refunds."
          },
          "paths": {
            "/payments/charge": {
              "post": {
                "summary": "Process customer payment charge",
                "tags": ["payments", "billing", "checkout"],
                "parameters": [
                  {"name": "customer_id", "in": "query", "required": true, "schema": {"type": "string"}},
                  {"name": "amount_cents", "in": "query", "required": true, "schema": {"type": "integer"}},
                  {"name": "currency", "in": "query", "required": false, "schema": {"type": "string"}}
                ]
              }
            },
            "/payments/refund": {
              "post": {
                "summary": "Execute transaction refund",
                "tags": ["payments", "refund", "billing"],
                "parameters": [
                  {"name": "charge_id", "in": "query", "required": true, "schema": {"type": "string"}},
                  {"name": "reason", "in": "query", "required": false, "schema": {"type": "string"}}
                ]
              }
            }
          }
        }
        "#;
        self.crawler
            .register_idl_source("payments-service", payments_openapi);

        // Run discovery cycle
        self.crawler.run_discovery_workflow();

        // Enable DriverDispatchService for out-of-the-box readiness
        self.registry
            .toggle_server_status("driverdispatchservice", ToolStatus::Enabled);
        self.registry.toggle_tool_status(
            "driverdispatchservice",
            "GetDriverLocation",
            ToolStatus::Enabled,
        );

        // Enable PaymentsService tools
        self.registry
            .toggle_server_status("paymentsservice", ToolStatus::Enabled);
        self.registry.toggle_tool_status(
            "paymentsservice",
            "payments_charge",
            ToolStatus::Enabled,
        );
        self.registry.toggle_tool_status(
            "paymentsservice",
            "payments_refund",
            ToolStatus::Enabled,
        );

        // 5. Seed Code Mode multi-step services (`uber_rides` & `accounting`)
        let mut rides_srv = ServerRecord::new(
            "uber_rides",
            "Uber Rides Service",
            "Rider trip history, status tracking, and ride lookup.",
            "rider-experience",
        );
        rides_srv.protocol = ProtocolType::Grpc;
        rides_srv.status = ToolStatus::Enabled;
        let mut list_recent = ToolRecord::new(
            "list_recent",
            "List recent rides for a rider account",
            json!({
                "type": "object",
                "properties": {
                    "user_id": {"type": "string", "description": "Rider user identifier"}
                },
                "required": ["user_id"]
            }),
        );
        list_recent.protocol = ProtocolType::Grpc;
        list_recent.downstream_endpoint = "muttley://uber_rides/list_recent".to_string();
        list_recent.status = ToolStatus::Enabled;
        list_recent.tags = vec!["rides".to_string(), "trip".to_string(), "history".to_string()];
        rides_srv.tools.insert("list_recent".to_string(), list_recent);
        self.registry.register_server(rides_srv);

        let mut acct_srv = ServerRecord::new(
            "accounting",
            "Accounting & Refunds Service",
            "Automated rider billing adjustments and refund execution.",
            "fintech-accounting",
        );
        acct_srv.protocol = ProtocolType::Http;
        acct_srv.status = ToolStatus::Enabled;
        let mut refund_tool = ToolRecord::new(
            "refund",
            "Issue a refund for a cancelled or disputed ride",
            json!({
                "type": "object",
                "properties": {
                    "ride_id": {"type": "string", "description": "Ride identifier to refund"},
                    "reason": {"type": "string", "description": "Audit reason for refund"}
                },
                "required": ["ride_id", "reason"]
            }),
        );
        refund_tool.protocol = ProtocolType::Http;
        refund_tool.downstream_endpoint = "muttley://accounting/refund".to_string();
        refund_tool.status = ToolStatus::Enabled;
        refund_tool.is_mutation = true;
        refund_tool.tags = vec!["refund".to_string(), "accounting".to_string(), "billing".to_string()];
        acct_srv.tools.insert("refund".to_string(), refund_tool);
        self.registry.register_server(acct_srv);

        self.cache.refresh_now();
    }

    /// Dispatches an HTTP request and returns `(status_code, content_type, extra_headers, body_bytes)`.
    pub fn handle_http(
        &self,
        method: &str,
        raw_path: &str,
        headers: &HashMap<String, String>,
        body_bytes: &[u8],
    ) -> (u16, String, Vec<(String, String)>, Vec<u8>) {
        self.metrics.requests_total.fetch_add(1, Ordering::Relaxed);
        let path = raw_path.split('?').next().unwrap_or(raw_path);

        if method.eq_ignore_ascii_case("OPTIONS") {
            return (200, "text/plain".to_string(), Vec::new(), Vec::new());
        }

        if method.eq_ignore_ascii_case("GET") {
            return self.handle_get(path);
        }

        if method.eq_ignore_ascii_case("POST") {
            return self.handle_post(path, headers, body_bytes);
        }

        self.json_response(405, json!({"error": "Method not allowed"}))
    }

    fn handle_get(&self, path: &str) -> (u16, String, Vec<(String, String)>, Vec<u8>) {
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
                let enabled_tools: usize =
                    servers.iter().map(|s| s.enabled_tools_count()).sum();
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
                        "version": "2.0.0",
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
            _ if path.starts_with("/api/servers/") => {
                let server_name = path.trim_start_matches("/api/servers/").trim();
                let server = match self.registry.get_server(server_name) {
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
            "/sse" | "/omni/sse" => {
                let sse_body = "event: endpoint\ndata: /omni/mcp\n\nevent: status\ndata: {\"status\":\"connected\",\"version\":\"3.0.0\",\"runtime\":\"rust\"}\n\n";
                (
                    200,
                    "text/event-stream; charset=utf-8".to_string(),
                    vec![
                        ("Cache-Control".to_string(), "no-cache".to_string()),
                        ("Connection".to_string(), "keep-alive".to_string()),
                        ("Access-Control-Allow-Origin".to_string(), "*".to_string()),
                    ],
                    sse_body.as_bytes().to_vec(),
                )
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
                let (verified, count_or_idx) = self.audit_store.verify_chain();
                if verified {
                    self.json_response(
                        200,
                        json!({
                            "verified": true,
                            "records_checked": count_or_idx as i64,
                            "algorithm": "SHA-256 (FIPS 180-4)",
                            "head_hash": self.audit_store.get_head_hash()
                        }),
                    )
                } else {
                    self.json_response(
                        409,
                        json!({
                            "verified": false,
                            "corrupted_at_index": count_or_idx as i64,
                            "reason": "Hash mismatch in cryptographic Merkle chain"
                        }),
                    )
                }
            }
            "/api/codemode/types" => {
                let gen = self.gateway.generate_codemode_types(None, None);
                self.json_response(200, gen.to_json())
            }
            _ => self.serve_static_or_index(path),
        }
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
        let actor = headers
            .get("x-caller-actor")
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("agent");

        let rate_key = format!("{}:{}", token, actor);
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

        let traceparent = headers.get("traceparent").map(|s| s.as_str());

        // 1. Dynamic Virtual Server MCP Route: `/<service-name>/mcp`
        if path.ends_with("/mcp") && path != "/omni/mcp" {
            let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
            if parts.len() == 2 && parts[1] == "mcp" {
                let service_name = parts[0];
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
                match RPCBatchOrSingle::from_json(&json_val) {
                    Ok(batch_or_single) => {
                        let resp_json = self.gateway.handle_batch_or_single_with_trace(
                            service_name,
                            batch_or_single,
                            actor,
                            Some(token),
                            traceparent,
                        );
                        return self.json_response(200, resp_json);
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
            match RPCBatchOrSingle::from_json(&json_val) {
                Ok(batch_or_single) => {
                    let resp_json = self.omni_mcp.handle_batch_or_single(
                        batch_or_single,
                        actor,
                        Some(token),
                    );
                    return self.json_response(200, resp_json);
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

        let body = parsed_body.unwrap_or_else(|_| JsonValue::object());

        // 3. Control Plane Management APIs
        match path {
            "/api/crawler/trigger" => {
                let res = self.crawler.run_discovery_workflow();
                self.cache.refresh_now();
                self.json_response(200, res)
            }
            "/api/tools/toggle_status" => {
                let s_name = body.get("serverName").and_then(|x| x.as_str()).unwrap_or("");
                let t_name = body.get("toolName").and_then(|x| x.as_str()).unwrap_or("");
                let status_str = body
                    .get("status")
                    .and_then(|x| x.as_str())
                    .unwrap_or("ENABLED");
                let new_status = ToolStatus::from_str(status_str);
                let ok = self.registry.toggle_tool_status(s_name, t_name, new_status);
                self.cache.refresh_now();
                self.gateway.semantic_cache.invalidate_service(s_name);
                self.json_response(200, json!({"success": ok}))
            }
            "/api/servers/toggle_status" => {
                let s_name = body.get("serverName").and_then(|x| x.as_str()).unwrap_or("");
                let status_str = body
                    .get("status")
                    .and_then(|x| x.as_str())
                    .unwrap_or("ENABLED");
                let new_status = ToolStatus::from_str(status_str);
                let ok = self.registry.toggle_server_status(s_name, new_status);
                self.cache.refresh_now();
                self.json_response(200, json!({"success": ok}))
            }
            "/api/tools/propose_update" => {
                let s_name = body.get("serverName").and_then(|x| x.as_str()).unwrap_or("");
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
                let ok = self.registry.approve_diff(diff_id, approver);
                self.cache.refresh_now();
                self.json_response(200, json!({"success": ok}))
            }
            "/api/tools/rollback" => {
                let s_name = body.get("serverName").and_then(|x| x.as_str()).unwrap_or("");
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
                let srv = body.get("server_name").and_then(|x| x.as_str()).unwrap_or("");
                let tool = body.get("tool_name").and_then(|x| x.as_str()).unwrap_or("");
                let args = body.get("arguments").cloned().unwrap_or_else(JsonValue::object);
                let req = crate::models::RPCRequest::new(1, "tools/call", json!({"name": tool, "arguments": args}));
                let resp = self.gateway.handle_request_with_trace(srv, &req, actor, Some(token), traceparent);
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
                self.json_response(200, out.to_json())
            }
            _ => self.json_response(404, json!({"error": "Endpoint not found"})),
        }
    }

    fn json_response(
        &self,
        status: u16,
        data: JsonValue,
    ) -> (u16, String, Vec<(String, String)>, Vec<u8>) {
        (
            status,
            "application/json".to_string(),
            Vec::new(),
            data.to_string_compact().into_bytes(),
        )
    }

    fn serve_static_or_index(&self, path: &str) -> (u16, String, Vec<(String, String)>, Vec<u8>) {
        let ui_dist = Path::new("ui").join("dist");
        if ui_dist.exists() {
            let rel = path.trim_start_matches('/');
            if !rel.is_empty() && !rel.contains("..") {
                let candidate = ui_dist.join(rel);
                if candidate.is_file() {
                    if let Ok(bytes) = fs::read(&candidate) {
                        let ctype = if rel.ends_with(".js") {
                            "application/javascript"
                        } else if rel.ends_with(".css") {
                            "text/css"
                        } else if rel.ends_with(".svg") {
                            "image/svg+xml"
                        } else if rel.ends_with(".png") {
                            "image/png"
                        } else if rel.ends_with(".html") {
                            "text/html; charset=utf-8"
                        } else {
                            "application/octet-stream"
                        };
                        return (200, ctype.to_string(), Vec::new(), bytes);
                    }
                }
            }
            let index_path = ui_dist.join("index.html");
            if index_path.is_file() {
                if let Ok(bytes) = fs::read(&index_path) {
                    return (
                        200,
                        "text/html; charset=utf-8".to_string(),
                        Vec::new(),
                        bytes,
                    );
                }
            }
        }
        (
            200,
            "text/html; charset=utf-8".to_string(),
            Vec::new(),
            EMBEDDED_DASHBOARD_HTML.as_bytes().to_vec(),
        )
    }
}

static GLOBAL_CONTEXT: OnceLock<ProductionGatewayContext> = OnceLock::new();

pub fn get_gateway_context() -> &'static ProductionGatewayContext {
    GLOBAL_CONTEXT.get_or_init(|| ProductionGatewayContext::new(None))
}

pub fn run_server(host: &str, port: u16) -> Result<(), String> {
    let ctx = get_gateway_context().clone();
    ctx.cache.start_background_refresh();

    let bind_addr = format!("{}:{}", host, port);
    let listener = TcpListener::bind(&bind_addr)
        .map_err(|e| format!("Failed to bind HTTP server on {}: {}", bind_addr, e))?;

    println!("[*] Omni MCP Gateway (Rust 2.0 Engine) running at http://{}:{}", host, port);
    println!("[*] Liveness probe: http://{}:{}/healthz", host, port);
    println!("[*] Prometheus metrics: http://{}:{}/metrics", host, port);
    println!("[*] Omni MCP proxy available at http://{}:{}/omni/mcp", host, port);
    println!("[*] Dynamic virtual servers available at http://{}:{}/<service>/mcp", host, port);

    // Bounded worker pool for zero-spawn-overhead request handling
    let num_workers = ctx.config.worker_threads.max(4);
    let (tx, rx) = mpsc::channel::<TcpStream>();
    let shared_rx = Arc::new(Mutex::new(rx));

    for _ in 0..num_workers {
        let worker_rx = shared_rx.clone();
        let worker_ctx = ctx.clone();
        thread::spawn(move || loop {
            let stream = {
                let lock = worker_rx.lock().unwrap();
                match lock.recv() {
                    Ok(s) => s,
                    Err(_) => break,
                }
            };
            handle_connection(stream, &worker_ctx);
        });
    }

    for incoming in listener.incoming() {
        if let Ok(stream) = incoming {
            let _ = stream.set_nodelay(true);
            let _ = tx.send(stream);
        }
    }

    Ok(())
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
                        RPCBatchOrSingle::Single(req) => {
                            gateway.handle_request(srv, &req, "agent", Some("stdio-token")).to_json()
                        }
                        RPCBatchOrSingle::Batch(reqs) => {
                            let results: Vec<JsonValue> = reqs
                                .iter()
                                .map(|r| gateway.handle_request(srv, r, "agent", Some("stdio-token")).to_json())
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

fn handle_connection(mut stream: TcpStream, ctx: &ProductionGatewayContext) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

    loop {
        let mut reader = match stream.try_clone() {
            Ok(s) => BufReader::new(s),
            Err(_) => return,
        };

        let mut request_line = String::new();
        match reader.read_line(&mut request_line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }

        let parts: Vec<&str> = request_line.trim().split_whitespace().collect();
        if parts.len() < 2 {
            return;
        }
        let method = parts[0].to_string();
        let path = parts[1].to_string();

        let mut headers = HashMap::new();
        let mut content_length = 0usize;
        let mut keep_alive = true;

        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = trimmed.split_once(':') {
                        let key_lc = k.trim().to_lowercase();
                        let val_str = v.trim().to_string();
                        if key_lc == "content-length" {
                            content_length = val_str.parse().unwrap_or(0);
                        } else if key_lc == "connection" && val_str.eq_ignore_ascii_case("close") {
                            keep_alive = false;
                        }
                        headers.insert(key_lc, val_str);
                    }
                }
            }
        }

        let mut body_bytes = vec![0u8; content_length.min(ctx.config.max_body_size_bytes + 1)];
        if content_length > 0 {
            if reader.read_exact(&mut body_bytes).is_err() {
                return;
            }
        }

        let (status, content_type, extra_headers, resp_body) =
            ctx.handle_http(&method, &path, &headers, &body_bytes);

        let status_text = match status {
            200 => "OK",
            400 => "Bad Request",
            404 => "Not Found",
            405 => "Method Not Allowed",
            413 => "Payload Too Large",
            429 => "Too Many Requests",
            _ => "Internal Server Error",
        };

        let conn_hdr = if keep_alive { "keep-alive" } else { "close" };
        let mut header_str = format!(
            "HTTP/1.1 {} {}\r\n\
             Content-Type: {}\r\n\
             Content-Length: {}\r\n\
             Access-Control-Allow-Origin: {}\r\n\
             Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
             Access-Control-Allow-Headers: Content-Type, Authorization, X-Caller-Actor\r\n\
             Connection: {}\r\n",
            status,
            status_text,
            content_type,
            resp_body.len(),
            ctx.config.cors_allowed_origins,
            conn_hdr
        );
        for (hk, hv) in extra_headers {
            header_str.push_str(&format!("{}: {}\r\n", hk, hv));
        }
        header_str.push_str("\r\n");

        if stream.write_all(header_str.as_bytes()).is_err() {
            return;
        }
        if stream.write_all(&resp_body).is_err() {
            return;
        }
        let _ = stream.flush();

        if !keep_alive {
            return;
        }
    }
}

const EMBEDDED_DASHBOARD_HTML: &str = r#"<!DOCTYPE html>
<html>
<head>
    <title>Omni MCP - Rust Enterprise Platform</title>
    <style>
        body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; background: #0b0b0f; color: #f3f4f6; margin: 0; padding: 24px; }
        header { border-bottom: 1px solid #222; padding-bottom: 16px; margin-bottom: 24px; display: flex; justify-content: space-between; align-items: center; }
        h1 { margin: 0; font-size: 22px; font-weight: 700; }
        .badge { background: #1b382b; color: #00e575; border-radius: 4px; padding: 4px 10px; font-size: 13px; font-weight: 600; }
        .card { background: #13141e; border: 1px solid #212333; border-radius: 8px; padding: 18px; margin-bottom: 16px; }
        .grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(320px, 1fr)); gap: 16px; }
        code { color: #00e575; }
    </style>
</head>
<body>
    <header>
        <div>
            <h1>Omni MCP Platform (Rust 2.0 Engine)</h1>
            <p style="color: #888; margin: 4px 0 0 0;">High-Speed Enterprise MCP Gateway & Runtime Registry</p>
        </div>
        <div class="badge">● Rust Runtime Online</div>
    </header>
    <div class="card">
        <h3>Platform Overview</h3>
        <p>Production Rust server active. Endpoints available:</p>
        <ul>
            <li>Dynamic Virtual MCP Servers: <code>/&lt;service-name&gt;/mcp</code></li>
            <li>Omni MCP Incremental Discovery Proxy: <code>/omni/mcp</code></li>
            <li>Health &amp; Readiness: <code>/healthz</code>, <code>/readyz</code>, <code>/metrics</code></li>
            <li>Control Plane APIs: <code>/api/servers</code>, <code>/api/stats</code>, <code>/api/diffs</code></li>
        </ul>
    </div>
</body>
</html>"#;
