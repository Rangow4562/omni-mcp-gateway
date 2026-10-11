"""
Native MCPFx Heartbeat Monitor - Detects native MCP servers emitting heartbeat signals and discovers tools.
"""
from __future__ import annotations
import time
from typing import Dict, Any, List, Optional, Callable
from omni_mcp.models.entities import (
    ServerRecord,
    ToolRecord,
    ToolStatus,
    ProtocolType,
)


class MCPFxHeartbeatMonitor:
    """Monitors MCPFx heartbeat signals from native MCP servers and performs listTools discovery."""

    def __init__(self, list_tools_client: Optional[Callable[[str], List[Dict[str, Any]]]] = None) -> None:
        self._heartbeats: Dict[str, Dict[str, Any]] = {}
        self._list_tools_client = list_tools_client or self._default_mock_list_tools

    def emit_heartbeat(self, server_name: str, host: str, port: int, owning_team: str, display_name: str = "") -> None:
        """Called by native MCPFx servers to broadcast readiness."""
        self._heartbeats[server_name] = {
            "server_name": server_name,
            "display_name": display_name or server_name,
            "host": host,
            "port": port,
            "owning_team": owning_team,
            "timestamp": time.time(),
        }

    def scan_active_native_servers(self, max_stale_seconds: float = 60.0) -> List[ServerRecord]:
        """Scans heartbeats, calls listTools on newly discovered servers, and returns virtual proxy server records."""
        now = time.time()
        discovered_servers: List[ServerRecord] = []

        for name, meta in list(self._heartbeats.items()):
            if now - meta["timestamp"] > max_stale_seconds:
                continue  # Stale heartbeat

            # Make listTools call to discover explicitly exposed native tools
            raw_tools = self._list_tools_client(name)
            tools_dict: Dict[str, ToolRecord] = {}

            for rt in raw_tools:
                t_name = rt["name"]
                t_desc = rt.get("description", f"Native MCP tool {t_name}")
                t_schema = rt.get("inputSchema", {"type": "object", "properties": {}})

                tools_dict[t_name] = ToolRecord(
                    name=t_name,
                    description=t_desc,
                    input_schema=t_schema,
                    status=ToolStatus.DISABLED,  # Disabled by default
                    version=1,
                    protocol=ProtocolType.NATIVE_MCP,
                    downstream_endpoint=f"http://{meta['host']}:{meta['port']}/mcp",
                    downstream_method=t_name,
                )

            server_record = ServerRecord(
                name=name,
                display_name=meta["display_name"],
                description=f"Native MCP server on {meta['host']}:{meta['port']} (discovered via MCPFx heartbeat)",
                owning_team=meta["owning_team"],
                protocol=ProtocolType.NATIVE_MCP,
                status=ToolStatus.DISABLED,  # Discovery does not imply exposure
                version=1,
                tools=tools_dict,
                heartbeat_timestamp=meta["timestamp"],
                is_native=True,
            )
            discovered_servers.append(server_record)

        return discovered_servers

    def _default_mock_list_tools(self, server_name: str) -> List[Dict[str, Any]]:
        """Mock listTools discovery response."""
        return [
            {
                "name": f"{server_name}_query",
                "description": f"Native high-throughput query on {server_name}",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Search or filter query"}
                    },
                    "required": ["query"]
                }
            }
        ]

