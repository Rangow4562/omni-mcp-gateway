//! Production-grade Multi-Threaded HTTP/1.1 & JSON-RPC 2.0 Gateway Server for Omni MCP Platform.
//! Includes Health Checks (`/healthz`, `/readyz`), Prometheus Metrics (`/metrics`), Sharded Rate Limiting,
//! Circuit Breaking, Semantic Caching, WAL Persistence, and Static UI Delivery.

use crate::config::{get_config, OmniConfig};
use crate::control_plane::{
    AutoCrawler, MCPFxHeartbeatMonitor, MCPRegistry, ThirdPartyAuthExchange,
};
use crate::data_plane::{
    CircuitBreakerRegistry, FaultInjectionConfig, InMemoryConfigCache, MCPProxyGateway,
    ProtocolTranslator, RateLimiter, SandboxLimits, SemanticResponseCache,
};
use crate::extension::OmniMCPServer;
use crate::json::JsonValue;
use crate::logging::now_epoch_secs;
use crate::models::{
    MutationCompensationSpec, ProtocolType, RPCBatchOrSingle, ServerRecord, ToolRecord, ToolStatus,
    TraceSpan, PARSE_ERROR, RATE_LIMITED,
};
use crate::storage::{MerkleAuditStore, WalRegistryStore};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock, RwLock};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AuthRole {
    Agent = 1,
    Operator = 2,
    Approver = 3,
    Admin = 4,
}

impl AuthRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthRole::Agent => "agent",
            AuthRole::Operator => "operator",
            AuthRole::Approver => "approver",
            AuthRole::Admin => "admin",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "agent" => Some(AuthRole::Agent),
            "operator" => Some(AuthRole::Operator),
            "approver" => Some(AuthRole::Approver),
            "admin" => Some(AuthRole::Admin),
            _ => None,
        }
    }

    pub fn can_satisfy(&self, required: AuthRole) -> bool {
        matches!(
            (self, required),
            (AuthRole::Admin, _)
                | (AuthRole::Approver, AuthRole::Approver)
                | (AuthRole::Approver, AuthRole::Agent)
                | (AuthRole::Operator, AuthRole::Operator)
                | (AuthRole::Operator, AuthRole::Agent)
                | (AuthRole::Agent, AuthRole::Agent)
        )
    }
}

#[derive(Debug, Clone)]
pub struct AuthPrincipal {
    pub id: String,
    pub role: AuthRole,
}

fn is_local_origin(origin: &str) -> bool {
    let rest = if let Some(r) = origin.strip_prefix("http://") {
        r
    } else if let Some(r) = origin.strip_prefix("https://") {
        r
    } else {
        return false;
    };
    let host_part = rest.split('/').next().unwrap_or(rest);
    let host_part = host_part.split('?').next().unwrap_or(host_part);
    let host_part = host_part.split('#').next().unwrap_or(host_part);
    let hostname = if host_part.starts_with('[') {
        if let Some(end_bracket) = host_part.find(']') {
            &host_part[..=end_bracket]
        } else {
            return false;
        }
    } else {
        host_part.split(':').next().unwrap_or(host_part)
    };
    hostname.eq_ignore_ascii_case("localhost") || hostname == "127.0.0.1" || hostname == "[::1]"
}

fn is_origin_allowed(origin: &str, cors_config: &str, host: Option<&str>) -> bool {
    if origin == "null" || origin.trim().is_empty() {
        return false;
    }

    let is_local_orig = is_local_origin(origin);

    if let Some(h) = host {
        let h_lower = h.to_lowercase();
        let h_name = h_lower.split(':').next().unwrap_or(&h_lower);
        let host_is_local = h_name == "localhost" || h_name == "127.0.0.1" || h_name == "[::1]";

        if host_is_local {
            if is_local_orig {
                return true;
            }
            if cors_config != "*" && !cors_config.trim().is_empty() {
                let allowed: Vec<&str> = cors_config.split(',').map(|s| s.trim()).collect();
                if allowed.contains(&origin) {
                    return true;
                }
            }
            return false;
        }
    }

    if cors_config != "*" && !cors_config.trim().is_empty() {
        let allowed: Vec<&str> = cors_config.split(',').map(|s| s.trim()).collect();
        if allowed.contains(&origin) {
            return true;
        }
    }

    if is_local_orig {
        return true;
    }

    cors_config == "*"
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
            || ctx.registry.get_server("rides_service").is_none()
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
        package fleet.driver;
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
        namespace py fleet.trip
        service TripBillingService {
            // Query fare estimates and receipt line items for rider trip
            TripFare getTripFare(1: string trip_id, 2: bool include_discounts)
        }
        "#;
        self.crawler
            .register_idl_source("trip-billing", trip_thrift);

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
            "title": "PaymentsService",
            "version": "3.0.0",
            "description": "Enterprise payment processing, charge authorisations, and instant refunds."
          },
          "paths": {
            "/payments/charge": {
              "post": {
                "operationId": "payments_charge",
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
                "operationId": "payments_refund",
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

        // Enable PaymentsService tools and attach Declarative Saga Compensation to `payments_charge`
        self.registry
            .toggle_server_status("paymentsservice", ToolStatus::Enabled);
        self.registry
            .toggle_tool_status("paymentsservice", "payments_charge", ToolStatus::Enabled);
        self.registry
            .toggle_tool_status("paymentsservice", "payments_refund", ToolStatus::Enabled);
        if let Some(mut pay_srv) = self.registry.get_server("paymentsservice") {
            if let Some(charge_tool) = pay_srv.tools.get_mut("payments_charge") {
                charge_tool.is_mutation = true;
                let mut arg_mapping = std::collections::BTreeMap::new();
                arg_mapping.insert("charge_id".to_string(), "$output.charge_id".to_string());
                arg_mapping.insert("reason".to_string(), "saga_rollback".to_string());
                charge_tool.compensation_spec = Some(MutationCompensationSpec {
                    forward_tool: "payments_charge".to_string(),
                    compensating_server: Some("paymentsservice".to_string()),
                    compensating_tool: "payments_refund".to_string(),
                    arg_mapping,
                });
            }
            self.registry.register_server(pay_srv);
        }

        // 5. Seed Code Mode multi-step services (`rides_service` & `accounting`, plus `uber_rides` alias)
        let mut rides_srv = ServerRecord::new(
            "rides_service",
            "Rides Service",
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
        list_recent.downstream_endpoint = "muttley://rides_service/list_recent".to_string();
        list_recent.status = ToolStatus::Enabled;
        list_recent.tags = vec![
            "rides".to_string(),
            "trip".to_string(),
            "history".to_string(),
        ];
        rides_srv
            .tools
            .insert("list_recent".to_string(), list_recent.clone());
        self.registry.register_server(rides_srv);

        // Alias for backwards compatibility with legacy tests
        let mut uber_srv = ServerRecord::new(
            "uber_rides",
            "Rides Service (Legacy Alias)",
            "Rider trip history, status tracking, and ride lookup.",
            "rider-experience",
        );
        uber_srv.protocol = ProtocolType::Grpc;
        uber_srv.status = ToolStatus::Enabled;
        let mut uber_list_recent = list_recent;
        uber_list_recent.downstream_endpoint = "muttley://uber_rides/list_recent".to_string();
        uber_srv
            .tools
            .insert("list_recent".to_string(), uber_list_recent);
        self.registry.register_server(uber_srv);

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
        refund_tool.tags = vec![
            "refund".to_string(),
            "accounting".to_string(),
            "billing".to_string(),
        ];
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

        if let Some(origin) = headers.get("origin") {
            if !is_origin_allowed(
                origin,
                &self.config.cors_allowed_origins,
                headers.get("host").map(|s| s.as_str()),
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

        if path.starts_with("/api/") && !method.eq_ignore_ascii_case("OPTIONS") {
            let required_role = match path {
                "/api/stats"
                | "/api/servers"
                | "/api/codemode/types"
                | "/api/codemode/speculative_types" => AuthRole::Agent,
                _ if path.starts_with("/api/servers/") => AuthRole::Agent,
                "/api/execute" | "/api/codemode/eval" | "/api/codemode/simulate" => AuthRole::Agent,
                "/api/diffs"
                | "/api/traces"
                | "/api/audit"
                | "/api/audit/verify"
                | "/api/tools/propose_update" => AuthRole::Operator,
                "/api/codemode/suspended" | "/api/codemode/resume" | "/api/diffs/approve" => {
                    AuthRole::Approver
                }
                "/api/tools/toggle_status"
                | "/api/servers/toggle_status"
                | "/api/tools/rollback"
                | "/api/crawler/trigger" => AuthRole::Admin,
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
            return self.handle_get(path, headers);
        }

        if method.eq_ignore_ascii_case("POST") {
            return self.handle_post(path, headers, body_bytes);
        }

        self.json_response(405, json!({"error": "Method not allowed"}))
    }

    pub fn authenticate(&self, headers: &HashMap<String, String>) -> Result<AuthPrincipal, u16> {
        let auth_hdr = headers
            .get("authorization")
            .map(|s| s.as_str())
            .unwrap_or("");
        let token = auth_hdr
            .strip_prefix("Bearer ")
            .or_else(|| auth_hdr.strip_prefix("bearer "))
            .map(|s| s.trim())
            .unwrap_or("");

        if token.is_empty() || token == "anonymous" {
            return Err(401);
        }

        // 1. If auth_secret is configured, strictly validate against it using constant-time comparison
        if !self.config.auth_secret.is_empty() {
            let mut matched = None;
            for entry in self.config.auth_secret.split(',') {
                let entry = entry.trim();
                if entry.is_empty() {
                    continue;
                }
                let parts: Vec<&str> = entry.split(':').collect();
                if parts.len() >= 2 {
                    if let Some(role) = AuthRole::from_str(parts[0]) {
                        let secret = parts[1];
                        if crate::models::constant_time_eq(token.as_bytes(), secret.as_bytes()) {
                            let id = if parts.len() >= 3 {
                                parts[2].to_string()
                            } else {
                                format!("{}_user", parts[0])
                            };
                            matched = Some(AuthPrincipal { id, role });
                            break;
                        }
                    } else if crate::models::constant_time_eq(token.as_bytes(), entry.as_bytes()) {
                        matched = Some(AuthPrincipal {
                            id: "admin".to_string(),
                            role: AuthRole::Admin,
                        });
                        break;
                    }
                } else if crate::models::constant_time_eq(token.as_bytes(), entry.as_bytes()) {
                    matched = Some(AuthPrincipal {
                        id: "admin".to_string(),
                        role: AuthRole::Admin,
                    });
                    break;
                }
            }

            if let Some(p) = matched {
                return Ok(p);
            } else {
                return Err(401);
            }
        }

        // 2. Dev/Test mode (auth_secret not configured on loopback)
        let role = if token.contains("admin") {
            AuthRole::Admin
        } else if token.contains("approver") {
            AuthRole::Approver
        } else if token.contains("operator") {
            AuthRole::Operator
        } else {
            AuthRole::Agent
        };

        Ok(AuthPrincipal {
            id: token.to_string(),
            role,
        })
    }

    pub fn is_authorized(&self, headers: &HashMap<String, String>) -> bool {
        self.authenticate(headers).is_ok()
    }

    fn forbidden_response(
        &self,
        endpoint: &str,
        required: AuthRole,
    ) -> (u16, String, Vec<(String, String)>, Vec<u8>) {
        (
            403,
            "application/json".to_string(),
            Vec::new(),
            json!({
                "error": format!("Forbidden: Insufficient role permissions for {}. Required role: {:?}", endpoint, required)
            })
            .to_string_compact()
            .into_bytes(),
        )
    }

    fn unauthorized_response(
        &self,
        endpoint: &str,
    ) -> (u16, String, Vec<(String, String)>, Vec<u8>) {
        (
            401,
            "application/json".to_string(),
            vec![("WWW-Authenticate".to_string(), "Bearer".to_string())],
            json!({
                "error": format!("Unauthorized: Valid Bearer token required for {}", endpoint)
            })
            .to_string_compact()
            .into_bytes(),
        )
    }

    fn handle_get(
        &self,
        path: &str,
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
                        (
                            "Deprecation".to_string(),
                            "@deprecated: use Streamable HTTP on /omni/mcp".to_string(),
                        ),
                        ("Sunset".to_string(), "2026-12-31".to_string()),
                    ],
                    sse_body.as_bytes().to_vec(),
                )
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
                    let service_name = parts[0];
                    if let Some(srv) = self.registry.get_server(service_name) {
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
            "/api/codemode/suspended" => {
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
                    let resp_json =
                        self.omni_mcp
                            .handle_batch_or_single(batch_or_single, actor, Some(token));
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
                let s_name = body
                    .get("serverName")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
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
                let s_name = body
                    .get("serverName")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
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
                let ok = self.registry.approve_diff(diff_id, approver);
                self.cache.refresh_now();
                self.json_response(200, json!({"success": ok}))
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

fn send_raw_error_response<W: Write>(
    writer: &mut W,
    status_code: u16,
    status_text: &str,
    error_msg: &str,
) {
    let body = format!("{{\"error\":\"{}\"}}", error_msg);
    let resp = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status_code,
        status_text,
        body.len(),
        body
    );
    let _ = writer.write_all(resp.as_bytes());
    let _ = writer.flush();
}

fn read_line_capped<R: BufRead>(
    reader: &mut R,
    buf: &mut String,
    max_bytes: usize,
) -> std::io::Result<usize> {
    buf.clear();
    let mut total_read = 0;
    while total_read < max_bytes {
        let available = match reader.fill_buf() {
            Ok(n) => n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            break;
        }
        let (consume_len, found_newline) = match available.iter().position(|&b| b == b'\n') {
            Some(pos) => (pos + 1, true),
            None => {
                let to_take = available.len().min(max_bytes - total_read);
                (to_take, false)
            }
        };
        let s = match std::str::from_utf8(&available[..consume_len]) {
            Ok(valid) => valid,
            Err(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid utf-8",
                ))
            }
        };
        buf.push_str(s);
        reader.consume(consume_len);
        total_read += consume_len;
        if found_newline {
            return Ok(total_read);
        }
        if total_read >= max_bytes {
            return Err(std::io::Error::other("Line length exceeded"));
        }
    }
    Ok(total_read)
}

pub fn run_server(host: &str, port: u16) -> Result<(), String> {
    let ctx = get_gateway_context().clone();
    ctx.config.validate_bind(host)?;
    ctx.cache.start_background_refresh();

    let bind_addr = format!("{}:{}", host, port);
    let listener = TcpListener::bind(&bind_addr)
        .map_err(|e| format!("Failed to bind HTTP server on {}: {}", bind_addr, e))?;

    println!("[*] Omni MCP Gateway running at http://{}:{}", host, port);
    println!("[*] Liveness probe: http://{}:{}/healthz", host, port);
    println!("[*] Prometheus metrics: http://{}:{}/metrics", host, port);
    println!(
        "[*] Omni MCP proxy available at http://{}:{}/omni/mcp",
        host, port
    );
    println!(
        "[*] Dynamic virtual servers available at http://{}:{}/<service>/mcp",
        host, port
    );

    // Bounded worker pool for zero-spawn-overhead request handling
    let num_workers = ctx.config.worker_threads.max(4);
    let queue_bound = (num_workers * 16).max(64);
    let (tx, rx) = mpsc::sync_channel::<TcpStream>(queue_bound);
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
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handle_connection(stream, &worker_ctx);
            }));
        });
    }

    for stream in listener.incoming().flatten() {
        let _ = stream.set_nodelay(true);
        match tx.try_send(stream) {
            Ok(_) => {}
            Err(mpsc::TrySendError::Full(mut saturated_stream)) => {
                send_raw_error_response(
                    &mut saturated_stream,
                    503,
                    "Service Unavailable",
                    "Service Unavailable: Worker accept queue saturated",
                );
            }
            Err(mpsc::TrySendError::Disconnected(_)) => break,
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

pub fn handle_connection(stream: TcpStream, ctx: &ProductionGatewayContext) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    handle_stream(stream, ctx);
}

pub fn handle_stream<S: Read + Write>(stream: S, ctx: &ProductionGatewayContext) {
    let mut reader = BufReader::new(stream);

    loop {
        let mut request_line = String::new();
        match read_line_capped(&mut reader, &mut request_line, 8192) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }

        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() < 2 {
            return;
        }
        let method = parts[0].to_string();
        let path = parts[1].to_string();

        let mut headers = HashMap::new();
        let mut content_length = 0usize;
        let mut keep_alive = true;
        let mut header_lines = 0usize;
        let mut total_header_bytes = 0usize;
        let mut seen_content_length = false;
        let mut seen_transfer_encoding = false;
        let mut smuggling_detected = false;
        let mut malformed_content_length = false;

        loop {
            let mut line = String::new();
            match read_line_capped(&mut reader, &mut line, 8192) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    header_lines += 1;
                    total_header_bytes += line.len();
                    if header_lines > 100 || total_header_bytes > 64 * 1024 {
                        send_raw_error_response(
                            reader.get_mut(),
                            431,
                            "Request Header Fields Too Large",
                            "Request Header Fields Too Large (Cap: 64KB)",
                        );
                        return;
                    }
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = trimmed.split_once(':') {
                        let key_lc = k.trim().to_lowercase();
                        let val_str = v.trim().to_string();
                        if key_lc == "content-length" {
                            if seen_content_length
                                && headers.get("content-length") != Some(&val_str)
                            {
                                smuggling_detected = true;
                            }
                            seen_content_length = true;
                            let val_trimmed = val_str.trim();
                            if val_trimmed.is_empty()
                                || !val_trimmed.chars().all(|c| c.is_ascii_digit())
                            {
                                malformed_content_length = true;
                            } else {
                                match val_trimmed.parse::<usize>() {
                                    Ok(cl) => content_length = cl,
                                    Err(_) => malformed_content_length = true,
                                }
                            }
                        } else if key_lc == "transfer-encoding" {
                            seen_transfer_encoding = true;
                        } else if key_lc == "connection" && val_str.eq_ignore_ascii_case("close") {
                            keep_alive = false;
                        }
                        headers.insert(key_lc, val_str);
                    }
                }
            }
        }

        if malformed_content_length {
            send_raw_error_response(
                reader.get_mut(),
                400,
                "Bad Request",
                "Bad Request: Invalid Content-Length",
            );
            return;
        }

        if smuggling_detected || (seen_transfer_encoding && seen_content_length) {
            send_raw_error_response(
                reader.get_mut(),
                400,
                "Bad Request",
                "Bad Request: Conflicting Content-Length / Transfer-Encoding",
            );
            return;
        }

        // A9: Reject Transfer-Encoding (e.g. chunked) without Content-Length with 501 Not Implemented
        if seen_transfer_encoding && !seen_content_length {
            send_raw_error_response(
                reader.get_mut(),
                501,
                "Not Implemented",
                "Not Implemented: Transfer-Encoding not supported",
            );
            return;
        }

        // A9: Close connection on body size limit violations
        if content_length > ctx.config.max_body_size_bytes {
            send_raw_error_response(
                reader.get_mut(),
                413,
                "Payload Too Large",
                "Payload Too Large (Exceeds Limit)",
            );
            return;
        }

        let mut body_bytes = vec![0u8; content_length];
        if content_length > 0 && reader.read_exact(&mut body_bytes).is_err() {
            return;
        }

        let (status, content_type, extra_headers, resp_body) =
            ctx.handle_http(&method, &path, &headers, &body_bytes);

        let status_text = match status {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            409 => "Conflict",
            413 => "Payload Too Large",
            415 => "Unsupported Media Type",
            429 => "Too Many Requests",
            431 => "Request Header Fields Too Large",
            500 => "Internal Server Error",
            501 => "Not Implemented",
            503 => "Service Unavailable",
            _ => "Internal Server Error",
        };

        let conn_hdr = if keep_alive && status < 400 {
            "keep-alive"
        } else {
            "close"
        };
        let mut header_str = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n",
            status,
            status_text,
            content_type,
            resp_body.len(),
        );

        // Emit CORS headers only when Origin is present and allowed, and never on 403 Forbidden
        if let Some(origin) = headers.get("origin") {
            if status != 403
                && is_origin_allowed(
                    origin,
                    &ctx.config.cors_allowed_origins,
                    headers.get("host").map(|s| s.as_str()),
                )
            {
                if ctx.config.cors_allowed_origins == "*" {
                    header_str.push_str("Access-Control-Allow-Origin: *\r\n");
                } else {
                    header_str.push_str(&format!(
                        "Access-Control-Allow-Origin: {}\r\nVary: Origin\r\n",
                        origin
                    ));
                }
                header_str.push_str("Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n");
                header_str
                    .push_str("Access-Control-Allow-Headers: Content-Type, Authorization\r\n");
            }
        }

        header_str.push_str(&format!("Connection: {}\r\n", conn_hdr));
        for (hk, hv) in extra_headers {
            header_str.push_str(&format!("{}: {}\r\n", hk, hv));
        }
        header_str.push_str("\r\n");

        if reader.get_mut().write_all(header_str.as_bytes()).is_err() {
            return;
        }
        if reader.get_mut().write_all(&resp_body).is_err() {
            return;
        }
        let _ = reader.get_mut().flush();

        if !keep_alive || status >= 400 {
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
            <h1>Omni MCP Platform</h1>
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
