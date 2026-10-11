"""
AutoCrawler - Cadence-powered distributed workflow engine for discovering APIs and native servers.
"""
from __future__ import annotations
import time
from typing import Dict, List, Optional
from omni_mcp.models.entities import (
    ServerRecord,
    ToolRecord,
    ToolStatus,
    ProtocolType,
)
from omni_mcp.control_plane.registry import MCPRegistry
from omni_mcp.control_plane.idl_parser import IDLParser, ServiceDefinition
from omni_mcp.control_plane.description_gen import ToolDescriptionGenerator
from omni_mcp.control_plane.native_monitor import MCPFxHeartbeatMonitor


class AutoCrawler:
    """Distributed discovery workflow engine mimicking Cadence cron execution."""

    def __init__(
        self,
        registry: MCPRegistry,
        native_monitor: Optional[MCPFxHeartbeatMonitor] = None,
        description_gen: Optional[ToolDescriptionGenerator] = None,
    ) -> None:
        self.registry = registry
        self.native_monitor = native_monitor or MCPFxHeartbeatMonitor()
        self.description_gen = description_gen or ToolDescriptionGenerator()
        self._idl_registry_store: Dict[str, str] = {}  # service_name -> idl_content
        self._last_run_timestamp: Optional[float] = None
        self._run_count: int = 0

    def register_idl_source(self, service_name: str, idl_content: str) -> None:
        """Register an IDL file in Uber's simulated IDL registry."""
        self._idl_registry_store[service_name] = idl_content

    def run_discovery_workflow(self) -> Dict[str, Any]:
        """
        Executes a Cadence workflow cycle:
        1. Scans IDL registry for Protobuf/Thrift definitions.
        2. Parses IDLs and generates LLM-enriched tool descriptions.
        3. Upserts virtual MCP servers and tools in DISABLED-by-default state.
        4. Scans native MCPFx heartbeats and upserts virtual proxy servers in DISABLED state.
        """
        self._run_count += 1
        self._last_run_timestamp = time.time()
        discovered_servers = 0
        discovered_tools = 0

        # 1. Discover IDL-Backed Services
        for svc_name, content in self._idl_registry_store.items():
            parsed_services: List[ServiceDefinition] = []
            if "syntax = \"proto3\"" in content or "service " in content and "rpc " in content:
                parsed_services = IDLParser.parse_protobuf(content)
            elif "namespace " in content or "service " in content:
                parsed_services = IDLParser.parse_thrift(content)

            for svc_def in parsed_services:
                virtual_server_name = svc_def.service_name.lower()
                server_record = self.registry.get_server(virtual_server_name)
                if not server_record:
                    server_record = ServerRecord(
                        name=virtual_server_name,
                        display_name=svc_def.service_name,
                        description=svc_def.doc_comment or f"Virtual MCP Server for {svc_def.service_name}",
                        owning_team=f"team-{virtual_server_name}",
                        protocol=svc_def.protocol,
                        status=ToolStatus.DISABLED,  # Disabled by default
                        version=1,
                    )
                    self.registry.register_server(server_record)
                    discovered_servers += 1

                for method in svc_def.methods:
                    tool_name = method.name
                    if tool_name not in server_record.tools:
                        input_schema = IDLParser.translate_to_json_schema(method)
                        enriched_desc = self.description_gen.generate_description(
                            svc_def.service_name, method
                        )

                        tool_record = ToolRecord(
                            name=tool_name,
                            description=enriched_desc,
                            input_schema=input_schema,
                            status=ToolStatus.DISABLED,  # Discovery does not imply exposure
                            version=1,
                            protocol=svc_def.protocol,
                            downstream_endpoint=f"muttley://{virtual_server_name}/{method.name}",
                            downstream_method=method.name,
                        )
                        server_record.add_tool(tool_record)
                        discovered_tools += 1

        # 2. Discover Native Servers via MCPFx Heartbeats
        native_servers = self.native_monitor.scan_active_native_servers()
        for ns in native_servers:
            existing = self.registry.get_server(ns.name)
            if not existing:
                self.registry.register_server(ns)
                discovered_servers += 1
                discovered_tools += len(ns.tools)
            else:
                for t_name, t_val in ns.tools.items():
                    if t_name not in existing.tools:
                        existing.add_tool(t_val)
                        discovered_tools += 1

        return {
            "workflow_run_id": f"cadence-run-{self._run_count}",
            "timestamp": self._last_run_timestamp,
            "discovered_servers": discovered_servers,
            "discovered_tools": discovered_tools,
            "total_registered_servers": len(self.registry.list_servers()),
        }

