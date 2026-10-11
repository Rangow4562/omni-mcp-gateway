"""
MCP Registry - Single source of truth for discovery, ownership, versioning, diff approval, and enablement.
Supports both high-throughput in-memory caching and persistent SQLite storage.
"""
from __future__ import annotations
import copy
import threading
import time
import uuid
from typing import Dict, List, Optional, Any
from omni_mcp.models.entities import (
    ServerRecord,
    ToolRecord,
    ToolStatus,
    ConfigDiff,
)
from omni_mcp.storage.sqlite_store import SQLiteRegistryStore


class MCPRegistry:
    def __init__(self, persistent_store: Optional[SQLiteRegistryStore] = None) -> None:
        self._lock = threading.RLock()
        self._servers: Dict[str, ServerRecord] = {}
        self._diffs: Dict[str, ConfigDiff] = {}
        self._version_history: Dict[str, List[Dict[str, Any]]] = {}
        self._store = persistent_store

        # If persistent store provided, load initial state
        if self._store:
            persisted_servers = self._store.list_all_servers()
            for s in persisted_servers:
                self._servers[s.name] = s
            for d in self._store.list_diffs(only_pending=False):
                self._diffs[d.diff_id] = d

    def register_server(self, server: ServerRecord) -> ServerRecord:
        with self._lock:
            if server.name in self._servers:
                existing = self._servers[server.name]
                for tool_name, tool in server.tools.items():
                    if tool_name not in existing.tools:
                        existing.tools[tool_name] = tool
                existing.heartbeat_timestamp = server.heartbeat_timestamp or existing.heartbeat_timestamp
                target = existing
            else:
                self._servers[server.name] = server
                self._version_history[server.name] = [self._snapshot_server(server)]
                target = server

            if self._store:
                self._store.save_server(target)
            return target

    def get_server(self, name: str) -> Optional[ServerRecord]:
        with self._lock:
            server = self._servers.get(name)
            if server:
                return copy.deepcopy(server)
            return None

    def list_servers(self, filter_status: Optional[ToolStatus] = None) -> List[ServerRecord]:
        with self._lock:
            res: List[ServerRecord] = []
            for s in self._servers.values():
                if filter_status is None or s.status == filter_status:
                    res.append(copy.deepcopy(s))
            return res

    def list_all_tools(self, status: Optional[ToolStatus] = None) -> List[Dict[str, Any]]:
        with self._lock:
            all_tools: List[Dict[str, Any]] = []
            for s in self._servers.values():
                for t in s.tools.values():
                    if status is None or t.status == status:
                        all_tools.append({
                            "server_name": s.name,
                            "server_display_name": s.display_name,
                            "owning_team": s.owning_team,
                            "tool": copy.deepcopy(t),
                        })
            return all_tools

    def propose_tool_update(
        self,
        server_name: str,
        tool_name: str,
        new_description: Optional[str] = None,
        new_input_schema: Optional[Dict[str, Any]] = None,
        proposed_by: str = "service_owner",
    ) -> ConfigDiff:
        """Generates a config change diff."""
        with self._lock:
            server = self._servers.get(server_name)
            if not server:
                raise ValueError(f"Server '{server_name}' not found")

            tool = server.tools.get(tool_name)
            if not tool:
                raise ValueError(f"Tool '{tool_name}' not found on server '{server_name}'")

            before_state = {
                "description": tool.description,
                "input_schema": copy.deepcopy(tool.input_schema),
                "version": tool.version,
                "status": tool.status.value,
            }

            changes: List[str] = []
            after_state = copy.deepcopy(before_state)
            after_state["version"] = tool.version + 1

            if new_description and new_description != tool.description:
                changes.append(f"Description updated: '{tool.description}' -> '{new_description}'")
                after_state["description"] = new_description

            if new_input_schema and new_input_schema != tool.input_schema:
                changes.append("Input schema updated")
                after_state["input_schema"] = new_input_schema

            diff_id = f"diff-{uuid.uuid4().hex[:8]}"
            diff = ConfigDiff(
                diff_id=diff_id,
                server_name=server_name,
                tool_name=tool_name,
                previous_version=tool.version,
                proposed_version=tool.version + 1,
                before_state=before_state,
                after_state=after_state,
                changes_summary=changes or ["No changes detected"],
                proposed_by=proposed_by,
            )
            self._diffs[diff_id] = diff
            if self._store:
                self._store.save_diff(diff)
            return diff

    def approve_diff(self, diff_id: str, approved_by: str) -> bool:
        """Approve and deploy the config change."""
        with self._lock:
            diff = self._diffs.get(diff_id)
            if not diff or diff.approved:
                return False

            server = self._servers.get(diff.server_name)
            if not server:
                return False
            tool = server.tools.get(diff.tool_name)
            if not tool:
                return False

            # Apply after_state
            tool.description = diff.after_state["description"]
            tool.input_schema = copy.deepcopy(diff.after_state["input_schema"])
            tool.version = diff.proposed_version

            diff.approved = True
            diff.approved_by = approved_by
            diff.approved_at = time.time()

            self._record_history(server, f"Tool '{tool.name}' updated to v{tool.version} by {approved_by}")
            if self._store:
                self._store.save_server(server)
                self._store.save_diff(diff)
            return True

    def rollback_tool(self, server_name: str, tool_name: str, target_version: int) -> bool:
        """Roll back a tool configuration to a previous known version."""
        with self._lock:
            history = self._version_history.get(server_name, [])
            for snapshot in reversed(history):
                tools_snapshot = snapshot.get("tools", {})
                if tool_name in tools_snapshot:
                    candidate = tools_snapshot[tool_name]
                    if candidate.get("version") == target_version:
                        server = self._servers.get(server_name)
                        if not server:
                            return False
                        tool = server.tools.get(tool_name)
                        if not tool:
                            return False
                        tool.description = candidate["description"]
                        tool.input_schema = copy.deepcopy(candidate["input_schema"])
                        tool.version = target_version
                        self._record_history(server, f"Tool '{tool.name}' rolled back to v{target_version}")
                        if self._store:
                            self._store.save_server(server)
                        return True
            return False

    def toggle_tool_status(self, server_name: str, tool_name: str, new_status: ToolStatus) -> bool:
        with self._lock:
            server = self._servers.get(server_name)
            if not server:
                return False
            tool = server.tools.get(tool_name)
            if not tool:
                return False
            tool.status = new_status
            self._record_history(server, f"Tool '{tool_name}' status changed to {new_status.value}")
            if self._store:
                self._store.save_server(server)
            return True

    def toggle_server_status(self, server_name: str, new_status: ToolStatus) -> bool:
        with self._lock:
            server = self._servers.get(server_name)
            if not server:
                return False
            server.status = new_status
            self._record_history(server, f"Server status changed to {new_status.value}")
            if self._store:
                self._store.save_server(server)
            return True

    def get_diff(self, diff_id: str) -> Optional[ConfigDiff]:
        with self._lock:
            diff = self._diffs.get(diff_id)
            return copy.deepcopy(diff) if diff else None

    def list_pending_diffs(self, server_name: Optional[str] = None) -> List[ConfigDiff]:
        with self._lock:
            return [
                copy.deepcopy(d)
                for d in self._diffs.values()
                if not d.approved and (server_name is None or d.server_name == server_name)
            ]

    def _snapshot_server(self, server: ServerRecord) -> Dict[str, Any]:
        tools_dict = {}
        for t_name, t in server.tools.items():
            tools_dict[t_name] = {
                "name": t.name,
                "description": t.description,
                "input_schema": copy.deepcopy(t.input_schema),
                "version": t.version,
                "status": t.status.value,
            }
        return {
            "version": server.version,
            "timestamp": time.time(),
            "status": server.status.value,
            "tools": tools_dict,
        }

    def _record_history(self, server: ServerRecord, action_description: str) -> None:
        server.version += 1
        snapshot = self._snapshot_server(server)
        snapshot["action"] = action_description
        server.history.append({"action": action_description, "version": server.version, "time": time.time()})
        self._version_history.setdefault(server.name, []).append(snapshot)
