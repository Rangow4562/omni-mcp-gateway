"""
SQLite Registry Storage - Production persistent backend for Omni MCP Control Plane.
"""
from __future__ import annotations
import json
import sqlite3
import threading
from typing import Dict, Any, List, Optional
from omni_mcp.models.entities import (
    ServerRecord,
    ToolRecord,
    ToolStatus,
    ProtocolType,
    CharterPolicy,
    ActorType,
    ConfigDiff,
)


class SQLiteRegistryStore:
    """Thread-safe SQLite storage with WAL mode for persistent registry durability."""

    def __init__(self, db_path: str = ":memory:") -> None:
        self.db_path = db_path
        self._lock = threading.RLock()
        self._conn = sqlite3.connect(self.db_path, check_same_thread=False)
        self._conn.row_factory = sqlite3.Row
        self._init_db()

    def _init_db(self) -> None:
        with self._lock:
            cur = self._conn.cursor()
            if self.db_path != ":memory:":
                cur.execute("PRAGMA journal_mode=WAL;")
            cur.execute("""
                CREATE TABLE IF NOT EXISTS servers (
                    name TEXT PRIMARY KEY,
                    display_name TEXT,
                    description TEXT,
                    owning_team TEXT,
                    protocol TEXT,
                    status TEXT,
                    version INTEGER,
                    charter_policy_json TEXT,
                    heartbeat_timestamp REAL,
                    is_native INTEGER,
                    is_third_party INTEGER,
                    third_party_provider TEXT,
                    updated_at REAL DEFAULT (strftime('%s', 'now'))
                );
            """)
            cur.execute("""
                CREATE TABLE IF NOT EXISTS tools (
                    server_name TEXT,
                    tool_name TEXT,
                    description TEXT,
                    input_schema_json TEXT,
                    output_schema_json TEXT,
                    status TEXT,
                    version INTEGER,
                    protocol TEXT,
                    downstream_endpoint TEXT,
                    downstream_method TEXT,
                    charter_policy_json TEXT,
                    metadata_json TEXT,
                    PRIMARY KEY (server_name, tool_name)
                );
            """)
            cur.execute("""
                CREATE TABLE IF NOT EXISTS diffs (
                    diff_id TEXT PRIMARY KEY,
                    server_name TEXT,
                    tool_name TEXT,
                    previous_version INTEGER,
                    proposed_version INTEGER,
                    before_state_json TEXT,
                    after_state_json TEXT,
                    changes_summary_json TEXT,
                    proposed_by TEXT,
                    created_at REAL,
                    approved INTEGER,
                    approved_by TEXT,
                    approved_at REAL
                );
            """)
            cur.execute("""
                CREATE TABLE IF NOT EXISTS history (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    server_name TEXT,
                    action TEXT,
                    version INTEGER,
                    timestamp REAL,
                    snapshot_json TEXT
                );
            """)
            self._conn.commit()

    def save_server(self, server: ServerRecord) -> None:
        with self._lock:
            cur = self._conn.cursor()
            cur.execute("""
                INSERT OR REPLACE INTO servers (
                    name, display_name, description, owning_team, protocol,
                    status, version, charter_policy_json, heartbeat_timestamp,
                    is_native, is_third_party, third_party_provider, updated_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, strftime('%s', 'now'))
            """, (
                server.name,
                server.display_name,
                server.description,
                server.owning_team,
                server.protocol.value,
                server.status.value,
                server.version,
                json.dumps({
                    "allowed_actors": [a.value for a in server.charter_policy.allowed_actors],
                    "allowed_tools": server.charter_policy.allowed_tools,
                    "rate_limit_rpm": server.charter_policy.rate_limit_rpm,
                    "require_redaction": server.charter_policy.require_redaction,
                }),
                server.heartbeat_timestamp,
                1 if server.is_native else 0,
                1 if server.is_third_party else 0,
                server.third_party_provider,
            ))

            for tool in server.tools.values():
                policy_json = None
                if tool.charter_policy:
                    policy_json = json.dumps({
                        "allowed_actors": [a.value for a in tool.charter_policy.allowed_actors],
                        "allowed_tools": tool.charter_policy.allowed_tools,
                        "rate_limit_rpm": tool.charter_policy.rate_limit_rpm,
                        "require_redaction": tool.charter_policy.require_redaction,
                    })

                cur.execute("""
                    INSERT OR REPLACE INTO tools (
                        server_name, tool_name, description, input_schema_json,
                        output_schema_json, status, version, protocol,
                        downstream_endpoint, downstream_method, charter_policy_json, metadata_json
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """, (
                    server.name,
                    tool.name,
                    tool.description,
                    json.dumps(tool.input_schema),
                    json.dumps(tool.output_schema),
                    tool.status.value,
                    tool.version,
                    tool.protocol.value,
                    tool.downstream_endpoint,
                    tool.downstream_method,
                    policy_json,
                    json.dumps(tool.metadata),
                ))

            self._conn.commit()

    def get_server(self, name: str) -> Optional[ServerRecord]:
        with self._lock:
            cur = self._conn.cursor()
            cur.execute("SELECT * FROM servers WHERE name = ?", (name,))
            row = cur.fetchone()
            if not row:
                return None

            policy_data = json.loads(row["charter_policy_json"] or "{}")
            charter_policy = CharterPolicy(
                allowed_actors=[ActorType(a) for a in policy_data.get("allowed_actors", ["human", "service", "agent"])],
                allowed_tools=policy_data.get("allowed_tools", ["*"]),
                rate_limit_rpm=policy_data.get("rate_limit_rpm", 1200),
                require_redaction=policy_data.get("require_redaction", True),
            )

            # Load tools
            cur.execute("SELECT * FROM tools WHERE server_name = ?", (name,))
            tools_dict: Dict[str, ToolRecord] = {}
            for t_row in cur.fetchall():
                t_policy = None
                if t_row["charter_policy_json"]:
                    tp_data = json.loads(t_row["charter_policy_json"])
                    t_policy = CharterPolicy(
                        allowed_actors=[ActorType(a) for a in tp_data.get("allowed_actors", ["human", "service", "agent"])],
                        allowed_tools=tp_data.get("allowed_tools", ["*"]),
                        rate_limit_rpm=tp_data.get("rate_limit_rpm", 1200),
                        require_redaction=tp_data.get("require_redaction", True),
                    )

                tools_dict[t_row["tool_name"]] = ToolRecord(
                    name=t_row["tool_name"],
                    description=t_row["description"],
                    input_schema=json.loads(t_row["input_schema_json"]),
                    output_schema=json.loads(t_row["output_schema_json"] or "{}"),
                    status=ToolStatus(t_row["status"]),
                    version=t_row["version"],
                    protocol=ProtocolType(t_row["protocol"]),
                    downstream_endpoint=t_row["downstream_endpoint"],
                    downstream_method=t_row["downstream_method"],
                    charter_policy=t_policy,
                    metadata=json.loads(t_row["metadata_json"] or "{}"),
                )

            return ServerRecord(
                name=row["name"],
                display_name=row["display_name"],
                description=row["description"],
                owning_team=row["owning_team"],
                protocol=ProtocolType(row["protocol"]),
                status=ToolStatus(row["status"]),
                version=row["version"],
                tools=tools_dict,
                charter_policy=charter_policy,
                heartbeat_timestamp=row["heartbeat_timestamp"],
                is_native=bool(row["is_native"]),
                is_third_party=bool(row["is_third_party"]),
                third_party_provider=row["third_party_provider"],
            )

    def list_all_servers(self) -> List[ServerRecord]:
        with self._lock:
            cur = self._conn.cursor()
            cur.execute("SELECT name FROM servers")
            names = [r["name"] for r in cur.fetchall()]
            servers = []
            for n in names:
                srv = self.get_server(n)
                if srv:
                    servers.append(srv)
            return servers

    def save_diff(self, diff: ConfigDiff) -> None:
        with self._lock:
            cur = self._conn.cursor()
            cur.execute("""
                INSERT OR REPLACE INTO diffs (
                    diff_id, server_name, tool_name, previous_version,
                    proposed_version, before_state_json, after_state_json,
                    changes_summary_json, proposed_by, created_at,
                    approved, approved_by, approved_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """, (
                diff.diff_id,
                diff.server_name,
                diff.tool_name,
                diff.previous_version,
                diff.proposed_version,
                json.dumps(diff.before_state),
                json.dumps(diff.after_state),
                json.dumps(diff.changes_summary),
                diff.proposed_by,
                diff.created_at,
                1 if diff.approved else 0,
                diff.approved_by,
                diff.approved_at,
            ))
            self._conn.commit()

    def list_diffs(self, only_pending: bool = True) -> List[ConfigDiff]:
        with self._lock:
            cur = self._conn.cursor()
            if only_pending:
                cur.execute("SELECT * FROM diffs WHERE approved = 0")
            else:
                cur.execute("SELECT * FROM diffs")

            results = []
            for r in cur.fetchall():
                results.append(ConfigDiff(
                    diff_id=r["diff_id"],
                    server_name=r["server_name"],
                    tool_name=r["tool_name"],
                    previous_version=r["previous_version"],
                    proposed_version=r["proposed_version"],
                    before_state=json.loads(r["before_state_json"]),
                    after_state=json.loads(r["after_state_json"]),
                    changes_summary=json.loads(r["changes_summary_json"]),
                    proposed_by=r["proposed_by"],
                    created_at=r["created_at"],
                    approved=bool(r["approved"]),
                    approved_by=r["approved_by"],
                    approved_at=r["approved_at"],
                ))
            return results

    def close(self) -> None:
        with self._lock:
            self._conn.close()

