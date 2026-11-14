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
    TraceSpan, HEADER_MISMATCH, PARSE_ERROR, RATE_LIMITED,
};
use crate::storage::{MerkleAuditStore, WalRegistryStore};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(not(feature = "async-runtime"))]
use std::net::TcpListener;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
#[cfg(not(feature = "async-runtime"))]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(feature = "async-runtime"))]
use std::sync::{mpsc, Mutex};
use std::sync::{Arc, OnceLock, RwLock};
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

#[derive(Clone, Debug)]
pub struct GatewayEvent {
    pub seq: u64,
    pub event_type: String,
    pub payload: JsonValue,
}

pub struct EventBus {
    next_seq: AtomicU64,
    events: RwLock<VecDeque<GatewayEvent>>,
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            next_seq: AtomicU64::new(1),
            events: RwLock::new(VecDeque::with_capacity(256)),
        }
    }

    pub fn publish(&self, event_type: impl Into<String>, payload: JsonValue) -> u64 {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let ev = GatewayEvent {
            seq,
            event_type: event_type.into(),
            payload,
        };
        if let Ok(mut guard) = self.events.write() {
            if guard.len() >= 256 {
                guard.pop_front();
            }
            guard.push_back(ev);
        }
        seq
    }

    pub fn since(&self, last_seq: u64) -> Vec<GatewayEvent> {
        self.events
            .read()
            .map(|guard| guard.iter().filter(|e| e.seq > last_seq).cloned().collect())
            .unwrap_or_default()
    }

    pub fn latest_seq(&self) -> u64 {
        self.next_seq.load(Ordering::Relaxed).saturating_sub(1)
    }
}

impl Default for EventBus {
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
    pub traces: Arc<RwLock<VecDeque<TraceSpan>>>,
    pub upstream_manager: Arc<crate::upstream::UpstreamManager>,
    pub event_bus: Arc<EventBus>,
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

/// Pure zero-dependency URL percent-decode helper function.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h1 = bytes[i + 1];
            let h2 = bytes[i + 2];
            let d1 = (h1 as char).to_digit(16);
            let d2 = (h2 as char).to_digit(16);
            if let (Some(v1), Some(v2)) = (d1, d2) {
                out.push(((v1 << 4) | v2) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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

pub(crate) fn get_header<'a>(headers: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    headers.get(name).map(|s| s.as_str()).or_else(|| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    })
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
        let upstream_manager = Arc::new(crate::upstream::UpstreamManager::new(Arc::new(
            config.clone(),
        )));
        let translator = ProtocolTranslator::new(None, Some(auth_exchange.clone()))
            .with_upstream(upstream_manager.clone(), cache.clone());
        if config.demo_seed {
            translator.mesh_client.set_allow_synthetic_mocks(true);
        }
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
        let traces = Arc::new(RwLock::new(VecDeque::new()));

        let gateway = MCPProxyGateway::with_resilience(cache.clone(), translator, cb, sem_cache)
            .with_observability(audit_store.clone(), traces.clone())
            .with_upstream(upstream_manager.clone());
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
            upstream_manager,
            event_bus: Arc::new(EventBus::new()),
        };

        if ctx.config.demo_seed && ctx.registry.list_servers(None).is_empty() {
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
        let raw_path_clean = raw_path.split('?').next().unwrap_or(raw_path);
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
                | "/api/crawler/trigger" => AuthRole::Admin,
                "/api/codemode/suspended" | "/api/codemode/resume" | "/api/diffs/approve" => {
                    AuthRole::Approver
                }
                "/api/diffs"
                | "/api/traces"
                | "/api/audit"
                | "/api/audit/verify"
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
            return self.handle_get(path, headers);
        }

        if method.eq_ignore_ascii_case("POST") {
            return self.handle_post(path, headers, body_bytes);
        }

        if method.eq_ignore_ascii_case("DELETE") {
            return self.handle_delete(path, headers);
        }

        self.json_response(405, json!({"error": "Method not allowed"}))
    }

    pub fn authenticate(&self, headers: &HashMap<String, String>) -> Result<AuthPrincipal, u16> {
        let auth_hdr = get_header(headers, "authorization").unwrap_or("");
        let token = auth_hdr
            .strip_prefix("Bearer ")
            .or_else(|| auth_hdr.strip_prefix("bearer "))
            .map(|s| s.trim())
            .unwrap_or("");

        if token.is_empty() || token == "anonymous" {
            return Err(401);
        }

        // 0. If token is a 3-part JWT, check OIDC JWKS (RS256/ES256/EdDSA) or HS256 (OMNI_JWT_SECRET)
        if token.matches('.').count() == 2 {
            if let Ok(jwks_uri) = std::env::var("OMNI_OIDC_JWKS_URI") {
                if !jwks_uri.is_empty() {
                    let expected_iss = std::env::var("OMNI_OIDC_ISSUER")
                        .or_else(|_| std::env::var("OMNI_JWT_ISSUER"))
                        .ok();
                    let expected_aud = std::env::var("OMNI_OIDC_AUDIENCE")
                        .or_else(|_| std::env::var("OMNI_JWT_AUDIENCE"))
                        .ok();
                    return validate_jwt_oidc_jwks(
                        token,
                        &jwks_uri,
                        expected_iss.as_deref(),
                        expected_aud.as_deref(),
                    );
                }
            }
            if let Ok(jwt_secret) = std::env::var("OMNI_JWT_SECRET") {
                if !jwt_secret.is_empty() {
                    let expected_iss = std::env::var("OMNI_JWT_ISSUER").ok();
                    let expected_aud = std::env::var("OMNI_JWT_AUDIENCE").ok();
                    return validate_jwt_hs256(
                        token,
                        jwt_secret.as_bytes(),
                        expected_iss.as_deref(),
                        expected_aud.as_deref(),
                    );
                }
            }
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

        // 2. Dev/Test mode (only on loopback and non-production, with explicit dev token allowlist — never substring inference)
        let is_loopback = matches!(
            self.config.host.as_str(),
            "127.0.0.1" | "localhost" | "::1" | "[::1]"
        );
        let is_prod = std::env::var("OMNI_ENV")
            .map(|v| v.eq_ignore_ascii_case("production"))
            .unwrap_or(false);
        if !is_loopback || is_prod {
            return Err(401);
        }

        let role = match token {
            "admin_dev_token" => AuthRole::Admin,
            "approver_dev_token" => AuthRole::Approver,
            "operator_dev_token"
            | "test_operator_token"
            | "operator_key"
            | "operator_secret_token" => AuthRole::Operator,
            "agent_dev_token" | "agent_token" => AuthRole::Agent,
            _ => return Err(401),
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
            vec![(
                "WWW-Authenticate".to_string(),
                "Bearer error=\"insufficient_scope\"".to_string(),
            )],
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
            vec![(
                "WWW-Authenticate".to_string(),
                "Bearer resource_metadata=\"/.well-known/oauth-protected-resource\"".to_string(),
            )],
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

    fn handle_delete(
        &self,
        path: &str,
        _headers: &HashMap<String, String>,
    ) -> (u16, String, Vec<(String, String)>, Vec<u8>) {
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

    fn json_response_with_headers(
        &self,
        status: u16,
        data: JsonValue,
        headers: Vec<(String, String)>,
    ) -> (u16, String, Vec<(String, String)>, Vec<u8>) {
        (
            status,
            "application/json".to_string(),
            headers,
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

/// Opt-in background AutoSyncScheduler for external MCP servers with `sync_interval_sec`.
/// Updates `ServerHealth` (`Connected` / `Degraded` / `Unreachable`), applies exponential backoff
/// on failures, and syncs external tools, resources, and prompts.
#[derive(Default)]
pub struct AutoSyncScheduler {
    pub last_synced: HashMap<String, f64>,
    pub backoff_failures: HashMap<String, u32>,
}

impl AutoSyncScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tick(&mut self, ctx: &ProductionGatewayContext, now: f64) {
        for srv in ctx.registry.list_servers(None) {
            if srv.protocol != crate::models::ProtocolType::ExternalMcp {
                continue;
            }
            let Some(interval_sec) = srv.sync_interval_sec else {
                continue;
            };
            if interval_sec == 0 {
                continue;
            }
            let failures = self.backoff_failures.get(&srv.name).copied().unwrap_or(0);
            let effective_interval = interval_sec.saturating_mul(1u64 << failures.min(4));
            let prev = self.last_synced.get(&srv.name).copied().unwrap_or(0.0);
            if now - prev >= effective_interval as f64 {
                self.last_synced.insert(srv.name.clone(), now);
                match ctx.upstream_manager.fetch_tools(&srv) {
                    Ok(tools) => {
                        self.backoff_failures.remove(&srv.name);
                        let _ = ctx.registry.apply_external_sync(&srv.name, tools);
                        let ext_resources = ctx.upstream_manager.fetch_resources(&srv).ok();
                        let ext_prompts = ctx.upstream_manager.fetch_prompts(&srv).ok();
                        ctx.registry.update_server_sync_state(
                            &srv.name,
                            crate::models::ServerHealth::Connected,
                            Some(ToolStatus::Enabled),
                            ext_resources,
                            ext_prompts,
                            Some(now),
                        );
                        ctx.cache.refresh_now();
                        ctx.event_bus.publish(
                            "server.health",
                            json!({
                                "server": srv.name,
                                "health": "connected",
                                "timestamp": now
                            }),
                        );
                    }
                    Err(err) => {
                        let new_failures = failures.saturating_add(1);
                        self.backoff_failures.insert(srv.name.clone(), new_failures);
                        let health = if new_failures >= 3 {
                            crate::models::ServerHealth::Unreachable
                        } else {
                            crate::models::ServerHealth::Degraded
                        };
                        let new_status = if new_failures >= 3 {
                            Some(ToolStatus::Disabled)
                        } else {
                            None
                        };
                        ctx.registry.update_server_sync_state(
                            &srv.name,
                            health.clone(),
                            new_status,
                            None,
                            None,
                            None,
                        );
                        ctx.cache.refresh_now();
                        ctx.event_bus.publish(
                            "server.health",
                            json!({
                                "server": srv.name,
                                "health": health.as_str(),
                                "error": err,
                                "timestamp": now
                            }),
                        );
                    }
                }
            }
        }
    }
}

pub fn run_server(host: &str, port: u16) -> Result<(), String> {
    let ctx = get_gateway_context().clone();
    ctx.config.validate_bind(host)?;
    ctx.cache.start_background_refresh();

    // Opt-in background AutoSyncScheduler for external MCP servers with sync_interval_sec
    {
        let sync_ctx = ctx.clone();
        thread::spawn(move || {
            let mut scheduler = AutoSyncScheduler::new();
            loop {
                thread::sleep(Duration::from_secs(10));
                let now = crate::logging::now_epoch_secs();
                scheduler.tick(&sync_ctx, now);
            }
        });
    }

    let bind_addr = format!("{}:{}", host, port);

    #[cfg(feature = "tls-rustls")]
    let tls_config = build_inbound_tls_config()?;

    let scheme = {
        #[cfg(feature = "tls-rustls")]
        {
            if tls_config.is_some() {
                "https"
            } else {
                "http"
            }
        }
        #[cfg(not(feature = "tls-rustls"))]
        {
            "http"
        }
    };

    println!(
        "[*] Omni MCP Gateway running at {}://{}:{}",
        scheme, host, port
    );
    println!("[*] Liveness probe: {}://{}:{}/healthz", scheme, host, port);
    println!(
        "[*] Prometheus metrics: {}://{}:{}/metrics",
        scheme, host, port
    );
    println!(
        "[*] Omni MCP proxy available at {}://{}:{}/omni/mcp",
        scheme, host, port
    );
    println!(
        "[*] Dynamic virtual servers available at {}://{}:{}/<service>/mcp",
        scheme, host, port
    );

    #[cfg(feature = "async-runtime")]
    {
        run_async_server(
            &bind_addr,
            Arc::new(ctx),
            #[cfg(feature = "tls-rustls")]
            tls_config,
        )
    }

    #[cfg(not(feature = "async-runtime"))]
    {
        let listener = TcpListener::bind(&bind_addr)
            .map_err(|e| format!("Failed to bind HTTP server on {}: {}", bind_addr, e))?;
        let _ = listener.set_nonblocking(true);
        let shutdown = Arc::new(AtomicBool::new(false));

        // Bounded worker pool for zero-spawn-overhead request handling
        let num_workers = ctx.config.worker_threads.max(4);
        let queue_bound = (num_workers * 16).max(64);
        let (tx, rx) = mpsc::sync_channel::<TcpStream>(queue_bound);
        let shared_rx = Arc::new(Mutex::new(rx));
        let mut worker_handles = Vec::with_capacity(num_workers);

        for _ in 0..num_workers {
            let worker_rx = shared_rx.clone();
            let worker_ctx = ctx.clone();
            #[cfg(feature = "tls-rustls")]
            let worker_tls = tls_config.clone();
            worker_handles.push(thread::spawn(move || loop {
                let stream = {
                    let lock = worker_rx.lock().unwrap();
                    match lock.recv() {
                        Ok(s) => s,
                        Err(_) => break,
                    }
                };
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    #[cfg(feature = "tls-rustls")]
                    if let Some(ref cfg) = worker_tls {
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
                        if let Ok(conn) = rustls::ServerConnection::new(cfg.clone()) {
                            let tls_stream = rustls::StreamOwned::new(conn, stream);
                            handle_stream(tls_stream, &worker_ctx);
                        }
                        return;
                    }
                    let _ = stream.set_nonblocking(false);
                    handle_connection(stream, &worker_ctx);
                }));
            }));
        }

        while !shutdown.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nodelay(true);
                    match tx.try_send(stream) {
                        Ok(_) => {}
                        Err(mpsc::TrySendError::Full(mut saturated_stream)) => {
                            let _ = saturated_stream.set_nonblocking(false);
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
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }

        drop(tx);
        for h in worker_handles {
            let _ = h.join();
        }

        Ok(())
    }
}

#[cfg(feature = "async-runtime")]
fn run_async_server(
    bind_addr: &str,
    ctx: Arc<ProductionGatewayContext>,
    #[cfg(feature = "tls-rustls")] tls_config: Option<Arc<rustls::ServerConfig>>,
) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(ctx.config.worker_threads.max(4))
        .build()
        .map_err(|e| format!("Failed to build tokio runtime: {}", e))?;

    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(bind_addr)
            .await
            .map_err(|e| format!("Failed to bind async HTTP server on {}: {}", bind_addr, e))?;
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let ctrl_c_tx = shutdown_tx.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            let _ = ctrl_c_tx.send(true);
        });
        serve_async_listener(
            listener,
            ctx,
            #[cfg(feature = "tls-rustls")]
            tls_config,
            shutdown_rx,
        )
        .await;
        Ok(())
    })
}

#[cfg(feature = "async-runtime")]
pub async fn serve_async_listener(
    listener: tokio::net::TcpListener,
    ctx: Arc<ProductionGatewayContext>,
    #[cfg(feature = "tls-rustls")] tls_config: Option<Arc<rustls::ServerConfig>>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    let max_conns = (ctx.config.worker_threads.max(4) * 64).max(256);
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_conns));
    loop {
        tokio::select! {
            biased;
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            accept_res = listener.accept() => {
                match accept_res {
                    Ok((stream, _)) => {
                        let _ = stream.set_nodelay(true);
                        let permit = match semaphore.clone().try_acquire_owned() {
                            Ok(p) => p,
                            Err(_) => {
                                let mut s = stream;
                                use tokio::io::AsyncWriteExt;
                                let body = b"Service Unavailable: Worker accept queue saturated";
                                let resp = format!(
                                    "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                    body.len()
                                );
                                let _ = s.write_all(resp.as_bytes()).await;
                                let _ = s.write_all(body).await;
                                continue;
                            }
                        };
                        let conn_ctx = ctx.clone();
                        let conn_shutdown = shutdown_rx.clone();
                        #[cfg(feature = "tls-rustls")]
                        let conn_tls = tls_config.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            #[cfg(feature = "tls-rustls")]
                            if let Some(cfg) = conn_tls {
                                if let Ok(std_stream) = stream.into_std() {
                                    let _ = tokio::task::spawn_blocking(move || {
                                        let _ = std_stream.set_nonblocking(false);
                                        let _ = std_stream.set_read_timeout(Some(Duration::from_secs(10)));
                                        let _ = std_stream.set_write_timeout(Some(Duration::from_secs(10)));
                                        if let Ok(conn) = rustls::ServerConnection::new(cfg) {
                                            let tls_stream = rustls::StreamOwned::new(conn, std_stream);
                                            handle_stream(tls_stream, &conn_ctx);
                                        }
                                    }).await;
                                }
                                return;
                            }
                            handle_async_stream(stream, conn_ctx, conn_shutdown).await;
                        });
                    }
                    Err(_) => break,
                }
            }
        }
    }
    // Graceful drain: wait briefly for active connections to finish
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        semaphore.acquire_many(max_conns as u32),
    )
    .await;
}

#[cfg(feature = "async-runtime")]
pub async fn handle_async_stream<S>(
    stream: S,
    ctx: Arc<ProductionGatewayContext>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let mut reader = BufReader::new(stream);
    let mut request_count = 0usize;
    let max_requests_per_conn = 100usize;

    loop {
        if *shutdown_rx.borrow() || request_count >= max_requests_per_conn {
            break;
        }
        let mut request_line = String::new();
        let read_res =
            tokio::time::timeout(Duration::from_secs(15), reader.read_line(&mut request_line))
                .await;
        match read_res {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(_)) => {}
        }
        request_count += 1;
        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() < 2 {
            break;
        }
        let method = parts[0].to_string();
        let path = parts[1].to_string();

        let mut headers = HashMap::new();
        let mut content_length: usize = 0;
        let mut client_wants_close = false;
        let mut header_count = 0usize;

        loop {
            let mut h_line = String::new();
            match reader.read_line(&mut h_line).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let trimmed = h_line.trim_end_matches(&['\r', '\n'][..]);
            if trimmed.is_empty() {
                break;
            }
            header_count += 1;
            if header_count > 128 {
                return;
            }
            if let Some((k, v)) = trimmed.split_once(':') {
                let kl = k.trim().to_ascii_lowercase();
                let val = v.trim().to_string();
                if kl == "content-length" {
                    content_length = val.parse::<usize>().unwrap_or(0);
                } else if kl == "connection" && val.eq_ignore_ascii_case("close") {
                    client_wants_close = true;
                }
                headers.insert(kl, val);
            }
        }

        if content_length > ctx.config.max_body_size_bytes {
            let w = reader.get_mut();
            let body = b"Payload Too Large (Exceeds Limit)";
            let resp = format!(
                "HTTP/1.1 413 Payload Too Large\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = w.write_all(resp.as_bytes()).await;
            let _ = w.write_all(body).await;
            return;
        }

        let mut body_bytes = vec![0u8; content_length];
        if content_length > 0 && reader.read_exact(&mut body_bytes).await.is_err() {
            return;
        }

        let path_base = path.split('?').next().unwrap_or(&path).to_string();
        if method == "GET" && (path_base == "/api/events" || path_base == "/api/v1/events") {
            let ctx_c = ctx.clone();
            let m_c = method.clone();
            let p_c = path.clone();
            let h_c = headers.clone();
            let b_c = body_bytes.clone();
            let (status, _ctype, _extra, initial_body) = match tokio::task::spawn_blocking(
                move || ctx_c.handle_http(&m_c, &p_c, &h_c, &b_c),
            )
            .await
            {
                Ok(res) => res,
                Err(_) => return,
            };
            if status == 200 {
                let mut hdr = String::from(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-cache\r\nConnection: close\r\nX-Accel-Buffering: no\r\n",
                );
                if let Some(origin) = get_header(&headers, "origin") {
                    if is_origin_allowed(
                        origin,
                        &ctx.config.cors_allowed_origins,
                        get_header(&headers, "host"),
                    ) {
                        if ctx.config.cors_allowed_origins == "*" {
                            hdr.push_str("Access-Control-Allow-Origin: *\r\n");
                        } else {
                            hdr.push_str(&format!(
                                "Access-Control-Allow-Origin: {}\r\nVary: Origin\r\n",
                                origin
                            ));
                        }
                    }
                }
                hdr.push_str("\r\n");
                let w = reader.get_mut();
                if w.write_all(hdr.as_bytes()).await.is_err()
                    || w.write_all(&initial_body).await.is_err()
                    || w.flush().await.is_err()
                {
                    return;
                }
                let mut last_seq = ctx.event_bus.latest_seq();
                let mut idle_ticks = 0u32;
                loop {
                    tokio::select! {
                        _ = shutdown_rx.changed() => {
                            if *shutdown_rx.borrow() {
                                return;
                            }
                        }
                        _ = tokio::time::sleep(Duration::from_millis(200)) => {
                            let new_events = ctx.event_bus.since(last_seq);
                            if !new_events.is_empty() {
                                idle_ticks = 0;
                                for ev in new_events {
                                    last_seq = ev.seq;
                                    let frame = format!(
                                        "id: {}\nevent: {}\ndata: {}\n\n",
                                        ev.seq,
                                        ev.event_type,
                                        ev.payload.to_string_compact()
                                    );
                                    if w.write_all(frame.as_bytes()).await.is_err() || w.flush().await.is_err() {
                                        return;
                                    }
                                }
                            } else {
                                idle_ticks += 1;
                                if idle_ticks >= 75 {
                                    idle_ticks = 0;
                                    if w.write_all(b": keepalive\n\n").await.is_err() || w.flush().await.is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let ctx_c = ctx.clone();
        let m_c = method.clone();
        let p_c = path.clone();
        let h_c = headers.clone();
        let (status, content_type, extra_headers, resp_body) =
            match tokio::task::spawn_blocking(move || {
                ctx_c.handle_http(&m_c, &p_c, &h_c, &body_bytes)
            })
            .await
            {
                Ok(res) => res,
                Err(_) => return,
            };

        let status_text = match status {
            200 => "OK",
            202 => "Accepted",
            204 => "No Content",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            415 => "Unsupported Media Type",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            _ => "OK",
        };
        let conn_header = if client_wants_close || request_count >= max_requests_per_conn {
            "close"
        } else {
            "keep-alive"
        };
        let mut response_header = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\n",
            status,
            status_text,
            content_type,
            resp_body.len(),
            conn_header
        );
        for (ek, ev) in extra_headers {
            response_header.push_str(&format!("{}: {}\r\n", ek, ev));
        }
        if let Some(origin) = get_header(&headers, "origin") {
            if is_origin_allowed(
                origin,
                &ctx.config.cors_allowed_origins,
                get_header(&headers, "host"),
            ) {
                if ctx.config.cors_allowed_origins == "*" {
                    response_header.push_str("Access-Control-Allow-Origin: *\r\n");
                } else {
                    response_header.push_str(&format!(
                        "Access-Control-Allow-Origin: {}\r\nVary: Origin\r\n",
                        origin
                    ));
                }
                response_header
                    .push_str("Access-Control-Allow-Methods: GET, POST, DELETE, OPTIONS\r\n");
                response_header.push_str(
                    "Access-Control-Allow-Headers: Content-Type, Authorization, Mcp-Session-Id, Mcp-Protocol-Version, Last-Event-ID, traceparent, tracestate\r\n",
                );
            }
        }
        response_header.push_str("\r\n");

        let w = reader.get_mut();
        if w.write_all(response_header.as_bytes()).await.is_err()
            || (!resp_body.is_empty() && w.write_all(&resp_body).await.is_err())
            || w.flush().await.is_err()
        {
            break;
        }
        if conn_header == "close" {
            break;
        }
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

        let path_base = path.split('?').next().unwrap_or(&path);
        if method == "GET" && (path_base == "/api/events" || path_base == "/api/v1/events") {
            let (status, _ctype, _extra, initial_body) =
                ctx.handle_http(&method, &path, &headers, &body_bytes);
            if status == 200 {
                let mut hdr = String::from(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-cache\r\nConnection: close\r\nX-Accel-Buffering: no\r\n",
                );
                if let Some(origin) = get_header(&headers, "origin") {
                    if is_origin_allowed(
                        origin,
                        &ctx.config.cors_allowed_origins,
                        get_header(&headers, "host"),
                    ) {
                        if ctx.config.cors_allowed_origins == "*" {
                            hdr.push_str("Access-Control-Allow-Origin: *\r\n");
                        } else {
                            hdr.push_str(&format!(
                                "Access-Control-Allow-Origin: {}\r\nVary: Origin\r\n",
                                origin
                            ));
                        }
                    }
                }
                hdr.push_str("\r\n");
                let w = reader.get_mut();
                if w.write_all(hdr.as_bytes()).is_err()
                    || w.write_all(&initial_body).is_err()
                    || w.flush().is_err()
                {
                    return;
                }
                let mut last_seq = ctx.event_bus.latest_seq();
                let mut idle_ticks = 0u32;
                loop {
                    thread::sleep(Duration::from_millis(500));
                    let new_events = ctx.event_bus.since(last_seq);
                    if !new_events.is_empty() {
                        idle_ticks = 0;
                        for ev in new_events {
                            last_seq = ev.seq;
                            let frame = format!(
                                "id: {}\nevent: {}\ndata: {}\n\n",
                                ev.seq,
                                ev.event_type,
                                ev.payload.to_string_compact()
                            );
                            if w.write_all(frame.as_bytes()).is_err() || w.flush().is_err() {
                                return;
                            }
                        }
                    } else {
                        idle_ticks += 1;
                        if idle_ticks >= 30 {
                            idle_ticks = 0;
                            if w.write_all(b": keepalive\n\n").is_err() || w.flush().is_err() {
                                return;
                            }
                        }
                    }
                }
            }
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
        if let Some(origin) = get_header(&headers, "origin") {
            if status != 403
                && is_origin_allowed(
                    origin,
                    &ctx.config.cors_allowed_origins,
                    get_header(&headers, "host"),
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

fn base64url_decode(input: &str) -> Result<Vec<u8>, u16> {
    let mut out = Vec::with_capacity((input.len() * 3) / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for b in input.bytes() {
        if b == b'=' {
            break;
        }
        let val = match b {
            b'A'..=b'Z' => (b - b'A') as u32,
            b'a'..=b'z' => (b - b'a' + 26) as u32,
            b'0'..=b'9' => (b - b'0' + 52) as u32,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return Err(401),
        };
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

pub fn validate_jwt_hs256(
    token: &str,
    secret: &[u8],
    expected_iss: Option<&str>,
    expected_aud: Option<&str>,
) -> Result<AuthPrincipal, u16> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(401);
    }
    let header_bytes = base64url_decode(parts[0])?;
    let payload_bytes = base64url_decode(parts[1])?;
    let sig_bytes = base64url_decode(parts[2])?;

    let header_str = std::str::from_utf8(&header_bytes).map_err(|_| 401u16)?;
    let header_json = JsonValue::parse(header_str).map_err(|_| 401u16)?;
    let alg = header_json
        .get("alg")
        .and_then(|x| x.as_str())
        .unwrap_or("");
    if alg != "HS256" {
        return Err(401);
    }

    let signing_input = format!("{}.{}", parts[0], parts[1]);
    let expected_sig = crate::storage::audit_log::hmac_sha256(secret, signing_input.as_bytes());
    if !crate::models::constant_time_eq(&sig_bytes, &expected_sig) {
        return Err(401);
    }

    let payload_str = std::str::from_utf8(&payload_bytes).map_err(|_| 401u16)?;
    let claims = JsonValue::parse(payload_str).map_err(|_| 401u16)?;
    let now = crate::logging::now_epoch_secs();

    if let Some(exp) = claims.get("exp").and_then(|x| x.as_f64()) {
        if now >= exp {
            return Err(401);
        }
    }
    if let Some(nbf) = claims.get("nbf").and_then(|x| x.as_f64()) {
        if now < nbf {
            return Err(401);
        }
    }
    if let Some(req_iss) = expected_iss {
        let iss = claims.get("iss").and_then(|x| x.as_str()).unwrap_or("");
        if iss != req_iss {
            return Err(401);
        }
    }
    if let Some(req_aud) = expected_aud {
        let aud_ok = match claims.get("aud") {
            Some(v) if v.as_str() == Some(req_aud) => true,
            Some(v) => v
                .as_array()
                .map(|arr| arr.iter().any(|x| x.as_str() == Some(req_aud)))
                .unwrap_or(false),
            None => false,
        };
        if !aud_ok {
            return Err(401);
        }
    }

    let sub = claims
        .get("sub")
        .and_then(|x| x.as_str())
        .unwrap_or("jwt_user")
        .to_string();
    let role_str = claims
        .get("role")
        .and_then(|x| x.as_str())
        .or_else(|| claims.get("scope").and_then(|x| x.as_str()))
        .unwrap_or("agent");
    let role = if role_str.contains("admin") {
        AuthRole::Admin
    } else if role_str.contains("approver") {
        AuthRole::Approver
    } else if role_str.contains("operator") {
        AuthRole::Operator
    } else {
        AuthRole::Agent
    };

    Ok(AuthPrincipal { id: sub, role })
}

/// Cached JWKS entry: `(fetched_at_epoch_secs, parsed_jwks_json)`.
#[cfg(feature = "tls-rustls")]
static JWKS_CACHE: OnceLock<RwLock<HashMap<String, (f64, JsonValue)>>> = OnceLock::new();
#[cfg(feature = "tls-rustls")]
const JWKS_CACHE_TTL_SECS: f64 = 300.0;

#[cfg(feature = "tls-rustls")]
fn fetch_jwks_cached(jwks_uri: &str, force_refresh: bool) -> Result<JsonValue, u16> {
    if jwks_uri.starts_with('{') {
        return JsonValue::parse(jwks_uri).map_err(|_| 401u16);
    }
    let cache = JWKS_CACHE.get_or_init(|| RwLock::new(HashMap::new()));
    let now = now_epoch_secs();
    if !force_refresh {
        if let Ok(guard) = cache.read() {
            if let Some((fetched_at, doc)) = guard.get(jwks_uri) {
                if (now - *fetched_at) < JWKS_CACHE_TTL_SECS {
                    return Ok(doc.clone());
                }
            }
        }
    }
    let jwks_raw = if let Some(path) = jwks_uri.strip_prefix("file://") {
        fs::read_to_string(path).map_err(|_| 401u16)?
    } else {
        let client = crate::data_plane::HttpDownstreamClient::new();
        client.get_raw(jwks_uri, None).map_err(|_| 401u16)?
    };
    let doc = JsonValue::parse(&jwks_raw).map_err(|_| 401u16)?;
    if let Ok(mut guard) = cache.write() {
        guard.insert(jwks_uri.to_string(), (now, doc.clone()));
    }
    Ok(doc)
}

#[cfg(feature = "tls-rustls")]
fn pad_be_32(bytes: &[u8]) -> Option<[u8; 32]> {
    if bytes.len() > 32 {
        return None;
    }
    let mut out = [0u8; 32];
    let offset = 32 - bytes.len();
    out[offset..].copy_from_slice(bytes);
    Some(out)
}

#[cfg(feature = "tls-rustls")]
fn verify_jwk_signature(
    alg: &str,
    jwk: &JsonValue,
    signing_input: &[u8],
    sig_bytes: &[u8],
) -> bool {
    match alg {
        "RS256" => {
            if jwk.get("kty").and_then(|v| v.as_str()) != Some("RSA") {
                return false;
            }
            let n = match jwk.get("n").and_then(|v| v.as_str()).map(base64url_decode) {
                Some(Ok(b)) if !b.is_empty() => b,
                _ => return false,
            };
            let e = match jwk.get("e").and_then(|v| v.as_str()).map(base64url_decode) {
                Some(Ok(b)) if !b.is_empty() => b,
                _ => return false,
            };
            let key = ring::signature::RsaPublicKeyComponents { n: &n, e: &e };
            key.verify(
                &ring::signature::RSA_PKCS1_2048_8192_SHA256,
                signing_input,
                sig_bytes,
            )
            .is_ok()
        }
        "ES256" => {
            if jwk.get("kty").and_then(|v| v.as_str()) != Some("EC")
                || jwk.get("crv").and_then(|v| v.as_str()) != Some("P-256")
            {
                return false;
            }
            let x = match jwk.get("x").and_then(|v| v.as_str()).map(base64url_decode) {
                Some(Ok(b)) => match pad_be_32(&b) {
                    Some(arr) => arr,
                    None => return false,
                },
                _ => return false,
            };
            let y = match jwk.get("y").and_then(|v| v.as_str()).map(base64url_decode) {
                Some(Ok(b)) => match pad_be_32(&b) {
                    Some(arr) => arr,
                    None => return false,
                },
                _ => return false,
            };
            let mut uncompressed = [0u8; 65];
            uncompressed[0] = 0x04;
            uncompressed[1..33].copy_from_slice(&x);
            uncompressed[33..65].copy_from_slice(&y);
            ring::signature::UnparsedPublicKey::new(
                &ring::signature::ECDSA_P256_SHA256_FIXED,
                &uncompressed,
            )
            .verify(signing_input, sig_bytes)
            .is_ok()
        }
        "EdDSA" => {
            if jwk.get("kty").and_then(|v| v.as_str()) != Some("OKP")
                || jwk.get("crv").and_then(|v| v.as_str()) != Some("Ed25519")
            {
                return false;
            }
            let x = match jwk.get("x").and_then(|v| v.as_str()).map(base64url_decode) {
                Some(Ok(b)) if b.len() == 32 => b,
                _ => return false,
            };
            ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &x)
                .verify(signing_input, sig_bytes)
                .is_ok()
        }
        _ => false,
    }
}

#[cfg(feature = "tls-rustls")]
fn extract_role_from_claims(claims: &JsonValue) -> AuthRole {
    let mut tokens: Vec<String> = Vec::new();
    if let Some(role_s) = claims.get("role").and_then(|x| x.as_str()) {
        tokens.extend(role_s.split_whitespace().map(|s| s.to_lowercase()));
    }
    if let Some(roles_arr) = claims.get("roles").and_then(|x| x.as_array()) {
        for r in roles_arr {
            if let Some(s) = r.as_str() {
                tokens.push(s.to_lowercase());
            }
        }
    } else if let Some(roles_s) = claims.get("roles").and_then(|x| x.as_str()) {
        tokens.extend(roles_s.split_whitespace().map(|s| s.to_lowercase()));
    }
    if let Some(scope_s) = claims.get("scope").and_then(|x| x.as_str()) {
        tokens.extend(scope_s.split_whitespace().map(|s| s.to_lowercase()));
    }
    if let Some(scp_arr) = claims.get("scp").and_then(|x| x.as_array()) {
        for r in scp_arr {
            if let Some(s) = r.as_str() {
                tokens.push(s.to_lowercase());
            }
        }
    } else if let Some(scp_s) = claims.get("scp").and_then(|x| x.as_str()) {
        tokens.extend(scp_s.split_whitespace().map(|s| s.to_lowercase()));
    }

    let has = |needle: &str| {
        tokens
            .iter()
            .any(|t| t == needle || t.ends_with(&format!(":{}", needle)))
    };
    if has("admin") {
        AuthRole::Admin
    } else if has("approver") {
        AuthRole::Approver
    } else if has("operator") {
        AuthRole::Operator
    } else {
        AuthRole::Agent
    }
}

/// Validates a JWT against a remote/local OIDC JWKS document (`RS256` / `ES256` / `EdDSA`),
/// enforcing cryptographic signature verification (`ring::signature`), `kid` matching,
/// `exp`/`nbf`, `iss`, and RFC 8707 `aud` resource indicator.
pub fn validate_jwt_oidc_jwks(
    token: &str,
    jwks_uri: &str,
    expected_iss: Option<&str>,
    expected_aud: Option<&str>,
) -> Result<AuthPrincipal, u16> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(401);
    }
    let header_bytes = base64url_decode(parts[0])?;
    let header_json =
        JsonValue::parse(&String::from_utf8_lossy(&header_bytes)).map_err(|_| 401u16)?;
    let alg = header_json
        .get("alg")
        .and_then(|a| a.as_str())
        .unwrap_or("");
    if !matches!(alg, "RS256" | "ES256" | "EdDSA") {
        return Err(401);
    }
    let kid = header_json.get("kid").and_then(|k| k.as_str());
    let sig_bytes = base64url_decode(parts[2])?;
    if sig_bytes.is_empty() {
        return Err(401);
    }
    let signing_input = format!("{}.{}", parts[0], parts[1]);

    #[cfg(not(feature = "tls-rustls"))]
    {
        let _ = (
            jwks_uri,
            expected_iss,
            expected_aud,
            kid,
            signing_input,
            parts,
        );
        Err(401)
    }

    #[cfg(feature = "tls-rustls")]
    {
        let mut verified = false;
        for attempt in 0..2 {
            let jwks = fetch_jwks_cached(jwks_uri, attempt == 1)?;
            let keys = jwks.get("keys").and_then(|k| k.as_array()).ok_or(401u16)?;
            for k in keys {
                let k_kid = k.get("kid").and_then(|x| x.as_str());
                let k_alg = k.get("alg").and_then(|x| x.as_str());
                let kid_ok = match (kid, k_kid) {
                    (Some(a), Some(b)) => a == b,
                    (None, _) => true,
                    _ => false,
                };
                let alg_ok = k_alg.map(|a| a == alg).unwrap_or(true);
                if kid_ok
                    && alg_ok
                    && verify_jwk_signature(alg, k, signing_input.as_bytes(), &sig_bytes)
                {
                    verified = true;
                    break;
                }
            }
            if verified || jwks_uri.starts_with('{') {
                break;
            }
        }
        if !verified {
            return Err(401);
        }

        let claims_bytes = base64url_decode(parts[1])?;
        let claims =
            JsonValue::parse(&String::from_utf8_lossy(&claims_bytes)).map_err(|_| 401u16)?;
        let now = now_epoch_secs();
        if let Some(exp) = claims.get("exp").and_then(|x| x.as_f64()) {
            if now > exp {
                return Err(401);
            }
        }
        if let Some(nbf) = claims.get("nbf").and_then(|x| x.as_f64()) {
            if now < nbf {
                return Err(401);
            }
        }
        if let Some(iss_req) = expected_iss {
            if claims.get("iss").and_then(|x| x.as_str()) != Some(iss_req) {
                return Err(401);
            }
        }
        if let Some(aud_req) = expected_aud {
            let aud_ok = match claims.get("aud") {
                Some(JsonValue::Str(s)) => s == aud_req,
                Some(JsonValue::Array(arr)) => arr.iter().any(|v| v.as_str() == Some(aud_req)),
                _ => false,
            };
            if !aud_ok {
                return Err(401);
            }
        }

        let sub = claims
            .get("sub")
            .and_then(|x| x.as_str())
            .unwrap_or("oidc_user")
            .to_string();
        let role = extract_role_from_claims(&claims);
        Ok(AuthPrincipal { id: sub, role })
    }
}

/// Validates a Client ID Metadata Document (CIMD) per MCP OAuth 2.1 draft.
pub fn validate_cimd_metadata(doc: &JsonValue, expected_client_id: &str) -> Result<(), String> {
    let client_id = doc
        .get("client_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "CIMD document missing 'client_id'".to_string())?;
    if client_id != expected_client_id {
        return Err(format!(
            "CIMD 'client_id' mismatch: expected '{}', got '{}'",
            expected_client_id, client_id
        ));
    }
    let redirect_uris = doc
        .get("redirect_uris")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "CIMD document missing 'redirect_uris' array".to_string())?;
    if redirect_uris.is_empty() {
        return Err("CIMD 'redirect_uris' must not be empty".to_string());
    }
    for u in redirect_uris {
        let s = u
            .as_str()
            .ok_or_else(|| "CIMD 'redirect_uris' must contain strings".to_string())?;
        if !s.contains("://") {
            return Err(format!("Invalid CIMD redirect_uri '{}'", s));
        }
    }
    Ok(())
}

/// Fetches and validates a Client ID Metadata Document (CIMD) from an HTTPS/HTTP client_id URL.
pub fn fetch_cimd_metadata(client_id_url: &str) -> Result<JsonValue, String> {
    if !client_id_url.starts_with("https://") && !client_id_url.starts_with("http://") {
        return Err("CIMD client_id must be an http(s) URL".to_string());
    }
    let client = crate::data_plane::HttpDownstreamClient::new();
    let raw = client.get_raw(client_id_url, None)?;
    let doc = JsonValue::parse(&raw)?;
    validate_cimd_metadata(&doc, client_id_url)?;
    Ok(doc)
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
                if let Some(der) = base64_decode_standard(&b64) {
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
fn base64_decode_standard(input: &str) -> Option<Vec<u8>> {
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

#[cfg(feature = "tls-rustls")]
pub fn build_inbound_tls_config_from_pem(
    cert_pem: &str,
    key_pem: &str,
    client_ca_pem: Option<&str>,
) -> Result<Arc<rustls::ServerConfig>, String> {
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        decode_pem_blocks(cert_pem, "BEGIN CERTIFICATE", "END CERTIFICATE")
            .into_iter()
            .map(rustls::pki_types::CertificateDer::from)
            .collect();
    if certs.is_empty() {
        return Err("No certificates found in OMNI_TLS_CERT".to_string());
    }

    let key_der = decode_pem_blocks(key_pem, "BEGIN PRIVATE KEY", "END PRIVATE KEY")
        .into_iter()
        .next()
        .map(rustls::pki_types::PrivatePkcs8KeyDer::from)
        .map(rustls::pki_types::PrivateKeyDer::Pkcs8)
        .or_else(|| {
            decode_pem_blocks(key_pem, "BEGIN RSA PRIVATE KEY", "END RSA PRIVATE KEY")
                .into_iter()
                .next()
                .map(rustls::pki_types::PrivatePkcs1KeyDer::from)
                .map(rustls::pki_types::PrivateKeyDer::Pkcs1)
        })
        .or_else(|| {
            decode_pem_blocks(key_pem, "BEGIN EC PRIVATE KEY", "END EC PRIVATE KEY")
                .into_iter()
                .next()
                .map(rustls::pki_types::PrivateSec1KeyDer::from)
                .map(rustls::pki_types::PrivateKeyDer::Sec1)
        })
        .ok_or_else(|| "No supported private key found in OMNI_TLS_KEY".to_string())?;

    let builder = rustls::ServerConfig::builder();
    let mut cfg = if let Some(ca_pem) = client_ca_pem {
        let ca_certs: Vec<rustls::pki_types::CertificateDer<'static>> =
            decode_pem_blocks(ca_pem, "BEGIN CERTIFICATE", "END CERTIFICATE")
                .into_iter()
                .map(rustls::pki_types::CertificateDer::from)
                .collect();
        if ca_certs.is_empty() {
            return Err("No CA certificates found in OMNI_TLS_CLIENT_CA".to_string());
        }
        let mut roots = rustls::RootCertStore::empty();
        for c in ca_certs {
            roots
                .add(c)
                .map_err(|e| format!("Invalid client CA certificate: {}", e))?;
        }
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|e| format!("Failed to build mTLS client verifier: {}", e))?;
        builder
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs, key_der)
            .map_err(|e| format!("Invalid inbound mTLS certificate/key: {}", e))?
    } else {
        builder
            .with_no_client_auth()
            .with_single_cert(certs, key_der)
            .map_err(|e| format!("Invalid inbound TLS certificate/key: {}", e))?
    };
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

#[cfg(feature = "tls-rustls")]
pub fn build_inbound_tls_config() -> Result<Option<Arc<rustls::ServerConfig>>, String> {
    let cert_path = match std::env::var("OMNI_TLS_CERT") {
        Ok(p) if !p.trim().is_empty() => p,
        _ => return Ok(None),
    };
    let key_path = std::env::var("OMNI_TLS_KEY")
        .map_err(|_| "OMNI_TLS_KEY must be set when OMNI_TLS_CERT is configured".to_string())?;

    let cert_pem = fs::read_to_string(&cert_path)
        .map_err(|e| format!("Failed to read OMNI_TLS_CERT '{}': {}", cert_path, e))?;
    let key_pem = fs::read_to_string(&key_path)
        .map_err(|e| format!("Failed to read OMNI_TLS_KEY '{}': {}", key_path, e))?;
    let client_ca_pem = match std::env::var("OMNI_TLS_CLIENT_CA") {
        Ok(ca_path) if !ca_path.trim().is_empty() => Some(
            fs::read_to_string(&ca_path)
                .map_err(|e| format!("Failed to read OMNI_TLS_CLIENT_CA '{}': {}", ca_path, e))?,
        ),
        _ => None,
    };

    build_inbound_tls_config_from_pem(&cert_pem, &key_pem, client_ca_pem.as_deref()).map(Some)
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
