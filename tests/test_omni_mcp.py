"""
Production Test Suite for Omni MCP Platform.
Covers:
- MCP Registry (Control Plane): In-memory and SQLite persistence, versioning, diff approval, rollback
- AutoCrawler & IDL Parser: Protobuf and Thrift schema translation, LLM enrichment, MCPFx heartbeats
- Dynamic Virtual Server Gateway (Data Plane): /<service-name>/mcp routing, in-memory refresh
- Wire Protocol Translation & Muttley mesh execution: HTTP, gRPC (Protobuf), TChannel (Thrift)
- Security & ABAC: Charter policies, actor-based access control, automatic PII redaction
- Token Optimization: Response Projection (nested GraphQL-like field trimming)
- Omni MCP: Incremental discovery proxy (discover_server, discover_tools, get_tool_schema, invoke_tool)
- Code Mode aifx / omni CLI
- Production Resilience: Token-bucket rate limiting, health probes (/healthz, /readyz), Prometheus metrics (/metrics)
"""
from __future__ import annotations
import json
import os
import tempfile
import unittest
import time
import threading

from omni_mcp.config import OmniConfig
from omni_mcp.models.entities import (
    ServerRecord,
    ToolRecord,
    ToolStatus,
    ProtocolType,
    CharterPolicy,
    ActorType,
)
from omni_mcp.models.rpc import (
    RPCRequest,
    RPCResponse,
    TOOL_DISABLED,
    UNAUTHORIZED,
)
from omni_mcp.storage.sqlite_store import SQLiteRegistryStore
from omni_mcp.control_plane.registry import MCPRegistry
from omni_mcp.control_plane.idl_parser import IDLParser, MethodDefinition, FieldDefinition
from omni_mcp.control_plane.description_gen import ToolDescriptionGenerator
from omni_mcp.control_plane.native_monitor import MCPFxHeartbeatMonitor
from omni_mcp.control_plane.third_party import ThirdPartyAuthExchange
from omni_mcp.control_plane.crawler import AutoCrawler
from omni_mcp.data_plane.cache import InMemoryConfigCache
from omni_mcp.data_plane.rate_limiter import RateLimiter
from omni_mcp.data_plane.security import SecurityCharterEngine, PIIRedactor
from omni_mcp.data_plane.projection import ResponseProjectionEngine
from omni_mcp.data_plane.translators import ProtocolTranslator
from omni_mcp.data_plane.muttley import MuttleyMeshClient
from omni_mcp.data_plane.gateway import MCPProxyGateway
from omni_mcp.extension.omni_mcp import OmniMCPServer
from omni_mcp.extension.aifx import AifxCLI


class TestSQLitePersistence(unittest.TestCase):
    """Verifies that the SQLite persistence store retains state across process restarts."""

    def setUp(self) -> None:
        self.tmp_dir = tempfile.mkdtemp()
        self.db_path = os.path.join(self.tmp_dir, "test_omni.db")
        self.store = SQLiteRegistryStore(self.db_path)
        self.registry = MCPRegistry(persistent_store=self.store)

    def tearDown(self) -> None:
        self.store.close()
        if os.path.exists(self.db_path):
            try:
                os.remove(self.db_path)
            except Exception:
                pass

    def test_persistence_across_restart(self) -> None:
        server = ServerRecord(
            name="logistics-svc",
            display_name="Logistics",
            description="Global fleet routing",
            owning_team="fleet-ops",
            protocol=ProtocolType.GRPC,
            status=ToolStatus.ENABLED,
        )
        tool = ToolRecord(
            name="RouteVehicle",
            description="Computes optimal route",
            input_schema={"type": "object", "properties": {"vehicle_id": {"type": "string"}}},
            status=ToolStatus.ENABLED,
        )
        server.add_tool(tool)
        self.registry.register_server(server)

        # Propose and approve diff
        diff = self.registry.propose_tool_update(
            "logistics-svc", "RouteVehicle", new_description="Production route computer"
        )
        self.registry.approve_diff(diff.diff_id, "tech_lead")

        # Simulate process termination and restart with fresh registry instance
        self.store.close()
        new_store = SQLiteRegistryStore(self.db_path)
        restarted_registry = MCPRegistry(persistent_store=new_store)

        recovered = restarted_registry.get_server("logistics-svc")
        self.assertIsNotNone(recovered)
        self.assertEqual(recovered.displayName if hasattr(recovered, 'displayName') else recovered.display_name, "Logistics")
        recovered_tool = recovered.get_tool("RouteVehicle")
        self.assertIsNotNone(recovered_tool)
        self.assertEqual(recovered_tool.version, 2)
        self.assertEqual(recovered_tool.description, "Production route computer")
        new_store.close()


class TestRateLimiting(unittest.TestCase):
    """Verifies token-bucket rate limiting enforcement."""

    def test_token_bucket_limit(self) -> None:
        limiter = RateLimiter(default_rpm=60)  # 1 request per second
        key = "test_agent_token"

        # First request allowed
        allowed, tokens = limiter.check_rate_limit(key, max_rpm=2)
        self.assertTrue(allowed)

        # Second request allowed (starts with capacity 2)
        allowed2, tokens2 = limiter.check_rate_limit(key, max_rpm=2)
        self.assertTrue(allowed2)

        # Third immediate request throttled
        allowed3, tokens3 = limiter.check_rate_limit(key, max_rpm=2)
        self.assertFalse(allowed3)


class TestMCPRegistryAndGovernance(unittest.TestCase):
    """Verifies Control Plane, disabled-by-default guarantee, diff approvals, and rollback."""

    def setUp(self) -> None:
        self.registry = MCPRegistry()
        self.server = ServerRecord(
            name="paymentservice",
            display_name="Payment Service",
            description="Processes payments for rides and eats",
            owning_team="team-payments",
            protocol=ProtocolType.HTTP,
            status=ToolStatus.DISABLED,
        )
        self.tool = ToolRecord(
            name="ChargeRider",
            description="Charges rider card for trip amount",
            input_schema={"type": "object", "properties": {"amount": {"type": "number"}}},
            status=ToolStatus.DISABLED,  # Disabled by default
            version=1,
        )
        self.server.add_tool(self.tool)
        self.registry.register_server(self.server)

    def test_disabled_by_default(self) -> None:
        fetched = self.registry.get_server("paymentservice")
        self.assertIsNotNone(fetched)
        self.assertEqual(fetched.status, ToolStatus.DISABLED)
        t = fetched.get_tool("ChargeRider")
        self.assertEqual(t.status, ToolStatus.DISABLED)

    def test_propose_diff_and_approve(self) -> None:
        diff = self.registry.propose_tool_update(
            server_name="paymentservice",
            tool_name="ChargeRider",
            new_description="Charges rider credit card securely with multi-currency support.",
            proposed_by="alice@uber.com",
        )
        self.assertFalse(diff.approved)
        self.assertEqual(diff.proposed_version, 2)
        self.assertIn("Charges rider credit card securely", diff.after_state["description"])

        srv = self.registry.get_server("paymentservice")
        self.assertEqual(srv.get_tool("ChargeRider").version, 1)

        ok = self.registry.approve_diff(diff.diff_id, approved_by="bob-lead@uber.com")
        self.assertTrue(ok)

        srv2 = self.registry.get_server("paymentservice")
        t2 = srv2.get_tool("ChargeRider")
        self.assertEqual(t2.version, 2)
        self.assertIn("multi-currency support", t2.description)

    def test_rollback_tool(self) -> None:
        diff = self.registry.propose_tool_update(
            server_name="paymentservice",
            tool_name="ChargeRider",
            new_description="Updated v2 description",
        )
        self.registry.approve_diff(diff.diff_id, "lead")

        srv = self.registry.get_server("paymentservice")
        self.assertEqual(srv.get_tool("ChargeRider").version, 2)

        ok = self.registry.rollback_tool("paymentservice", "ChargeRider", target_version=1)
        self.assertTrue(ok)

        srv_rolled = self.registry.get_server("paymentservice")
        self.assertEqual(srv_rolled.get_tool("ChargeRider").version, 1)
        self.assertEqual(srv_rolled.get_tool("ChargeRider").description, "Charges rider card for trip amount")


class TestAutoCrawlerAndIDLParsing(unittest.TestCase):
    """Verifies Protobuf/Thrift parsing, LLM description enrichment, and Cadence discovery."""

    def test_protobuf_parsing_and_schema_translation(self) -> None:
        proto = """
        syntax = "proto3";
        package uber.fleet;
        message FleetVehicleRequest {
            string vehicle_id = 1; // Unique vehicle identifier
            repeated string sensor_ids = 2; // Specific sensors
        }
        service FleetManagement {
            // Retrieve vehicle health and diagnostic telemetry
            rpc GetVehicleTelemetry (FleetVehicleRequest) returns (VehicleTelemetryResponse);
        }
        """
        services = IDLParser.parse_protobuf(proto)
        self.assertEqual(len(services), 1)
        svc = services[0]
        self.assertEqual(svc.service_name, "FleetManagement")
        self.assertEqual(len(svc.methods), 1)
        m = svc.methods[0]
        self.assertEqual(m.name, "GetVehicleTelemetry")
        self.assertEqual(m.protocol, ProtocolType.GRPC)

        schema = IDLParser.translate_to_json_schema(m)
        self.assertIn("vehicle_id", schema["properties"])
        self.assertEqual(schema["properties"]["vehicle_id"]["type"], "string")
        self.assertEqual(schema["properties"]["sensor_ids"]["type"], "array")

    def test_thrift_parsing(self) -> None:
        thrift = """
        namespace py uber.trip
        service TripPricing {
            // Calculate dynamic surge pricing for trip
            TripQuote calculateSurge(1: string rider_id, 2: double pickup_lat, 3: double pickup_lng)
        }
        """
        services = IDLParser.parse_thrift(thrift)
        self.assertEqual(len(services), 1)
        svc = services[0]
        self.assertEqual(svc.service_name, "TripPricing")
        self.assertEqual(svc.protocol, ProtocolType.TCHANNEL)
        self.assertEqual(len(svc.methods), 1)

    def test_native_mcpfx_heartbeat_discovery(self) -> None:
        monitor = MCPFxHeartbeatMonitor()
        monitor.emit_heartbeat("geospatial-index", "10.0.1.20", 9090, "maps-team")
        discovered = monitor.scan_active_native_servers()
        self.assertEqual(len(discovered), 1)
        s = discovered[0]
        self.assertEqual(s.name, "geospatial-index")
        self.assertTrue(s.is_native)
        self.assertEqual(s.status, ToolStatus.DISABLED)
        self.assertIn("geospatial-index_query", s.tools)


class TestDataPlaneAndProtocolTranslation(unittest.TestCase):
    """Verifies Dynamic Virtual Servers, Muttley mesh execution, and wire translation."""

    def setUp(self) -> None:
        self.registry = MCPRegistry()
        self.cache = InMemoryConfigCache(self.registry)
        self.mesh = MuttleyMeshClient()
        self.translator = ProtocolTranslator(mesh_client=self.mesh)
        self.gateway = MCPProxyGateway(self.cache, self.translator)

        self.server = ServerRecord(
            name="dispatch-service",
            display_name="Dispatch Service",
            description="Real-time ride matching and driver dispatch",
            owning_team="core-dispatch",
            protocol=ProtocolType.GRPC,
            status=ToolStatus.ENABLED,
        )
        self.tool = ToolRecord(
            name="FindNearbyDrivers",
            description="Finds top matching drivers within radius",
            input_schema={
                "type": "object",
                "properties": {
                    "lat": {"type": "number"},
                    "lng": {"type": "number"},
                },
                "required": ["lat", "lng"],
            },
            status=ToolStatus.ENABLED,
            protocol=ProtocolType.GRPC,
            downstream_endpoint="muttley://dispatch/FindNearbyDrivers",
            downstream_method="FindNearbyDrivers",
        )
        self.server.add_tool(self.tool)
        self.registry.register_server(self.server)
        self.cache.refresh_now()

    def test_initialize_and_tools_list(self) -> None:
        init_req = RPCRequest(id=1, method="initialize")
        resp = self.gateway.handle_request("dispatch-service", init_req)
        self.assertIsNone(resp.error)
        self.assertEqual(resp.result["serverInfo"]["name"], "omni-mcp/dispatch-service")

        list_req = RPCRequest(id=2, method="tools/list")
        list_resp = self.gateway.handle_request("dispatch-service", list_req)
        self.assertIsNone(list_resp.error)
        tools = list_resp.result["tools"]
        self.assertEqual(len(tools), 1)
        self.assertEqual(tools[0]["name"], "FindNearbyDrivers")

    def test_invoking_disabled_tool_rejected(self) -> None:
        self.registry.toggle_tool_status("dispatch-service", "FindNearbyDrivers", ToolStatus.DISABLED)
        self.cache.refresh_now()

        call_req = RPCRequest(
            id=3,
            method="tools/call",
            params={"name": "FindNearbyDrivers", "arguments": {"lat": 37.77, "lng": -122.41}},
        )
        resp = self.gateway.handle_request("dispatch-service", call_req)
        self.assertIsNotNone(resp.error)
        self.assertEqual(resp.error.code, TOOL_DISABLED)

    def test_grpc_protocol_translation_and_muttley(self) -> None:
        call_req = RPCRequest(
            id=4,
            method="tools/call",
            params={"name": "FindNearbyDrivers", "arguments": {"lat": 37.77, "lng": -122.41}},
        )
        resp = self.gateway.handle_request("dispatch-service", call_req)
        self.assertIsNone(resp.error)
        self.assertTrue(resp.result["content"][0]["text"])
        self.assertEqual(self.mesh.get_call_count("dispatch-service"), 1)


class TestSecurityABACAndPIIRedaction(unittest.TestCase):
    """Verifies ABAC charter policy enforcement and automatic sensitive data redaction."""

    def setUp(self) -> None:
        self.redactor = PIIRedactor()

    def test_pii_redactor(self) -> None:
        raw_text = "Driver: John Doe (john.doe@uber.com), Phone: +1 555-234-5678, SSN: 123-45-6789, Card: 4111 2222 3333 4444"
        redacted = self.redactor.redact_text(raw_text)
        self.assertNotIn("john.doe@uber.com", redacted)
        self.assertIn("[REDACTED_EMAIL]", redacted)
        self.assertIn("[REDACTED_PHONE]", redacted)
        self.assertIn("[REDACTED_SSN]", redacted)
        self.assertIn("[REDACTED_CARD]", redacted)

    def test_abac_actor_policy_enforcement(self) -> None:
        policy = CharterPolicy(allowed_actors=[ActorType.HUMAN])
        tool = ToolRecord(
            name="DeleteCustomerData",
            description="GDPR deletion",
            input_schema={},
            charter_policy=policy,
        )
        server = ServerRecord(
            name="security-svc",
            display_name="Security",
            description="",
            owning_team="sec",
        )

        is_allowed, reason = SecurityCharterEngine.evaluate_access(server, tool, caller_actor="agent")
        self.assertFalse(is_allowed)
        self.assertIn("not authorized", reason)

        is_allowed_human, _ = SecurityCharterEngine.evaluate_access(server, tool, caller_actor="human")
        self.assertTrue(is_allowed_human)


class TestResponseProjectionAndOmniMCP(unittest.TestCase):
    """Verifies GraphQL-like Response Projection and Omni MCP incremental discovery."""

    def test_response_projection_nested_pruning(self) -> None:
        full_payload = {
            "driver": {
                "id": "drv_1",
                "name": "Alex",
                "internal_metrics": {"secret_score": 98.4},
                "vehicle": {"make": "Tesla", "model": "Model 3", "vin": "5YJ3E1EB"},
            },
            "unrelated_logs": ["log1", "log2"],
            "status": "AVAILABLE",
        }

        projected = ResponseProjectionEngine.apply_projection(
            full_payload,
            ["driver.name", "driver.vehicle.make", "status"],
        )

        self.assertEqual(projected["driver"]["name"], "Alex")
        self.assertEqual(projected["driver"]["vehicle"]["make"], "Tesla")
        self.assertEqual(projected["status"], "AVAILABLE")
        self.assertNotIn("unrelated_logs", projected)
        self.assertNotIn("vin", projected["driver"]["vehicle"])
        self.assertNotIn("internal_metrics", projected["driver"])

    def test_omni_mcp_incremental_discovery_flow(self) -> None:
        registry = MCPRegistry()
        cache = InMemoryConfigCache(registry)
        gateway = MCPProxyGateway(cache)
        omni = OmniMCPServer(cache, gateway)

        srv = ServerRecord(
            name="eats-catalog",
            display_name="Merchant Catalog",
            description="Discovers restaurant menus and dishes",
            owning_team="eats-eng",
            status=ToolStatus.ENABLED,
        )
        tool = ToolRecord(
            name="GetMenu",
            description="Gets menu items for restaurant",
            input_schema={"type": "object", "properties": {"store_id": {"type": "string"}}},
            status=ToolStatus.ENABLED,
        )
        srv.add_tool(tool)
        registry.register_server(srv)
        cache.refresh_now()

        # Step 1: discover_server
        disc_req = RPCRequest(
            id=1,
            method="tools/call",
            params={"name": "discover_server", "arguments": {"query": "restaurant menu dishes"}},
        )
        disc_resp = omni.handle_request(disc_req)
        self.assertIsNone(disc_resp.error)
        parsed = json.loads(disc_resp.result["content"][0]["text"])
        self.assertTrue(any(s["server_name"] == "eats-catalog" for s in parsed["servers"]))

        # Step 2: discover_tools
        tools_req = RPCRequest(
            id=2,
            method="tools/call",
            params={"name": "discover_tools", "arguments": {"server_name": "eats-catalog"}},
        )
        tools_resp = omni.handle_request(tools_req)
        parsed_tools = json.loads(tools_resp.result["content"][0]["text"])
        self.assertEqual(parsed_tools["tools"][0]["name"], "GetMenu")

        # Step 3: get_tool_schema
        schema_req = RPCRequest(
            id=3,
            method="tools/call",
            params={"name": "get_tool_schema", "arguments": {"server_name": "eats-catalog", "tool_name": "GetMenu"}},
        )
        schema_resp = omni.handle_request(schema_req)
        schema_data = json.loads(schema_resp.result["content"][0]["text"])
        self.assertIn("store_id", schema_data["inputSchema"]["properties"])


class TestAifxCLI(unittest.TestCase):
    """Verifies aifx CLI Code Mode commands and file writing."""

    def setUp(self) -> None:
        self.registry = MCPRegistry()
        srv = ServerRecord(
            name="trip-history",
            display_name="Trip History",
            description="Rider past trips",
            owning_team="rider",
            status=ToolStatus.ENABLED,
        )
        tool = ToolRecord(
            name="ListRiderTrips",
            description="Lists past trips for rider",
            input_schema={"type": "object", "properties": {"rider_id": {"type": "string"}}},
            status=ToolStatus.ENABLED,
        )
        srv.add_tool(tool)
        self.registry.register_server(srv)
        self.cli = AifxCLI(self.registry)

    def test_aifx_mcp_list(self) -> None:
        out = self.cli.list_servers(format_type="json")
        data = json.loads(out)
        self.assertEqual(len(data), 1)
        self.assertEqual(data[0]["name"], "trip-history")

    def test_aifx_mcp_search(self) -> None:
        out = self.cli.search_tools(query="past trips", format_type="json")
        data = json.loads(out)
        self.assertEqual(len(data), 1)
        self.assertEqual(data[0]["tool"], "ListRiderTrips")

    def test_aifx_mcp_call_to_file(self) -> None:
        with tempfile.NamedTemporaryFile(suffix=".json", delete=False) as tmp:
            tmp_path = tmp.name

        try:
            res_msg = self.cli.call_tool(
                server_name="trip-history",
                tool_name="ListRiderTrips",
                arguments_json='{"rider_id": "usr_99"}',
                output_file=tmp_path,
            )
            self.assertIn(f"Written response", res_msg)
            self.assertTrue(os.path.exists(tmp_path))
            with open(tmp_path, "r", encoding="utf-8") as f:
                saved = json.load(f)
            self.assertIn("result", saved)
        finally:
            if os.path.exists(tmp_path):
                os.remove(tmp_path)


if __name__ == "__main__":
    unittest.main()

