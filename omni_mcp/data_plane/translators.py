"""
Protocol Translators - Converts MCP JSON-RPC payloads to HTTP, gRPC (Protobuf), and TChannel (Thrift) wire formats.
"""
from __future__ import annotations
import json
from typing import Dict, Any, Optional
from omni_mcp.models.entities import ProtocolType, ToolRecord
from omni_mcp.data_plane.muttley import MuttleyMeshClient
from omni_mcp.control_plane.third_party import ThirdPartyAuthExchange


class ProtocolTranslator:
    """Translates incoming JSON payloads into wire formats and executes via Muttley mesh sidecar."""

    def __init__(
        self,
        mesh_client: Optional[MuttleyMeshClient] = None,
        auth_exchange: Optional[ThirdPartyAuthExchange] = None,
    ) -> None:
        self.mesh_client = mesh_client or MuttleyMeshClient()
        self.auth_exchange = auth_exchange or ThirdPartyAuthExchange()

    def execute_tool_call(
        self,
        server_name: str,
        tool: ToolRecord,
        arguments: Dict[str, Any],
        user_token: Optional[str] = None,
        headers: Optional[Dict[str, str]] = None,
    ) -> Dict[str, Any]:
        """
        1. Translates incoming JSON payload into appropriate wire format.
        2. Serializes request into Protobuf, Thrift, or HTTP payload.
        3. Forwards request to downstream service via Muttley sidecar.
        4. Translates wire response bytes back to MCP JSON.
        """
        headers = dict(headers or {})
        protocol = tool.protocol

        # Handle 3P Token Exchange
        if protocol == ProtocolType.THIRD_PARTY:
            if user_token:
                provider = tool.metadata.get("provider", "jira")
                oauth_token = self.auth_exchange.exchange_token(user_token, provider)
                if oauth_token:
                    headers["Authorization"] = f"Bearer {oauth_token}"
            wire_payload = json.dumps(arguments).encode("utf-8")

        elif protocol == ProtocolType.GRPC:
            # gRPC / Protobuf translation: serialize JSON args into simulated Protobuf frame
            wire_payload = self._serialize_protobuf(tool.name, arguments)
            headers["content-type"] = "application/grpc+proto"

        elif protocol == ProtocolType.TCHANNEL:
            # TChannel / Thrift translation: serialize into simulated Thrift frame
            wire_payload = self._serialize_thrift(tool.name, arguments)
            headers["as"] = "thrift"

        elif protocol == ProtocolType.NATIVE_MCP:
            # Native MCP Server: transparent proxying of native MCP request
            wire_payload = json.dumps({"method": "tools/call", "params": {"name": tool.name, "arguments": arguments}}).encode("utf-8")

        else:
            # Default HTTP REST
            wire_payload = json.dumps(arguments).encode("utf-8")
            headers["content-type"] = "application/json"

        # Forward via Muttley
        resp_bytes = self.mesh_client.execute_downstream(
            service_name=server_name,
            method_name=tool.downstream_method or tool.name,
            protocol=protocol,
            wire_payload=wire_payload,
            headers=headers,
        )

        # Deserialize back to JSON
        return json.loads(resp_bytes.decode("utf-8"))

    def _serialize_protobuf(self, method: str, args: Dict[str, Any]) -> bytes:
        # Protobuf binary envelope simulation
        header = b"\x00\x00\x00\x00" + bytes([len(method)]) + method.encode("utf-8")
        body = json.dumps(args).encode("utf-8")
        return header + body

    def _serialize_thrift(self, method: str, args: Dict[str, Any]) -> bytes:
        # Thrift TBinaryProtocol envelope simulation
        header = b"\x80\x01\x00\x01" + bytes([len(method)]) + method.encode("utf-8")
        body = json.dumps(args).encode("utf-8")
        return header + body

