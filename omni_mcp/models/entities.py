"""
Domain entities representing servers, tools, policies, diffs, and governance states.
"""
from __future__ import annotations
from dataclasses import dataclass, field
from enum import Enum
from typing import Dict, Any, List, Optional
import time


class ProtocolType(str, Enum):
    HTTP = "HTTP"
    GRPC = "gRPC"
    TCHANNEL = "TChannel"
    NATIVE_MCP = "Native_MCP"
    THIRD_PARTY = "ThirdParty"


class ToolStatus(str, Enum):
    DISABLED = "DISABLED"          # Default state: "discovery doesn't imply exposure"
    PENDING_REVIEW = "PENDING_REVIEW"
    ENABLED = "ENABLED"            # Explicitly approved and deployed by owning team


class ActorType(str, Enum):
    HUMAN = "human"
    SERVICE = "service"
    AGENT = "agent"


@dataclass
class CharterPolicy:
    """Attribute-Based Access Control (ABAC) charter policy configured on caller actors."""
    allowed_actors: List[ActorType] = field(default_factory=lambda: [ActorType.HUMAN, ActorType.SERVICE, ActorType.AGENT])
    allowed_tools: List[str] = field(default_factory=lambda: ["*"])
    rate_limit_rpm: int = 1200
    require_redaction: bool = True
    custom_attributes: Dict[str, Any] = field(default_factory=dict)

    def is_actor_allowed(self, actor: str) -> bool:
        try:
            actor_enum = ActorType(actor.lower())
            return actor_enum in self.allowed_actors
        except ValueError:
            return False

    def is_tool_allowed(self, tool_name: str) -> bool:
        if "*" in self.allowed_tools:
            return True
        return tool_name in self.allowed_tools


@dataclass
class ToolRecord:
    name: str
    description: str
    input_schema: Dict[str, Any]
    output_schema: Dict[str, Any] = field(default_factory=lambda: {"type": "object"})
    status: ToolStatus = ToolStatus.DISABLED
    version: int = 1
    protocol: ProtocolType = ProtocolType.HTTP
    downstream_endpoint: str = ""
    downstream_method: str = ""
    charter_policy: Optional[CharterPolicy] = None
    metadata: Dict[str, Any] = field(default_factory=dict)

    def to_mcp_spec(self, enable_projection: bool = True) -> Dict[str, Any]:
        """Convert to standard Model Context Protocol tool description."""
        schema = dict(self.input_schema)
        if enable_projection:
            # Inject Response Projection field for token optimization
            props = dict(schema.get("properties", {}))
            props["_projection"] = {
                "type": "array",
                "items": {"type": "string"},
                "description": "GraphQL-like nested field projections. Specify only fields needed to conserve context tokens.",
            }
            schema["properties"] = props

        return {
            "name": self.name,
            "description": self.description,
            "inputSchema": schema,
        }


@dataclass
class ConfigDiff:
    diff_id: str
    server_name: str
    tool_name: str
    previous_version: int
    proposed_version: int
    before_state: Dict[str, Any]
    after_state: Dict[str, Any]
    changes_summary: List[str]
    proposed_by: str
    created_at: float = field(default_factory=time.time)
    approved: bool = False
    approved_by: Optional[str] = None
    approved_at: Optional[float] = None


@dataclass
class ServerRecord:
    name: str
    display_name: str
    description: str
    owning_team: str
    protocol: ProtocolType = ProtocolType.HTTP
    status: ToolStatus = ToolStatus.DISABLED
    version: int = 1
    tools: Dict[str, ToolRecord] = field(default_factory=dict)
    charter_policy: CharterPolicy = field(default_factory=CharterPolicy)
    heartbeat_timestamp: Optional[float] = None
    is_native: bool = False
    is_third_party: bool = False
    third_party_provider: Optional[str] = None  # e.g., 'jira', 'google'
    history: List[Dict[str, Any]] = field(default_factory=list)

    def get_tool(self, tool_name: str) -> Optional[ToolRecord]:
        return self.tools.get(tool_name)

    def add_tool(self, tool: ToolRecord) -> None:
        self.tools[tool.name] = tool

