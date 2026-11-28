//! Production-grade Modular HTTP/1.1, Async TLS & JSON-RPC 2.0 Gateway Server for Omni MCP Platform (v6.5.0).
//!
//! Decomposed into focused submodules:
//! - `auth`: RBAC roles, HS256 & OIDC/JWKS verification, Origin/DNS-rebinding protection, CIMD validation
//! - `http_codec`: HTTP/1.1 framing helpers, JSON responses, static dashboard delivery, inbound `rustls` TLS config
//! - `rest_api`: Control plane REST management APIs (`/api/...`, `/healthz`, `/readyz`, `/metrics`)
//! - `mcp_router`: MCP JSON-RPC 2.0 routing (`/omni/mcp`, `/<service>/mcp`, `/groups/<slug>/mcp`, `run_stdio_server`)
//! - `sse`: Sync & non-blocking async SSE event streaming (`/api/v1/events`)
//! - `listener`: Non-blocking `tokio` + `rustls` `AsyncTlsStream` listener and `--no-default-features` thread pool

pub mod auth;
pub mod http_codec;
pub mod listener;
pub mod mcp_router;
pub mod rest_api;
pub mod sse;

pub use auth::{
    fetch_cimd_metadata, validate_cimd_metadata, validate_jwt_hs256, validate_jwt_oidc_jwks,
    AuthPrincipal, AuthRole,
};
pub use http_codec::percent_decode;
#[cfg(feature = "tls-rustls")]
pub use http_codec::{build_inbound_tls_config, build_inbound_tls_config_from_pem};
#[cfg(feature = "async-runtime")]
pub use listener::{handle_async_stream, serve_async_listener};
pub use listener::{handle_connection, handle_stream, run_server};
pub use mcp_router::run_stdio_server;

use crate::config::{get_config, OmniConfig};
use crate::control_plane::{
    AutoCrawler, MCPFxHeartbeatMonitor, MCPRegistry, ThirdPartyAuthExchange,
};
use crate::data_plane::{
    CircuitBreakerRegistry, InMemoryConfigCache, MCPProxyGateway, ProtocolTranslator, RateLimiter,
    SemanticResponseCache,
};
use crate::extension::OmniMCPServer;
use crate::json::JsonValue;
use crate::logging::now_epoch_secs;
use crate::models::{
    MutationCompensationSpec, ProtocolType, ServerRecord, ToolRecord, ToolStatus, TraceSpan,
};
use crate::storage::{MerkleAuditStore, WalRegistryStore};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
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

#[derive(Clone, Debug)]
pub struct OAuthPendingFlow {
    pub server_name: String,
    pub code_verifier: String,
    pub redirect_uri: String,
    pub token_endpoint: String,
    pub client_id: String,
    pub scope: String,
    pub created_at: f64,
}

#[derive(Clone)]
pub struct ProductionGatewayContext {
    pub config: OmniConfig,
    pub db_path: String,
    pub storage_backend_name: String,
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
    pub external_audit_sink: Option<Arc<dyn crate::storage::AuditSink>>,
    pub traces: Arc<RwLock<VecDeque<TraceSpan>>>,
    pub upstream_manager: Arc<crate::upstream::UpstreamManager>,
    pub event_bus: Arc<EventBus>,
    pub otlp_worker: Option<Arc<crate::telemetry::OtlpBatchWorker>>,
    pub oauth_pending_flows: Arc<RwLock<HashMap<String, OAuthPendingFlow>>>,
    // v6.8.0 Extensions Runtime
    pub skills: crate::control_plane::SkillEngine,
    pub task_manager: crate::data_plane::TaskManager,
    pub file_store: crate::storage::FileBlobStore,
}

impl ProductionGatewayContext {
    pub fn new(config: Option<OmniConfig>) -> Self {
        let config = config.unwrap_or_else(get_config);
        let _ = fs::create_dir_all(&config.data_dir);
        let mut db_path_buf = PathBuf::from(&config.data_dir);
        db_path_buf.push("omni_registry.db");
        let db_path = db_path_buf.to_string_lossy().into_owned();

        let store = Arc::new(WalRegistryStore::new(&db_path));

        // Dynamic storage backend selection via OMNI_STORAGE_BACKEND=wal|sqlite|postgres & OMNI_DATABASE_URL (v6.5.0)
        #[allow(unused_mut)]
        let mut storage_backend_name = "wal".to_string();
        #[allow(unused_mut)]
        let mut external_audit_sink: Option<Arc<dyn crate::storage::AuditSink>> = None;
        #[cfg(feature = "storage-postgres")]
        let mut pg_backend_for_listener: Option<
            Arc<crate::storage::PostgresRegistryBackend>,
        > = None;

        let registry = match config.storage_backend.as_str() {
            "sqlite" => {
                #[cfg(feature = "storage-sqlite")]
                {
                    let dsn = if !config.database_url.trim().is_empty() {
                        config.database_url.clone()
                    } else {
                        let mut p = PathBuf::from(&config.data_dir);
                        p.push("omni_registry.sqlite");
                        p.to_string_lossy().into_owned()
                    };
                    match crate::storage::SqliteRegistryBackend::try_open(dsn) {
                        Ok(sq) => {
                            let sq_arc = Arc::new(sq);
                            storage_backend_name = "sqlite".to_string();
                            external_audit_sink =
                                Some(sq_arc.clone() as Arc<dyn crate::storage::AuditSink>);
                            MCPRegistry::with_backend(
                                sq_arc as Arc<dyn crate::storage::RegistryStorageBackend>,
                            )
                        }
                        Err(e) => {
                            eprintln!(
                                "[!] Failed to open SQLite backend ({}); falling back to WAL",
                                e
                            );
                            MCPRegistry::new(Some(store.clone()))
                        }
                    }
                }
                #[cfg(not(feature = "storage-sqlite"))]
                {
                    eprintln!("[!] OMNI_STORAGE_BACKEND=sqlite requested without 'storage-sqlite' feature; falling back to WAL");
                    MCPRegistry::new(Some(store.clone()))
                }
            }
            "postgres" | "postgresql" => {
                #[cfg(feature = "storage-postgres")]
                {
                    let dsn = if !config.database_url.trim().is_empty() {
                        config.database_url.clone()
                    } else {
                        ":memory:".to_string()
                    };
                    match crate::storage::PostgresRegistryBackend::connect(dsn) {
                        Ok(pg) => {
                            let pg_arc = Arc::new(pg);
                            storage_backend_name = "postgres".to_string();
                            external_audit_sink =
                                Some(pg_arc.clone() as Arc<dyn crate::storage::AuditSink>);
                            pg_backend_for_listener = Some(pg_arc.clone());
                            MCPRegistry::with_backend(
                                pg_arc as Arc<dyn crate::storage::RegistryStorageBackend>,
                            )
                        }
                        Err(e) => {
                            eprintln!(
                                "[!] Failed to connect to PostgreSQL backend ({}); falling back to WAL",
                                e
                            );
                            MCPRegistry::new(Some(store.clone()))
                        }
                    }
                }
                #[cfg(not(feature = "storage-postgres"))]
                {
                    eprintln!("[!] OMNI_STORAGE_BACKEND=postgres requested without 'storage-postgres' feature; falling back to WAL");
                    MCPRegistry::new(Some(store.clone()))
                }
            }
            _ => MCPRegistry::new(Some(store.clone())),
        };

        let rate_limiter = RateLimiter::new(config.default_rate_limit_rpm);
        let native_monitor = MCPFxHeartbeatMonitor::new();
        let auth_exchange = ThirdPartyAuthExchange::new();
        let crawler = AutoCrawler::new(registry.clone(), Some(native_monitor.clone()), None);
        let cache = InMemoryConfigCache::new(registry.clone(), config.cache_refresh_interval_sec);
        let event_bus = Arc::new(EventBus::new());

        // v6.8.0: Initialize Fleet-aware Skills Engine, HA TaskManager, and Multi-Cloud FileBlobStore
        let skills = crate::control_plane::SkillEngine::new(registry.clone());
        let task_manager = crate::data_plane::TaskManager::new(
            registry.clone(),
            config.pod_id.clone(),
            config.max_concurrent_tasks,
            config.task_sync_grace_ms,
            config.task_ttl_sec,
        );
        {
            let eb_task = event_bus.clone();
            task_manager.set_event_sink(Arc::new(
                move |kind: &str, task: &crate::models::McpTaskRecord| {
                    eb_task.publish(kind, task.to_json());
                },
            ));
        }
        task_manager.start_ttl_reaper();
        let file_store = crate::storage::FileBlobStore::from_config(&config, registry.clone());
        {
            let fs_reaper = file_store.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
                let _ = fs_reaper.purge_expired();
            });
        }

        // Wire channel-dispatched PostgreSQL LISTEN/NOTIFY:
        // - `omni_registry_changed` -> reload registry & cache (<50ms convergence)
        // - `omni_task_changed` -> wake TaskManager cancellation flags + SSE (NO catalog reload storm!)
        #[cfg(feature = "storage-postgres")]
        if let Some(pg_arc) = pg_backend_for_listener {
            let reg_cb = registry.clone();
            let cache_cb = cache.clone();
            let task_cb = task_manager.clone();
            let _ = pg_arc.start_channel_invalidation_listener(move |channel, payload| {
                if channel == crate::storage::PostgresRegistryBackend::NOTIFY_TASK_CHANNEL {
                    task_cb.handle_remote_task_notification(payload);
                } else {
                    reg_cb.reload_from_backend();
                    cache_cb.refresh_now();
                }
            });
        }

        let upstream_manager = Arc::new(crate::upstream::UpstreamManager::new(Arc::new(
            config.clone(),
        )));
        upstream_manager.set_registry(registry.clone());

        // Wire Bidirectional Upstream Callback Hub -> EventBus + live policy resolver (v6.5.0)
        {
            let eb_prog = event_bus.clone();
            upstream_manager.callback_hub.set_progress_sink(Arc::new(
                move |srv: &str, notif: &JsonValue| {
                    eb_prog.publish(
                        "mcp.progress",
                        json!({
                            "server_name": srv,
                            "notification": notif.clone(),
                        }),
                    );
                },
            ));
            let eb_cb = event_bus.clone();
            upstream_manager
                .callback_hub
                .set_callback_event_sink(Arc::new(
                    move |event_kind: &str, cb: &crate::models::PendingUpstreamCallback| {
                        eb_cb.publish(event_kind, cb.to_json());
                    },
                ));
            let cache_pol = cache.clone();
            upstream_manager
                .callback_hub
                .set_policy_resolver(Arc::new(move |srv: &str| {
                    cache_pol
                        .get_server_arc(srv)
                        .map(|s| s.sampling_elicitation_policy.clone())
                        .unwrap_or(crate::models::SamplingElicitationPolicy::Deny)
                }));
        }

        let translator = ProtocolTranslator::new(None, Some(auth_exchange.clone()))
            .with_upstream(upstream_manager.clone(), cache.clone())
            .with_file_store(file_store.clone());
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
        if let Some(ref sink) = external_audit_sink {
            audit_store.set_external_sink(sink.clone());
        }
        let traces = Arc::new(RwLock::new(VecDeque::new()));

        // Wire background OTLP batch worker when OMNI_OTLP_ENDPOINT is configured (v6.5.0)
        let otlp_worker = config.otlp_endpoint.as_ref().map(|ep| {
            let worker = crate::telemetry::OtlpBatchWorker::new(
                ep.clone(),
                config.otlp_flush_interval_ms,
                crate::telemetry::DEFAULT_OTLP_BATCH_THRESHOLD,
            );
            worker.start();
            worker
        });

        let mut gateway =
            MCPProxyGateway::with_resilience(cache.clone(), translator, cb, sem_cache)
                .with_observability(audit_store.clone(), traces.clone())
                .with_upstream(upstream_manager.clone())
                .with_file_store(file_store.clone())
                .with_task_manager(task_manager.clone());
        if let Some(ref worker) = otlp_worker {
            gateway = gateway.with_otlp_worker(worker.clone());
        }
        let omni_mcp = OmniMCPServer::with_runtime(
            cache.clone(),
            gateway.clone(),
            skills.clone(),
            task_manager.clone(),
            file_store.clone(),
        );

        let ctx = Self {
            config,
            db_path,
            storage_backend_name,
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
            external_audit_sink,
            traces,
            upstream_manager,
            event_bus,
            otlp_worker,
            oauth_pending_flows: Arc::new(RwLock::new(HashMap::new())),
            skills,
            task_manager,
            file_store,
        };

        if ctx.config.demo_seed && ctx.registry.list_servers(None).is_empty() {
            ctx.seed_sample_catalog();
        }
        if ctx.config.seed_builtin_skills {
            let _ =
                crate::extension::plugin::seed_builtin_skills_into_registry(&ctx.registry, None);
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
}

static GLOBAL_CONTEXT: OnceLock<ProductionGatewayContext> = OnceLock::new();

pub fn get_gateway_context() -> &'static ProductionGatewayContext {
    GLOBAL_CONTEXT.get_or_init(|| ProductionGatewayContext::new(None))
}

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
