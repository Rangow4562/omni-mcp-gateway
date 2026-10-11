"""
JSON-RPC 2.0 and Model Context Protocol (MCP) data models and error primitives.
"""
from __future__ import annotations
from dataclasses import dataclass, field
from typing import Any, Dict, Optional, Union

JSONRPC_VERSION = "2.0"

# Standard JSON-RPC 2.0 Error Codes
PARSE_ERROR = -32700
INVALID_REQUEST = -32600
METHOD_NOT_FOUND = -32601
INVALID_PARAMS = -32602
INTERNAL_ERROR = -32603

# MCP specific error codes
UNAUTHORIZED = -32001
TOOL_DISABLED = -32002
DOWNSTREAM_ERROR = -32003


@dataclass
class RPCError:
    code: int
    message: str
    data: Optional[Any] = None

    def to_dict(self) -> Dict[str, Any]:
        result = {"code": self.code, "message": self.message}
        if self.data is not None:
            result["data"] = self.data
        return result


@dataclass
class RPCRequest:
    method: str
    params: Optional[Dict[str, Any]] = field(default_factory=dict)
    id: Optional[Union[str, int]] = None
    jsonrpc: str = JSONRPC_VERSION

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> RPCRequest:
        if not isinstance(data, dict):
            raise ValueError("Request must be a JSON object")
        return cls(
            method=data.get("method", ""),
            params=data.get("params") or {},
            id=data.get("id"),
            jsonrpc=data.get("jsonrpc", JSONRPC_VERSION),
        )


@dataclass
class RPCResponse:
    id: Optional[Union[str, int]] = None
    result: Optional[Any] = None
    error: Optional[RPCError] = None
    jsonrpc: str = JSONRPC_VERSION

    def to_dict(self) -> Dict[str, Any]:
        out: Dict[str, Any] = {"jsonrpc": self.jsonrpc, "id": self.id}
        if self.error is not None:
            out["error"] = self.error.to_dict()
        else:
            out["result"] = self.result
        return out

