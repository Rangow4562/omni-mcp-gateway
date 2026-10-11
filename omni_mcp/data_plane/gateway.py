"""
MCP Proxy Gateway - Core runtime service materializing dynamic virtual MCP servers at /<service-name>/mcp.
"""
from __future__ import annotations
import json
from typing import Dict, Any, Optional, Tuple, List
from omni_mcp.models.entities import ToolStatus, ActorType
from omni_mcp.models.rpc import (
    RPCRequest,
    RPCResponse,
    RPCError,
    METHOD_NOT_FOUND,
    INVALID_PARAMS,
    UNAUTHORIZED,
    TOOL_DISABLED,
    INTERNAL_ERROR,
)
from omni_mcp.data_plane.cache import InMemoryConfigCache
from omni_mcp.data_plane.security import SecurityCharterEngine, PIIRedactor
from omni_mcp.data_plane.projection import ResponseProjectionEngine
from omni_mcp.data_plane.translators import ProtocolTranslator


class MCPProxyGateway:
    """Dynamic virtual server host and runtime router."""

    def __init__(
        self,
        cache: InMemoryConfigCache,
        translator: Optional[ProtocolTranslator] = None,
    ) -> None:
        self.cache = cache
        self.translator = translator or ProtocolTranslator()

    def handle_request(
        self,
        service_name: str,
        request: RPCRequest,
        caller_actor: str = "agent",
        user_token: Optional[str] = None,
    ) -> RPCResponse:
        """Processes an incoming JSON-RPC 2.0 request dispatched to /<service-name>/mcp."""
        server = self.cache.get_server(service_name)
        if not server:
            return RPCResponse(
                id=request.id,
                error=RPCError(
                    code=METHOD_NOT_FOUND,
                    message=f"Virtual MCP server '{service_name}' not found or not registered.",
                ),
            )

        method = request.method
        params = request.params or {}

        if method == "initialize":
            return self._handle_initialize(server, request)
        elif method == "notifications/initialized":
            return RPCResponse(id=request.id, result={})
        elif method == "tools/list":
            return self._handle_tools_list(server, request)
        elif method == "tools/call":
            return self._handle_tools_call(server, request, caller_actor, user_token)
        else:
            return RPCResponse(
                id=request.id,
                error=RPCError(code=METHOD_NOT_FOUND, message=f"Method '{method}' not supported."),
            )

    def _handle_initialize(self, server: Any, request: RPCRequest) -> RPCResponse:
        return RPCResponse(
            id=request.id,
            result={
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {"listChanged": True},
                    "logging": {},
                },
                "serverInfo": {
                    "name": f"omni-mcp/{server.name}",
                    "version": str(server.version),
                    "description": server.description,
                },
            },
        )

    def _handle_tools_list(self, server: Any, request: RPCRequest) -> RPCResponse:
        # Only expose tools that are in ENABLED status (discovery doesn't imply exposure)
        tools_list: List[Dict[str, Any]] = []
        for tool in server.tools.values():
            if tool.status == ToolStatus.ENABLED:
                tools_list.append(tool.to_mcp_spec(enable_projection=True))

        return RPCResponse(id=request.id, result={"tools": tools_list})

    def _handle_tools_call(
        self,
        server: Any,
        request: RPCRequest,
        caller_actor: str,
        user_token: Optional[str],
    ) -> RPCResponse:
        params = request.params or {}
        tool_name = params.get("name")
        arguments = params.get("arguments") or {}

        if not tool_name:
            return RPCResponse(
                id=request.id,
                error=RPCError(code=INVALID_PARAMS, message="Missing 'name' in tools/call parameters."),
            )

        tool = server.tools.get(tool_name)
        if not tool:
            return RPCResponse(
                id=request.id,
                error=RPCError(code=METHOD_NOT_FOUND, message=f"Tool '{tool_name}' not found on server '{server.name}'."),
            )

        # 1. Check Tool Status (Must be ENABLED)
        if tool.status != ToolStatus.ENABLED:
            return RPCResponse(
                id=request.id,
                error=RPCError(
                    code=TOOL_DISABLED,
                    message=f"Tool '{tool_name}' is currently {tool.status.value}. Discovery does not imply exposure; owner must enable tool.",
                ),
            )

        # 2. Security Charter & ABAC Policy Check
        is_allowed, reason = SecurityCharterEngine.evaluate_access(
            server=server,
            tool=tool,
            caller_actor=caller_actor,
            user_token=user_token,
        )
        if not is_allowed:
            return RPCResponse(
                id=request.id,
                error=RPCError(code=UNAUTHORIZED, message=reason or "Unauthorized by Charter Policy."),
            )

        # 3. Extract Response Projection paths if provided by agent/caller
        projection_paths = arguments.pop("_projection", None)

        try:
            # 4. Protocol Translation & Downstream Execution via Muttley
            raw_result = self.translator.execute_tool_call(
                server_name=server.name,
                tool=tool,
                arguments=arguments,
                user_token=user_token,
            )

            # 5. Apply Response Projection (GraphQL-like field trimming)
            projected_result = ResponseProjectionEngine.apply_projection(raw_result, projection_paths)

            # 6. Apply PII Redaction
            effective_policy = tool.charter_policy or server.charter_policy
            if effective_policy.require_redaction:
                final_result = PIIRedactor.redact_payload(projected_result)
            else:
                final_result = projected_result

            # Format as standard MCP content
            return RPCResponse(
                id=request.id,
                result={
                    "content": [
                        {
                            "type": "text",
                            "text": json.dumps(final_result, indent=2),
                        }
                    ],
                    "isError": False,
                    "_metadata": {
                        "server": server.name,
                        "tool": tool.name,
                        "projected": bool(projection_paths),
                    }
                },
            )

        except Exception as ex:
            return RPCResponse(
                id=request.id,
                error=RPCError(code=INTERNAL_ERROR, message=f"Execution error downstream: {str(ex)}"),
            )

