"""
Omni MCP Proxy Server - Centralized incremental discovery proxy for hundreds of servers.
Exposes:
- discover_server
- discover_tools
- get_tool_schema
- invoke_tool
"""
from __future__ import annotations
import json
import re
from typing import Dict, Any, List, Optional
from omni_mcp.models.entities import ToolStatus
from omni_mcp.models.rpc import (
    RPCRequest,
    RPCResponse,
    RPCError,
    INVALID_PARAMS,
    METHOD_NOT_FOUND,
)
from omni_mcp.data_plane.cache import InMemoryConfigCache
from omni_mcp.data_plane.gateway import MCPProxyGateway


class OmniMCPServer:
    """Single proxy server enabling token-efficient incremental discovery across the enterprise."""

    OMNI_TOOLS = [
        {
            "name": "discover_server",
            "description": "Discover MCP servers based on query intent or keyword search.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Natural language query or intent (e.g. 'driver tracking', 'eats merchant', 'rider payments')",
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max servers to return (default: 5)",
                    },
                },
                "required": ["query"],
            },
        },
        {
            "name": "discover_tools",
            "description": "Lookup enabled tools for an identified MCP server.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "server_name": {
                        "type": "string",
                        "description": "Name of the target MCP server",
                    }
                },
                "required": ["server_name"],
            },
        },
        {
            "name": "get_tool_schema",
            "description": "Get the precise JSON input schema and parameter requirements for a tool.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "server_name": {"type": "string"},
                    "tool_name": {"type": "string"},
                },
                "required": ["server_name", "tool_name"],
            },
        },
        {
            "name": "invoke_tool",
            "description": "Invoke a discovered MCP tool through the gateway with optional field projection.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "server_name": {"type": "string"},
                    "tool_name": {"type": "string"},
                    "arguments": {
                        "type": "object",
                        "description": "Tool parameters matching the tool schema",
                    },
                    "_projection": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Optional field projections to return only required paths (e.g. ['driver.name', 'rating'])",
                    },
                },
                "required": ["server_name", "tool_name", "arguments"],
            },
        },
    ]

    def __init__(self, cache: InMemoryConfigCache, gateway: MCPProxyGateway) -> None:
        self.cache = cache
        self.gateway = gateway

    def handle_request(
        self,
        request: RPCRequest,
        caller_actor: str = "agent",
        user_token: Optional[str] = None,
    ) -> RPCResponse:
        """Standard MCP protocol handler for Omni MCP."""
        method = request.method
        params = request.params or {}

        if method == "initialize":
            return RPCResponse(
                id=request.id,
                result={
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {
                        "name": "uber-omni-mcp-gateway",
                        "version": "1.0.0",
                        "description": "Uber Omni MCP incremental discovery and routing proxy",
                    },
                },
            )

        elif method == "tools/list":
            return RPCResponse(id=request.id, result={"tools": self.OMNI_TOOLS})

        elif method == "tools/call":
            tool_name = params.get("name")
            arguments = params.get("arguments") or {}

            if tool_name == "discover_server":
                return self._call_discover_server(request, arguments)
            elif tool_name == "discover_tools":
                return self._call_discover_tools(request, arguments)
            elif tool_name == "get_tool_schema":
                return self._call_get_tool_schema(request, arguments)
            elif tool_name == "invoke_tool":
                return self._call_invoke_tool(request, arguments, caller_actor, user_token)
            else:
                return RPCResponse(
                    id=request.id,
                    error=RPCError(code=METHOD_NOT_FOUND, message=f"Unknown Omni MCP tool: {tool_name}"),
                )

        return RPCResponse(
            id=request.id,
            error=RPCError(code=METHOD_NOT_FOUND, message=f"Method '{method}' not recognized"),
        )

    def _call_discover_server(self, request: RPCRequest, args: Dict[str, Any]) -> RPCResponse:
        query = args.get("query", "").lower()
        limit = args.get("limit", 5)

        servers = self.cache.list_servers()
        scored: List[Tuple[float, Dict[str, Any]]] = []

        tokens = re.findall(r"\w+", query)
        for s in servers:
            score = 0.0
            text = f"{s.name} {s.display_name} {s.description} {' '.join(s.tools.keys())}".lower()
            for token in tokens:
                if token in text:
                    score += 1.0
            if score > 0 or not tokens:
                scored.append((
                    score,
                    {
                        "server_name": s.name,
                        "display_name": s.display_name,
                        "description": s.description,
                        "protocol": s.protocol.value,
                        "status": s.status.value,
                        "enabled_tools_count": sum(1 for t in s.tools.values() if t.status == ToolStatus.ENABLED),
                        "total_tools_count": len(s.tools),
                    }
                ))

        scored.sort(key=lambda x: x[0], reverse=True)
        results = [item[1] for item in scored[:limit]]

        return RPCResponse(
            id=request.id,
            result={
                "content": [
                    {
                        "type": "text",
                        "text": json.dumps({"servers": results, "query": query, "total_matches": len(results)}, indent=2),
                    }
                ]
            },
        )

    def _call_discover_tools(self, request: RPCRequest, args: Dict[str, Any]) -> RPCResponse:
        server_name = args.get("server_name")
        if not server_name:
            return RPCResponse(id=request.id, error=RPCError(code=INVALID_PARAMS, message="Missing 'server_name'"))

        server = self.cache.get_server(server_name)
        if not server:
            return RPCResponse(
                id=request.id,
                error=RPCError(code=INVALID_PARAMS, message=f"Server '{server_name}' not found"),
            )

        tools_summary = []
        for t in server.tools.values():
            if t.status == ToolStatus.ENABLED:
                tools_summary.append({
                    "name": t.name,
                    "description": t.description,
                    "status": t.status.value,
                    "protocol": t.protocol.value,
                })

        return RPCResponse(
            id=request.id,
            result={
                "content": [
                    {
                        "type": "text",
                        "text": json.dumps({
                            "server_name": server_name,
                            "tools": tools_summary,
                            "count": len(tools_summary),
                        }, indent=2),
                    }
                ]
            },
        )

    def _call_get_tool_schema(self, request: RPCRequest, args: Dict[str, Any]) -> RPCResponse:
        server_name = args.get("server_name")
        tool_name = args.get("tool_name")
        if not server_name or not tool_name:
            return RPCResponse(
                id=request.id,
                error=RPCError(code=INVALID_PARAMS, message="'server_name' and 'tool_name' are required"),
            )

        server = self.cache.get_server(server_name)
        if not server:
            return RPCResponse(id=request.id, error=RPCError(code=INVALID_PARAMS, message=f"Server '{server_name}' not found"))

        tool = server.tools.get(tool_name)
        if not tool:
            return RPCResponse(id=request.id, error=RPCError(code=INVALID_PARAMS, message=f"Tool '{tool_name}' not found"))

        return RPCResponse(
            id=request.id,
            result={
                "content": [
                    {
                        "type": "text",
                        "text": json.dumps(tool.to_mcp_spec(enable_projection=True), indent=2),
                    }
                ]
            },
        )

    def _call_invoke_tool(
        self,
        request: RPCRequest,
        args: Dict[str, Any],
        caller_actor: str,
        user_token: Optional[str],
    ) -> RPCResponse:
        server_name = args.get("server_name")
        tool_name = args.get("tool_name")
        tool_args = args.get("arguments") or {}
        projection = args.get("_projection")

        if not server_name or not tool_name:
            return RPCResponse(
                id=request.id,
                error=RPCError(code=INVALID_PARAMS, message="'server_name' and 'tool_name' are required"),
            )

        if projection:
            tool_args["_projection"] = projection

        # Delegate execution directly to the virtual server handler on the gateway
        call_req = RPCRequest(
            id=request.id,
            method="tools/call",
            params={"name": tool_name, "arguments": tool_args},
        )
        return self.gateway.handle_request(server_name, call_req, caller_actor, user_token)

