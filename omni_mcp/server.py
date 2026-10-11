"""
Production-grade Multi-Threaded HTTP & JSON-RPC Gateway Server for Omni MCP Platform.
Includes Health Checks (/healthz, /readyz), Prometheus Metrics (/metrics), Rate Limiting,
Structured Logging, SQLite Persistence, and Static UI Delivery.
"""
from __future__ import annotations
import json
import os
import sys
import time
import uuid
import signal
from http.server import HTTPServer, BaseHTTPRequestHandler
from socketserver import ThreadingMixIn
from typing import Optional, Dict, Any
from urllib.parse import urlparse

from omni_mcp.config import get_config, OmniConfig
from omni_mcp.logging import logger
from omni_mcp.models.entities import (
    ServerRecord,
    ToolRecord,
    ToolStatus,
    ProtocolType,
    ActorType,
)
from omni_mcp.models.rpc import RPCRequest
from omni_mcp.storage.sqlite_store import SQLiteRegistryStore
from omni_mcp.control_plane.registry import MCPRegistry
from omni_mcp.control_plane.crawler import AutoCrawler
from omni_mcp.control_plane.native_monitor import MCPFxHeartbeatMonitor
from omni_mcp.control_plane.third_party import ThirdPartyAuthExchange
from omni_mcp.data_plane.cache import InMemoryConfigCache
from omni_mcp.data_plane.rate_limiter import RateLimiter
from omni_mcp.data_plane.gateway import MCPProxyGateway
from omni_mcp.extension.omni_mcp import OmniMCPServer


class ThreadedHTTPServer(ThreadingMixIn, HTTPServer):
    """Handles requests in separate threads for enterprise multi-agent concurrency."""
    daemon_threads = True
    allow_reuse_address = True


class ProductionGatewayContext:
    def __init__(self, config: Optional[OmniConfig] = None) -> None:
        self.config = config or get_config()
        self.db_path = os.path.join(self.config.data_dir, "omni_registry.db")
        self.store = SQLiteRegistryStore(self.db_path)
        self.registry = MCPRegistry(persistent_store=self.store)
        self.rate_limiter = RateLimiter(default_rpm=self.config.default_rate_limit_rpm)
        self.native_monitor = MCPFxHeartbeatMonitor()
        self.auth_exchange = ThirdPartyAuthExchange()
        self.crawler = AutoCrawler(self.registry, self.native_monitor)
        self.cache = InMemoryConfigCache(
            self.registry,
            refresh_interval_seconds=self.config.cache_refresh_interval_sec,
        )
        self.gateway = MCPProxyGateway(self.cache)
        self.omni_mcp = OmniMCPServer(self.cache, self.gateway)

        # Telemetry metrics counters
        self.metrics = {
            "requests_total": 0,
            "errors_total": 0,
            "rate_limit_hits_total": 0,
            "start_time": time.time(),
        }

        # Seed catalog if store is freshly initialized
        if len(self.registry.list_servers()) == 0:
            self._seed_sample_catalog()

    def _seed_sample_catalog(self) -> None:
        # 1. Driver Location & Dispatch Service (gRPC)
        driver_proto = """
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
        """
        self.crawler.register_idl_source("driver-dispatch", driver_proto)

        # 2. Trip Billing Service (Thrift / TChannel)
        trip_thrift = """
        namespace py uber.trip
        service TripBillingService {
            // Query fare estimates and receipt line items for rider trip
            TripFare getTripFare(1: string trip_id, 2: bool include_discounts)
        }
        """
        self.crawler.register_idl_source("trip-billing", trip_thrift)

        # 3. Third-party integrations (Jira & Google)
        jira_srv = self.auth_exchange.create_third_party_server("jira", "eng-productivity")
        self.registry.register_server(jira_srv)

        # Run discovery cycle
        self.crawler.run_discovery_workflow()

        # Enable DriverDispatchService for out-of-the-box readiness
        self.registry.toggle_server_status("driverdispatchservice", ToolStatus.ENABLED)
        self.registry.toggle_tool_status("driverdispatchservice", "GetDriverLocation", ToolStatus.ENABLED)
        self.cache.refresh_now()


_GLOBAL_CONTEXT: Optional[ProductionGatewayContext] = None


def get_gateway_context() -> ProductionGatewayContext:
    global _GLOBAL_CONTEXT
    if _GLOBAL_CONTEXT is None:
        _GLOBAL_CONTEXT = ProductionGatewayContext()
    return _GLOBAL_CONTEXT


class GatewayHTTPHandler(BaseHTTPRequestHandler):
    def do_OPTIONS(self) -> None:
        self.send_response(200)
        self._send_cors_headers()
        self.end_headers()

    def do_GET(self) -> None:
        start_time = time.time()
        ctx = get_gateway_context()
        ctx.metrics["requests_total"] += 1

        parsed = urlparse(self.path)
        path = parsed.path
        req_id = f"req-{uuid.uuid4().hex[:8]}"

        # Health Checks & Monitoring
        if path == "/healthz":
            self._send_json(200, {"status": "healthy", "timestamp": time.time(), "platform": "omni-mcp"})
            return

        elif path == "/readyz":
            server_count = len(ctx.cache.list_servers())
            self._send_json(200, {
                "status": "ready",
                "cache_servers": server_count,
                "db_connected": True,
                "timestamp": time.time(),
            })
            return

        elif path == "/metrics":
            uptime = time.time() - ctx.metrics["start_time"]
            total_servers = len(ctx.cache.list_servers())
            total_tools = sum(len(s.tools) for s in ctx.cache.list_servers())
            metrics_payload = (
                f"# HELP omni_uptime_seconds Total runtime in seconds\n"
                f"# TYPE omni_uptime_seconds gauge\n"
                f"omni_uptime_seconds {uptime:.2f}\n"
                f"# HELP omni_requests_total Total HTTP requests received\n"
                f"# TYPE omni_requests_total counter\n"
                f"omni_requests_total {ctx.metrics['requests_total']}\n"
                f"# HELP omni_errors_total Total errors encountered\n"
                f"# TYPE omni_errors_total counter\n"
                f"omni_errors_total {ctx.metrics['errors_total']}\n"
                f"# HELP omni_rate_limit_hits_total Rate limit throttle count\n"
                f"# TYPE omni_rate_limit_hits_total counter\n"
                f"omni_rate_limit_hits_total {ctx.metrics['rate_limit_hits_total']}\n"
                f"# HELP omni_servers_total Active virtual MCP servers\n"
                f"# TYPE omni_servers_total gauge\n"
                f"omni_servers_total {total_servers}\n"
                f"# HELP omni_tools_total Registered tools across all servers\n"
                f"# TYPE omni_tools_total gauge\n"
                f"omni_tools_total {total_tools}\n"
            )
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
            self._send_cors_headers()
            self.end_headers()
            self.wfile.write(metrics_payload.encode("utf-8"))
            return

        elif path == "/api/stats":
            servers = ctx.registry.list_servers()
            total_tools = sum(len(s.tools) for s in servers)
            enabled_tools = sum(
                sum(1 for t in s.tools.values() if t.status == ToolStatus.ENABLED)
                for s in servers
            )
            self._send_json(200, {
                "total_servers": len(servers),
                "total_tools": total_tools,
                "enabled_tools": enabled_tools,
                "disabled_tools": total_tools - enabled_tools,
                "platform": "omni-mcp",
                "version": "1.0.0",
            })
            return

        elif path == "/api/servers":
            servers = ctx.registry.list_servers()
            out = []
            for s in servers:
                out.append({
                    "name": s.name,
                    "displayName": s.display_name,
                    "description": s.description,
                    "owningTeam": s.owning_team,
                    "protocol": s.protocol.value,
                    "status": s.status.value,
                    "version": s.version,
                    "toolsCount": len(s.tools),
                    "enabledToolsCount": sum(1 for t in s.tools.values() if t.status == ToolStatus.ENABLED),
                })
            self._send_json(200, {"servers": out})
            return

        elif path.startswith("/api/servers/"):
            server_name = path.replace("/api/servers/", "").strip()
            server = ctx.registry.get_server(server_name)
            if not server:
                self._send_json(404, {"error": f"Server '{server_name}' not found"})
                return
            tools_out = {}
            for t_name, t in server.tools.items():
                tools_out[t_name] = {
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema,
                    "status": t.status.value,
                    "version": t.version,
                    "protocol": t.protocol.value,
                    "downstreamEndpoint": t.downstream_endpoint,
                }
            self._send_json(200, {
                "name": server.name,
                "displayName": server.display_name,
                "description": server.description,
                "owningTeam": server.owning_team,
                "protocol": server.protocol.value,
                "status": server.status.value,
                "version": server.version,
                "tools": tools_out,
                "history": server.history,
            })
            return

        elif path == "/api/diffs":
            diffs = ctx.registry.list_pending_diffs()
            out = []
            for d in diffs:
                out.append({
                    "diffId": d.diff_id,
                    "serverName": d.server_name,
                    "toolName": d.tool_name,
                    "prevVersion": d.previous_version,
                    "proposedVersion": d.proposed_version,
                    "changes": d.changes_summary,
                    "proposedBy": d.proposed_by,
                    "createdAt": d.created_at,
                    "beforeState": d.before_state,
                    "afterState": d.after_state,
                })
            self._send_json(200, {"diffs": out})
            return

        # Fallback to static UI file serving
        self._serve_static_or_index(path)

    def do_POST(self) -> None:
        start_time = time.time()
        ctx = get_gateway_context()
        ctx.metrics["requests_total"] += 1
        req_id = f"req-{uuid.uuid4().hex[:8]}"

        parsed = urlparse(self.path)
        path = parsed.path

        # Rate Limiting check
        token = self.headers.get("Authorization", "").replace("Bearer ", "").strip() or "anonymous"
        actor = self.headers.get("X-Caller-Actor", "agent")
        rate_key = f"{token}:{actor}"

        allowed, rem_tokens = ctx.rate_limiter.check_rate_limit(rate_key)
        if not allowed:
            ctx.metrics["rate_limit_hits_total"] += 1
            self.send_response(429)
            self.send_header("Content-Type", "application/json")
            self.send_header("Retry-After", "5")
            self._send_cors_headers()
            self.end_headers()
            self.wfile.write(json.dumps({
                "jsonrpc": "2.0",
                "error": {"code": -32029, "message": "Rate limit exceeded. Too many requests."},
            }).encode("utf-8"))
            return

        body = self._read_json_body()

        # Dynamic Virtual Server MCP Route: /<service-name>/mcp
        if path.endswith("/mcp") and path != "/omni/mcp":
            parts = [p for p in path.split("/") if p]
            if len(parts) == 2 and parts[1] == "mcp":
                service_name = parts[0]
                try:
                    req = RPCRequest.from_dict(body)
                except Exception as ex:
                    self._send_json(400, {"jsonrpc": "2.0", "error": {"code": -32700, "message": str(ex)}})
                    return

                resp = ctx.gateway.handle_request(service_name, req, caller_actor=actor, user_token=token)
                self._send_json(200, resp.to_dict())
                return

        # Omni MCP Proxy Route: /omni/mcp
        if path == "/omni/mcp":
            try:
                req = RPCRequest.from_dict(body)
            except Exception as ex:
                self._send_json(400, {"jsonrpc": "2.0", "error": {"code": -32700, "message": str(ex)}})
                return

            resp = ctx.omni_mcp.handle_request(req, caller_actor=actor, user_token=token)
            self._send_json(200, resp.to_dict())
            return

        # Control Plane Management APIs
        if path == "/api/crawler/trigger":
            res = ctx.crawler.run_discovery_workflow()
            ctx.cache.refresh_now()
            self._send_json(200, res)
            return

        elif path == "/api/tools/toggle_status":
            s_name = body.get("serverName")
            t_name = body.get("toolName")
            status_str = body.get("status", "ENABLED")
            new_status = ToolStatus(status_str)
            ok = ctx.registry.toggle_tool_status(s_name, t_name, new_status)
            ctx.cache.refresh_now()
            self._send_json(200, {"success": ok})
            return

        elif path == "/api/servers/toggle_status":
            s_name = body.get("serverName")
            status_str = body.get("status", "ENABLED")
            new_status = ToolStatus(status_str)
            ok = ctx.registry.toggle_server_status(s_name, new_status)
            ctx.cache.refresh_now()
            self._send_json(200, {"success": ok})
            return

        elif path == "/api/tools/propose_update":
            s_name = body.get("serverName")
            t_name = body.get("toolName")
            new_desc = body.get("description")
            new_schema = body.get("inputSchema")
            author = body.get("author", "engineer")
            diff = ctx.registry.propose_tool_update(s_name, t_name, new_desc, new_schema, author)
            self._send_json(200, {
                "diffId": diff.diff_id,
                "changes": diff.changes_summary,
                "proposedVersion": diff.proposed_version,
            })
            return

        elif path == "/api/diffs/approve":
            diff_id = body.get("diffId")
            approver = body.get("approver", "service_owner")
            ok = ctx.registry.approve_diff(diff_id, approver)
            ctx.cache.refresh_now()
            self._send_json(200, {"success": ok})
            return

        elif path == "/api/tools/rollback":
            s_name = body.get("serverName")
            t_name = body.get("toolName")
            target_v = int(body.get("targetVersion", 1))
            ok = ctx.registry.rollback_tool(s_name, t_name, target_v)
            ctx.cache.refresh_now()
            self._send_json(200, {"success": ok})
            return

        self._send_json(404, {"error": "Endpoint not found"})

    def _read_json_body(self) -> Dict[str, Any]:
        length = int(self.headers.get("Content-Length", 0))
        max_bytes = get_gateway_context().config.max_body_size_bytes
        if length > max_bytes:
            raise ValueError(f"Payload size {length} bytes exceeds maximum allowed limit {max_bytes} bytes")
        if length > 0:
            raw = self.rfile.read(length).decode("utf-8")
            try:
                return json.loads(raw)
            except Exception:
                return {}
        return {}

    def _send_json(self, status: int, data: Any) -> None:
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self._send_cors_headers()
        self.end_headers()
        self.wfile.write(json.dumps(data).encode("utf-8"))

    def _send_cors_headers(self) -> None:
        cfg = get_gateway_context().config
        self.send_header("Access-Control-Allow-Origin", cfg.cors_allowed_origins)
        self.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "Content-Type, Authorization, X-Caller-Actor")

    def _serve_static_or_index(self, path: str) -> None:
        ui_dist = os.path.join(os.path.dirname(__file__), "..", "ui", "dist")
        if not os.path.exists(ui_dist):
            self._serve_embedded_html()
            return

        file_path = os.path.join(ui_dist, path.lstrip("/"))
        if os.path.isfile(file_path):
            self.send_response(200)
            if file_path.endswith(".js"):
                self.send_header("Content-Type", "application/javascript")
            elif file_path.endswith(".css"):
                self.send_header("Content-Type", "text/css")
            elif file_path.endswith(".svg"):
                self.send_header("Content-Type", "image/svg+xml")
            elif file_path.endswith(".html"):
                self.send_header("Content-Type", "text/html")
            self._send_cors_headers()
            self.end_headers()
            with open(file_path, "rb") as f:
                self.wfile.write(f.read())
        else:
            index_path = os.path.join(ui_dist, "index.html")
            if os.path.exists(index_path):
                self.send_response(200)
                self.send_header("Content-Type", "text/html")
                self._send_cors_headers()
                self.end_headers()
                with open(index_path, "rb") as f:
                    self.wfile.write(f.read())
            else:
                self._serve_embedded_html()

    def _serve_embedded_html(self) -> None:
        html = """<!DOCTYPE html>
<html>
<head>
    <title>Omni MCP - Enterprise Platform</title>
    <style>
        body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; background: #0b0b0f; color: #f3f4f6; margin: 0; padding: 24px; }
        header { border-bottom: 1px solid #222; padding-bottom: 16px; margin-bottom: 24px; display: flex; justify-content: space-between; align-items: center; }
        h1 { margin: 0; font-size: 22px; font-weight: 700; }
        .badge { background: #1b382b; color: #00e575; border-radius: 4px; padding: 4px 10px; font-size: 13px; font-weight: 600; }
        .card { background: #13141e; border: 1px solid #212333; border-radius: 8px; padding: 18px; margin-bottom: 16px; }
        .grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(320px, 1fr)); gap: 16px; }
        pre { background: #07070a; border: 1px solid #1a1c27; padding: 12px; border-radius: 6px; color: #00e575; overflow-x: auto; }
        a { color: #276ef1; text-decoration: none; }
    </style>
</head>
<body>
    <header>
        <div>
            <h1>Omni MCP Platform</h1>
            <p style="color: #888; margin: 4px 0 0 0;">Enterprise MCP Gateway & Runtime Registry</p>
        </div>
        <div class="badge">● Online</div>
    </header>
    <div class="card">
        <h3>Platform Overview</h3>
        <p>Production server active. Endpoints available:</p>
        <ul>
            <li>Dynamic Virtual MCP Servers: <code>/<service-name>/mcp</code></li>
            <li>Omni MCP Incremental Discovery Proxy: <code>/omni/mcp</code></li>
            <li>Health & Readiness: <code>/healthz</code>, <code>/readyz</code>, <code>/metrics</code></li>
            <li>Control Plane APIs: <code>/api/servers</code>, <code>/api/stats</code>, <code>/api/diffs</code></li>
        </ul>
    </div>
    <div id="content" class="grid"></div>
    <script>
        fetch('/api/servers').then(r => r.json()).then(data => {
            const container = document.getElementById('content');
            data.servers.forEach(s => {
                const div = document.createElement('div');
                div.className = 'card';
                div.innerHTML = `
                    <div style="display:flex; justify-content:space-between;">
                        <h4 style="margin:0;">${s.displayName}</h4>
                        <span style="font-size:12px; color:#888;">${s.protocol}</span>
                    </div>
                    <p style="color:#aaa; font-size:14px;">${s.description}</p>
                    <div style="font-size:13px; color:#666;">
                        Tools: <strong>${s.enabledToolsCount} / ${s.toolsCount}</strong> enabled
                    </div>
                `;
                container.appendChild(div);
            });
        });
    </script>
</body>
</html>"""
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self._send_cors_headers()
        self.end_headers()
        self.wfile.write(html.encode("utf-8"))


def run_server(host: str = "0.0.0.0", port: int = 8080) -> None:
    ctx = get_gateway_context()
    server = ThreadedHTTPServer((host, port), GatewayHTTPHandler)
    print(f"[*] Omni MCP Gateway running at http://{host}:{port}")
    print(f"[*] Liveness probe: http://{host}:{port}/healthz")
    print(f"[*] Prometheus metrics: http://{host}:{port}/metrics")
    print(f"[*] Omni MCP proxy available at http://{host}:{port}/omni/mcp")
    print(f"[*] Dynamic virtual servers available at http://{host}:{port}/<service>/mcp")

    def shutdown_handler(signum, frame):
        print("\n[*] Gracefully stopping Omni MCP Gateway...")
        server.server_close()
        sys.exit(0)

    try:
        signal.signal(signal.SIGINT, shutdown_handler)
        signal.signal(signal.SIGTERM, shutdown_handler)
    except Exception:
        pass

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


def main() -> int:
    cfg = get_config()
    host = cfg.host
    port = cfg.port
    if len(sys.argv) > 1 and sys.argv[1].isdigit():
        port = int(sys.argv[1])
    run_server(host=host, port=port)
    return 0


if __name__ == "__main__":
    sys.exit(main())
