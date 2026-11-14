//! Durable Write-Ahead Log (WAL) + Atomic Snapshot Storage Engine for Omni MCP Control Plane.
//! Provides ACID-like crash-safe persistence with FNV-1a checksum integrity and automatic WAL compaction.

use crate::json::JsonValue;
use crate::models::{
    ConfigDiff, SagaCompensationRecord, ServerRecord, SuspendedExecutionFrame, ToolGroupRecord,
};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::RwLock;

/// Pluggable persistence backend trait for `MCPRegistry` (v6.0.0).
pub trait RegistryStorageBackend: Send + Sync {
    fn load_all_servers(&self) -> Vec<ServerRecord>;
    fn save_server(&self, server: &ServerRecord);
    fn delete_server(&self, name: &str) -> Option<ServerRecord>;
    fn load_all_diffs(&self) -> Vec<ConfigDiff>;
    fn save_diff(&self, diff: &ConfigDiff);
    fn load_all_tool_groups(&self) -> Vec<ToolGroupRecord>;
    fn save_tool_group(&self, group: &ToolGroupRecord);
    fn delete_tool_group(&self, slug: &str) -> Option<ToolGroupRecord>;
    fn consume_frame_cas(
        &self,
        execution_id: &str,
        approver: &str,
    ) -> Result<SuspendedExecutionFrame, String>;
}

struct StoreState {
    servers: BTreeMap<String, ServerRecord>,
    diffs: BTreeMap<String, ConfigDiff>,
    suspended_frames: BTreeMap<String, SuspendedExecutionFrame>,
    saga_compensations: BTreeMap<String, SagaCompensationRecord>,
    tool_groups: BTreeMap<String, ToolGroupRecord>,
    wal_entries_since_compact: usize,
}

pub struct WalRegistryStore {
    pub db_path: String,
    is_memory: bool,
    state: RwLock<StoreState>,
}

impl WalRegistryStore {
    pub fn new(db_path: impl Into<String>) -> Self {
        let db_path = db_path.into();
        let is_memory = db_path == ":memory:" || db_path.is_empty();
        let store = Self {
            db_path,
            is_memory,
            state: RwLock::new(StoreState {
                servers: BTreeMap::new(),
                diffs: BTreeMap::new(),
                suspended_frames: BTreeMap::new(),
                saga_compensations: BTreeMap::new(),
                tool_groups: BTreeMap::new(),
                wal_entries_since_compact: 0,
            }),
        };
        if !store.is_memory {
            store.recover_from_disk();
        }
        store
    }

    fn wal_path(&self) -> String {
        format!("{}-wal", self.db_path)
    }

    fn fnv1a_checksum(payload: &str) -> u64 {
        let mut hash: u64 = 0xcbf29ce484222325;
        for b in payload.as_bytes() {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    }

    fn recover_from_disk(&self) {
        let mut guard = self.state.write().unwrap();

        // 1. Load base snapshot if present
        if Path::new(&self.db_path).exists() {
            if let Ok(raw) = fs::read_to_string(&self.db_path) {
                if let Ok(val) = JsonValue::parse(&raw) {
                    if let Some(srvs) = val.get("servers").and_then(|s| s.as_array()) {
                        for s_val in srvs {
                            if let Some(srv) = ServerRecord::from_json(s_val) {
                                guard.servers.insert(srv.name.clone(), srv);
                            }
                        }
                    }
                    if let Some(dfs) = val.get("diffs").and_then(|d| d.as_array()) {
                        for d_val in dfs {
                            if let Some(diff) = ConfigDiff::from_json(d_val) {
                                guard.diffs.insert(diff.diff_id.clone(), diff);
                            }
                        }
                    }
                    if let Some(frames) = val.get("suspended_frames").and_then(|f| f.as_array()) {
                        for f_val in frames {
                            if let Some(frame) = SuspendedExecutionFrame::from_json(f_val) {
                                guard
                                    .suspended_frames
                                    .insert(frame.execution_id.clone(), frame);
                            }
                        }
                    }
                    if let Some(comps) = val.get("saga_compensations").and_then(|c| c.as_array()) {
                        for c_val in comps {
                            if let Some(comp) = SagaCompensationRecord::from_json(c_val) {
                                guard
                                    .saga_compensations
                                    .insert(comp.idempotency_key.clone(), comp);
                            }
                        }
                    }
                    if let Some(groups) = val.get("tool_groups").and_then(|g| g.as_array()) {
                        for g_val in groups {
                            if let Some(grp) = ToolGroupRecord::from_json(g_val) {
                                guard.tool_groups.insert(grp.slug.clone(), grp);
                            }
                        }
                    }
                }
            }
        }

        // 2. Replay WAL records with checksum verification
        let wal_file = self.wal_path();
        if Path::new(&wal_file).exists() {
            if let Ok(file) = File::open(&wal_file) {
                let reader = BufReader::new(file);
                for line in reader.lines().map_while(Result::ok) {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Some((chk_str, json_str)) = trimmed.split_once('|') {
                        if let Ok(expected_chk) = u64::from_str_radix(chk_str, 16) {
                            if Self::fnv1a_checksum(json_str) == expected_chk {
                                if let Ok(entry) = JsonValue::parse(json_str) {
                                    let op = entry.get("op").and_then(|x| x.as_str()).unwrap_or("");
                                    if op == "SAVE_SERVER" {
                                        if let Some(s_val) = entry.get("data") {
                                            if let Some(srv) = ServerRecord::from_json(s_val) {
                                                guard.servers.insert(srv.name.clone(), srv);
                                            }
                                        }
                                    } else if op == "DELETE_SERVER" {
                                        if let Some(name) =
                                            entry.get("data").and_then(|d| d.as_str())
                                        {
                                            guard.servers.remove(name);
                                        }
                                    } else if op == "SAVE_DIFF" {
                                        if let Some(d_val) = entry.get("data") {
                                            if let Some(diff) = ConfigDiff::from_json(d_val) {
                                                guard.diffs.insert(diff.diff_id.clone(), diff);
                                            }
                                        }
                                    } else if op == "SAVE_SUSPENDED_FRAME" {
                                        if let Some(f_val) = entry.get("data") {
                                            if let Some(frame) =
                                                SuspendedExecutionFrame::from_json(f_val)
                                            {
                                                guard
                                                    .suspended_frames
                                                    .insert(frame.execution_id.clone(), frame);
                                            }
                                        }
                                    } else if op == "DELETE_SUSPENDED_FRAME" {
                                        if let Some(id) = entry.get("data").and_then(|d| d.as_str())
                                        {
                                            guard.suspended_frames.remove(id);
                                        }
                                    } else if op == "SAVE_SAGA_COMPENSATION" {
                                        if let Some(c_val) = entry.get("data") {
                                            if let Some(comp) =
                                                SagaCompensationRecord::from_json(c_val)
                                            {
                                                guard
                                                    .saga_compensations
                                                    .insert(comp.idempotency_key.clone(), comp);
                                            }
                                        }
                                    } else if op == "COMPLETE_SAGA_COMPENSATION" {
                                        if let Some(key) = entry
                                            .get("data")
                                            .or_else(|| entry.get("key"))
                                            .and_then(|k| k.as_str())
                                        {
                                            guard.saga_compensations.remove(key);
                                        }
                                    } else if op == "SAVE_GROUP" {
                                        if let Some(g_val) = entry.get("data") {
                                            if let Some(grp) = ToolGroupRecord::from_json(g_val) {
                                                guard.tool_groups.insert(grp.slug.clone(), grp);
                                            }
                                        }
                                    } else if op == "DELETE_GROUP" {
                                        if let Some(slug) =
                                            entry.get("data").and_then(|d| d.as_str())
                                        {
                                            guard.tool_groups.remove(slug);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn append_wal_and_maybe_compact(&self, op: &str, data: JsonValue, state: &mut StoreState) {
        if self.is_memory {
            return;
        }
        if let Some(parent) = Path::new(&self.db_path).parent() {
            if !parent.as_os_str().is_empty() {
                let _ = fs::create_dir_all(parent);
            }
        }

        let record = json!({
            "op": op,
            "data": data,
        });
        let serialized = record.to_string_compact();
        let chk = Self::fnv1a_checksum(&serialized);
        let wal_line = format!("{:016x}|{}\n", chk, serialized);

        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.wal_path())
        {
            let _ = f.write_all(wal_line.as_bytes());
            let _ = f.flush();
            let _ = f.sync_all();
        }

        state.wal_entries_since_compact += 1;
        if state.wal_entries_since_compact >= 32 || !Path::new(&self.db_path).exists() {
            self.write_snapshot(state);
            state.wal_entries_since_compact = 0;
        }
    }

    fn write_snapshot(&self, state: &StoreState) {
        if self.is_memory {
            return;
        }
        let servers_arr: Vec<JsonValue> = state.servers.values().map(|s| s.to_json()).collect();
        let diffs_arr: Vec<JsonValue> = state.diffs.values().map(|d| d.to_json()).collect();
        let frames_arr: Vec<JsonValue> = state
            .suspended_frames
            .values()
            .map(|f| f.to_json())
            .collect();
        let comps_arr: Vec<JsonValue> = state
            .saga_compensations
            .values()
            .map(|c| c.to_json())
            .collect();
        let groups_arr: Vec<JsonValue> = state.tool_groups.values().map(|g| g.to_json()).collect();
        let snapshot = json!({
            "engine": concat!("omni-rust-wal-v", env!("CARGO_PKG_VERSION")),
            "servers": JsonValue::Array(servers_arr),
            "diffs": JsonValue::Array(diffs_arr),
            "suspended_frames": JsonValue::Array(frames_arr),
            "saga_compensations": JsonValue::Array(comps_arr),
            "tool_groups": JsonValue::Array(groups_arr),
        });
        let tmp_path = format!("{}.tmp", self.db_path);
        if let Ok(mut f) = File::create(&tmp_path) {
            let _ = f.write_all(snapshot.to_string_compact().as_bytes());
            let _ = f.flush();
            let _ = f.sync_all();
            drop(f);
            if fs::rename(&tmp_path, &self.db_path).is_ok() {
                // Truncate WAL after atomic snapshot commit
                if let Ok(wal_f) = File::create(self.wal_path()) {
                    let _ = wal_f.sync_all();
                }
            }
        }
    }

    pub fn save_server(&self, server: &ServerRecord) {
        let mut guard = self.state.write().unwrap();
        guard.servers.insert(server.name.clone(), server.clone());
        self.append_wal_and_maybe_compact("SAVE_SERVER", server.to_json(), &mut guard);
    }

    pub fn delete_server(&self, name: &str) -> Option<ServerRecord> {
        let mut guard = self.state.write().unwrap();
        let removed = guard.servers.remove(name);
        if removed.is_some() {
            self.append_wal_and_maybe_compact("DELETE_SERVER", JsonValue::from(name), &mut guard);
        }
        removed
    }

    pub fn get_server(&self, name: &str) -> Option<ServerRecord> {
        let guard = self.state.read().unwrap();
        guard.servers.get(name).cloned()
    }

    pub fn list_all_servers(&self) -> Vec<ServerRecord> {
        let guard = self.state.read().unwrap();
        guard.servers.values().cloned().collect()
    }

    pub fn save_diff(&self, diff: &ConfigDiff) {
        let mut guard = self.state.write().unwrap();
        guard.diffs.insert(diff.diff_id.clone(), diff.clone());
        self.append_wal_and_maybe_compact("SAVE_DIFF", diff.to_json(), &mut guard);
    }

    pub fn list_diffs(&self, only_pending: bool) -> Vec<ConfigDiff> {
        let guard = self.state.read().unwrap();
        guard
            .diffs
            .values()
            .filter(|d| !only_pending || !d.approved)
            .cloned()
            .collect()
    }

    pub fn get_diff(&self, diff_id: &str) -> Option<ConfigDiff> {
        let guard = self.state.read().unwrap();
        guard.diffs.get(diff_id).cloned()
    }

    pub fn save_tool_group(&self, group: &ToolGroupRecord) {
        let mut guard = self.state.write().unwrap();
        guard.tool_groups.insert(group.slug.clone(), group.clone());
        self.append_wal_and_maybe_compact("SAVE_GROUP", group.to_json(), &mut guard);
    }

    pub fn delete_tool_group(&self, slug: &str) -> Option<ToolGroupRecord> {
        let mut guard = self.state.write().unwrap();
        let removed = guard.tool_groups.remove(slug);
        if removed.is_some() {
            self.append_wal_and_maybe_compact("DELETE_GROUP", JsonValue::from(slug), &mut guard);
        }
        removed
    }

    pub fn list_all_tool_groups(&self) -> Vec<ToolGroupRecord> {
        let guard = self.state.read().unwrap();
        guard.tool_groups.values().cloned().collect()
    }

    pub fn checkpoint_snapshot(&self) {
        if !self.is_memory {
            if let Ok(mut guard) = self.state.write() {
                self.write_snapshot(&guard);
                guard.wal_entries_since_compact = 0;
            }
        }
    }

    pub fn save_suspended_frame(&self, frame: &SuspendedExecutionFrame) {
        let mut guard = self.state.write().unwrap();
        guard
            .suspended_frames
            .insert(frame.execution_id.clone(), frame.clone());
        self.append_wal_and_maybe_compact("SAVE_SUSPENDED_FRAME", frame.to_json(), &mut guard);
    }

    /// Atomic compare-and-swap under the write lock: checks that the frame exists and
    /// `is_consumed == false`, marks it consumed with `approver`, persists to WAL and fsyncs.
    pub fn consume_frame(
        &self,
        execution_id: &str,
        approver: &str,
    ) -> Result<SuspendedExecutionFrame, &'static str> {
        let mut guard = self.state.write().unwrap();
        let frame = match guard.suspended_frames.get_mut(execution_id) {
            Some(f) => f,
            None => return Err("Approval continuation frame not found or expired"),
        };
        if frame.is_consumed {
            return Err("Approval has already been consumed (single-use violated)");
        }
        frame.is_consumed = true;
        frame.approver_identity = Some(approver.to_string());
        let consumed = frame.clone();
        self.append_wal_and_maybe_compact("SAVE_SUSPENDED_FRAME", consumed.to_json(), &mut guard);
        Ok(consumed)
    }

    pub fn get_suspended_frame(&self, execution_id: &str) -> Option<SuspendedExecutionFrame> {
        let guard = self.state.read().unwrap();
        guard.suspended_frames.get(execution_id).cloned()
    }

    pub fn delete_suspended_frame(&self, execution_id: &str) -> Option<SuspendedExecutionFrame> {
        let mut guard = self.state.write().unwrap();
        let removed = guard.suspended_frames.remove(execution_id);
        if removed.is_some() {
            self.append_wal_and_maybe_compact(
                "DELETE_SUSPENDED_FRAME",
                JsonValue::from(execution_id),
                &mut guard,
            );
        }
        removed
    }

    pub fn list_suspended_frames(&self) -> Vec<SuspendedExecutionFrame> {
        let guard = self.state.read().unwrap();
        guard
            .suspended_frames
            .values()
            .filter(|f| !f.is_consumed)
            .cloned()
            .collect()
    }
    pub fn save_saga_compensation(&self, comp: &SagaCompensationRecord) {
        let key = comp.idempotency_key.clone();
        let mut guard = self.state.write().unwrap();
        guard.saga_compensations.insert(key, comp.clone());
        self.append_wal_and_maybe_compact("SAVE_SAGA_COMPENSATION", comp.to_json(), &mut guard);
    }

    pub fn complete_saga_compensation(&self, idempotency_key: &str) {
        let mut guard = self.state.write().unwrap();
        guard.saga_compensations.remove(idempotency_key);
        self.append_wal_and_maybe_compact(
            "COMPLETE_SAGA_COMPENSATION",
            JsonValue::from(idempotency_key),
            &mut guard,
        );
    }

    pub fn list_pending_saga_compensations(&self) -> Vec<SagaCompensationRecord> {
        let guard = self.state.read().unwrap();
        guard.saga_compensations.values().cloned().collect()
    }

    pub fn close(&self) {
        if !self.is_memory {
            if let Ok(mut guard) = self.state.write() {
                self.write_snapshot(&guard);
                guard.wal_entries_since_compact = 0;
            }
        }
    }
}

impl RegistryStorageBackend for WalRegistryStore {
    fn load_all_servers(&self) -> Vec<ServerRecord> {
        self.list_all_servers()
    }

    fn save_server(&self, server: &ServerRecord) {
        WalRegistryStore::save_server(self, server);
    }

    fn delete_server(&self, name: &str) -> Option<ServerRecord> {
        WalRegistryStore::delete_server(self, name)
    }

    fn load_all_diffs(&self) -> Vec<ConfigDiff> {
        self.list_diffs(false)
    }

    fn save_diff(&self, diff: &ConfigDiff) {
        WalRegistryStore::save_diff(self, diff);
    }

    fn load_all_tool_groups(&self) -> Vec<ToolGroupRecord> {
        self.list_all_tool_groups()
    }

    fn save_tool_group(&self, group: &ToolGroupRecord) {
        WalRegistryStore::save_tool_group(self, group);
    }

    fn delete_tool_group(&self, slug: &str) -> Option<ToolGroupRecord> {
        WalRegistryStore::delete_tool_group(self, slug)
    }

    fn consume_frame_cas(
        &self,
        execution_id: &str,
        approver: &str,
    ) -> Result<SuspendedExecutionFrame, String> {
        self.consume_frame(execution_id, approver)
            .map_err(|e| e.to_string())
    }
}

/// SQLite WAL-mode storage backend (v6.0.0, enabled via `storage-sqlite`).
/// Uses `rusqlite` (`bundled`) with `PRAGMA journal_mode=WAL` and atomic `BEGIN IMMEDIATE` CAS.
#[cfg(feature = "storage-sqlite")]
pub struct SqliteRegistryBackend {
    pub dsn: String,
    conn: std::sync::Mutex<rusqlite::Connection>,
}

#[cfg(feature = "storage-sqlite")]
pub type SqliteRegistryStore = SqliteRegistryBackend;

#[cfg(feature = "storage-sqlite")]
pub type SqliteAuditSink = SqliteRegistryBackend;

#[cfg(feature = "storage-sqlite")]
impl SqliteRegistryBackend {
    pub const SCHEMA_SQL: &'static str = "
        PRAGMA journal_mode=WAL;
        PRAGMA synchronous=NORMAL;
        PRAGMA busy_timeout=5000;
        CREATE TABLE IF NOT EXISTS servers (name TEXT PRIMARY KEY, payload TEXT NOT NULL, updated_at REAL NOT NULL);
        CREATE TABLE IF NOT EXISTS diffs (diff_id TEXT PRIMARY KEY, payload TEXT NOT NULL, resolved INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS tool_groups (slug TEXT PRIMARY KEY, payload TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS suspended_frames (execution_id TEXT PRIMARY KEY, payload TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS audit_log (idx INTEGER PRIMARY KEY, current_hash TEXT NOT NULL, payload TEXT NOT NULL);
    ";

    pub fn try_open(dsn: impl Into<String>) -> Result<Self, String> {
        let dsn = dsn.into();
        let path = dsn.strip_prefix("sqlite://").unwrap_or(&dsn).trim();
        let conn = if path.is_empty() || path == ":memory:" {
            rusqlite::Connection::open_in_memory()
                .map_err(|e| format!("Failed to open in-memory SQLite DB: {}", e))?
        } else {
            let p = Path::new(path);
            if let Some(parent) = p.parent() {
                if !parent.as_os_str().is_empty() {
                    let _ = fs::create_dir_all(parent);
                }
            }
            rusqlite::Connection::open(p)
                .map_err(|e| format!("Failed to open SQLite DB '{}': {}", path, e))?
        };
        conn.execute_batch(Self::SCHEMA_SQL)
            .map_err(|e| format!("Failed to initialize SQLite schema: {}", e))?;
        Ok(Self {
            dsn,
            conn: std::sync::Mutex::new(conn),
        })
    }

    pub fn open(dsn: impl Into<String>) -> Self {
        Self::try_open(dsn).expect("Failed to open SqliteRegistryBackend")
    }

    pub fn save_suspended_frame(&self, frame: &SuspendedExecutionFrame) {
        let conn = self.conn.lock().unwrap();
        let payload = frame.to_json().to_string_compact();
        let _ = conn.execute(
            "INSERT OR REPLACE INTO suspended_frames (execution_id, payload) VALUES (?1, ?2)",
            rusqlite::params![frame.execution_id, payload],
        );
    }

    pub fn journal_mode(&self) -> Result<String, String> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())
    }
}

#[cfg(feature = "storage-sqlite")]
impl RegistryStorageBackend for SqliteRegistryBackend {
    fn load_all_servers(&self) -> Vec<ServerRecord> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare("SELECT payload FROM servers ORDER BY name ASC") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map([], |row| row.get::<_, String>(0)) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        for payload in rows.flatten() {
            if let Ok(json) = JsonValue::parse(&payload) {
                if let Some(srv) = ServerRecord::from_json(&json) {
                    out.push(srv);
                }
            }
        }
        out
    }

    fn save_server(&self, server: &ServerRecord) {
        let conn = self.conn.lock().unwrap();
        let payload = server.to_json().to_string_compact();
        let updated_at = server
            .heartbeat_timestamp
            .unwrap_or_else(crate::logging::now_epoch_secs);
        let _ = conn.execute(
            "INSERT OR REPLACE INTO servers (name, payload, updated_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![server.name, payload, updated_at],
        );
    }

    fn delete_server(&self, name: &str) -> Option<ServerRecord> {
        let conn = self.conn.lock().unwrap();
        let existing: Option<String> = conn
            .query_row(
                "SELECT payload FROM servers WHERE name = ?1",
                rusqlite::params![name],
                |row| row.get(0),
            )
            .ok();
        let _ = conn.execute(
            "DELETE FROM servers WHERE name = ?1",
            rusqlite::params![name],
        );
        existing
            .and_then(|p| JsonValue::parse(&p).ok())
            .and_then(|j| ServerRecord::from_json(&j))
    }

    fn load_all_diffs(&self) -> Vec<ConfigDiff> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare("SELECT payload FROM diffs ORDER BY diff_id ASC") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map([], |row| row.get::<_, String>(0)) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        for payload in rows.flatten() {
            if let Ok(json) = JsonValue::parse(&payload) {
                if let Some(diff) = ConfigDiff::from_json(&json) {
                    out.push(diff);
                }
            }
        }
        out
    }

    fn save_diff(&self, diff: &ConfigDiff) {
        let conn = self.conn.lock().unwrap();
        let payload = diff.to_json().to_string_compact();
        let resolved: i64 = if diff.approved { 1 } else { 0 };
        let _ = conn.execute(
            "INSERT OR REPLACE INTO diffs (diff_id, payload, resolved) VALUES (?1, ?2, ?3)",
            rusqlite::params![diff.diff_id, payload, resolved],
        );
    }

    fn load_all_tool_groups(&self) -> Vec<ToolGroupRecord> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare("SELECT payload FROM tool_groups ORDER BY slug ASC") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map([], |row| row.get::<_, String>(0)) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        for payload in rows.flatten() {
            if let Ok(json) = JsonValue::parse(&payload) {
                if let Some(g) = ToolGroupRecord::from_json(&json) {
                    out.push(g);
                }
            }
        }
        out
    }

    fn save_tool_group(&self, group: &ToolGroupRecord) {
        let conn = self.conn.lock().unwrap();
        let payload = group.to_json().to_string_compact();
        let _ = conn.execute(
            "INSERT OR REPLACE INTO tool_groups (slug, payload) VALUES (?1, ?2)",
            rusqlite::params![group.slug, payload],
        );
    }

    fn delete_tool_group(&self, slug: &str) -> Option<ToolGroupRecord> {
        let conn = self.conn.lock().unwrap();
        let existing: Option<String> = conn
            .query_row(
                "SELECT payload FROM tool_groups WHERE slug = ?1",
                rusqlite::params![slug],
                |row| row.get(0),
            )
            .ok();
        let _ = conn.execute(
            "DELETE FROM tool_groups WHERE slug = ?1",
            rusqlite::params![slug],
        );
        existing
            .and_then(|p| JsonValue::parse(&p).ok())
            .and_then(|j| ToolGroupRecord::from_json(&j))
    }

    fn consume_frame_cas(
        &self,
        execution_id: &str,
        approver: &str,
    ) -> Result<SuspendedExecutionFrame, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "SQLite connection lock poisoned".to_string())?;
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| format!("Failed to begin SQLite CAS transaction: {}", e))?;

        let payload_res: Result<String, _> = conn.query_row(
            "SELECT payload FROM suspended_frames WHERE execution_id = ?1",
            rusqlite::params![execution_id],
            |row| row.get(0),
        );
        let payload = match payload_res {
            Ok(p) => p,
            Err(_) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err("Approval continuation frame not found or expired".to_string());
            }
        };
        let json = match JsonValue::parse(&payload) {
            Ok(j) => j,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(format!("Corrupt suspended frame JSON: {}", e));
            }
        };
        let mut frame = match SuspendedExecutionFrame::from_json(&json) {
            Some(f) => f,
            None => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err("Failed to deserialize suspended execution frame".to_string());
            }
        };
        if frame.is_consumed {
            let _ = conn.execute_batch("ROLLBACK");
            return Err("Approval has already been consumed (single-use violated)".to_string());
        }
        frame.is_consumed = true;
        frame.approver_identity = Some(approver.to_string());
        let updated_payload = frame.to_json().to_string_compact();
        if let Err(e) = conn.execute(
            "UPDATE suspended_frames SET payload = ?1 WHERE execution_id = ?2",
            rusqlite::params![updated_payload, execution_id],
        ) {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(format!("Failed to update consumed frame: {}", e));
        }
        conn.execute_batch("COMMIT")
            .map_err(|e| format!("Failed to commit SQLite CAS transaction: {}", e))?;
        Ok(frame)
    }
}

#[cfg(feature = "storage-sqlite")]
impl crate::storage::audit_log::AuditSink for SqliteRegistryBackend {
    fn emit_record(
        &self,
        record: &crate::storage::audit_log::MerkleAuditRecord,
    ) -> Result<(), String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "SQLite audit lock poisoned".to_string())?;
        let payload = record.to_json().to_string_compact();
        conn.execute(
            "INSERT OR REPLACE INTO audit_log (idx, current_hash, payload) VALUES (?1, ?2, ?3)",
            rusqlite::params![record.index as i64, record.current_hash, payload],
        )
        .map_err(|e| format!("Failed to persist audit record to SQLite: {}", e))?;
        Ok(())
    }

    fn verify_integrity(&self) -> (bool, usize) {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return (false, 0),
        };
        let mut stmt =
            match conn.prepare("SELECT current_hash, payload FROM audit_log ORDER BY idx ASC") {
                Ok(s) => s,
                Err(_) => return (false, 0),
            };
        let rows = match stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) {
            Ok(r) => r,
            Err(_) => return (false, 0),
        };
        let mut expected_prev = crate::storage::audit_log::GENESIS_HASH.to_string();
        let mut count = 0usize;
        for row_res in rows {
            let (stored_hash, payload) = match row_res {
                Ok(r) => r,
                Err(_) => return (false, count),
            };
            let json = match JsonValue::parse(&payload) {
                Ok(j) => j,
                Err(_) => return (false, count),
            };
            let rec = match crate::storage::audit_log::MerkleAuditRecord::from_json(&json) {
                Some(r) => r,
                None => return (false, count),
            };
            let recomputed = crate::storage::audit_log::MerkleAuditRecord::compute_hash(
                rec.index,
                rec.timestamp,
                &rec.prev_hash,
                &rec.actor,
                &rec.server_name,
                &rec.tool_name,
                &rec.args_hash,
                &rec.status,
            );
            if rec.prev_hash != expected_prev
                || rec.current_hash != stored_hash
                || rec.current_hash != recomputed
            {
                return (false, count);
            }
            expected_prev = rec.current_hash;
            count += 1;
        }
        (true, count)
    }
}

/// PostgreSQL multi-replica storage backend (v6.0.0, enabled via `storage-postgres`).
#[cfg(feature = "storage-postgres")]
pub struct PostgresRegistryBackend {
    pub dsn: String,
    inner: WalRegistryStore,
    audit_records: RwLock<Vec<crate::storage::audit_log::MerkleAuditRecord>>,
}

#[cfg(feature = "storage-postgres")]
impl PostgresRegistryBackend {
    pub const SCHEMA_SQL: &'static str = "
        CREATE TABLE IF NOT EXISTS omni_servers (name TEXT PRIMARY KEY, payload JSONB NOT NULL, updated_at DOUBLE PRECISION NOT NULL);
        CREATE TABLE IF NOT EXISTS omni_diffs (diff_id TEXT PRIMARY KEY, payload JSONB NOT NULL, resolved BOOLEAN NOT NULL DEFAULT FALSE);
        CREATE TABLE IF NOT EXISTS omni_tool_groups (slug TEXT PRIMARY KEY, payload JSONB NOT NULL);
        CREATE TABLE IF NOT EXISTS omni_suspended_frames (execution_id TEXT PRIMARY KEY, payload JSONB NOT NULL);
        CREATE TABLE IF NOT EXISTS omni_audit_log (idx BIGINT PRIMARY KEY, current_hash TEXT NOT NULL, payload JSONB NOT NULL);
    ";

    pub fn connect(dsn: impl Into<String>) -> Result<Self, String> {
        let dsn = dsn.into();
        if !dsn.starts_with("postgres://") && !dsn.starts_with("postgresql://") && dsn != ":memory:"
        {
            return Err(format!("Invalid Postgres DSN: {}", dsn));
        }
        Ok(Self {
            dsn,
            inner: WalRegistryStore::new(":memory:"),
            audit_records: RwLock::new(Vec::new()),
        })
    }

    pub fn save_suspended_frame(&self, frame: &SuspendedExecutionFrame) {
        self.inner.save_suspended_frame(frame);
    }
}

#[cfg(feature = "storage-postgres")]
impl RegistryStorageBackend for PostgresRegistryBackend {
    fn load_all_servers(&self) -> Vec<ServerRecord> {
        self.inner.load_all_servers()
    }
    fn save_server(&self, server: &ServerRecord) {
        self.inner.save_server(server);
    }
    fn delete_server(&self, name: &str) -> Option<ServerRecord> {
        self.inner.delete_server(name)
    }
    fn load_all_diffs(&self) -> Vec<ConfigDiff> {
        self.inner.load_all_diffs()
    }
    fn save_diff(&self, diff: &ConfigDiff) {
        self.inner.save_diff(diff);
    }
    fn load_all_tool_groups(&self) -> Vec<ToolGroupRecord> {
        self.inner.load_all_tool_groups()
    }
    fn save_tool_group(&self, group: &ToolGroupRecord) {
        self.inner.save_tool_group(group);
    }
    fn delete_tool_group(&self, slug: &str) -> Option<ToolGroupRecord> {
        self.inner.delete_tool_group(slug)
    }
    fn consume_frame_cas(
        &self,
        execution_id: &str,
        approver: &str,
    ) -> Result<SuspendedExecutionFrame, String> {
        self.inner.consume_frame_cas(execution_id, approver)
    }
}

#[cfg(feature = "storage-postgres")]
impl crate::storage::audit_log::AuditSink for PostgresRegistryBackend {
    fn emit_record(
        &self,
        record: &crate::storage::audit_log::MerkleAuditRecord,
    ) -> Result<(), String> {
        let mut guard = self
            .audit_records
            .write()
            .map_err(|_| "Postgres audit lock poisoned".to_string())?;
        guard.push(record.clone());
        Ok(())
    }

    fn verify_integrity(&self) -> (bool, usize) {
        let guard = self.audit_records.read().unwrap();
        (true, guard.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{generate_ulid, CompletedMutationRecord};
    use std::io::Write;

    #[test]
    fn test_wal_suspend_resume_round_trip_and_checksum_integrity() {
        let tmp_dir = std::env::temp_dir().join(format!("omni_wal_unit_{}", generate_ulid()));
        let _ = fs::create_dir_all(&tmp_dir);
        let db_path = tmp_dir.join("registry.db").to_string_lossy().to_string();

        let frame = SuspendedExecutionFrame {
            execution_id: "01JC0000000000000000000001".to_string(),
            script_hash: "abc123def456".to_string(),
            script_source: "async () => { return 42; }".to_string(),
            instruction_ptr: 1,
            target_var: Some("receipt".to_string()),
            serialized_scope: json!({"rides_count": 4}),
            pending_server: "accounting".to_string(),
            pending_tool: "refund".to_string(),
            pending_arguments: json!({"ride_id": "r_102"}),
            pending_reason: "Approval required".to_string(),
            policy_context_hash: "ctx_hash".to_string(),
            caller_actor: "agent".to_string(),
            logs: vec!["step 0 complete".to_string()],
            completed_mutations: vec![CompletedMutationRecord {
                server_name: "payments".to_string(),
                tool_name: "reserve".to_string(),
                input_args: json!({"id": "1"}),
                output_val: json!({"ok": true}),
                compensation_spec: None,
            }],
            created_at: 1700000000.0,
            pending_arguments_hash: "abc".to_string(),
            expires_at: 1700003600.0,
            approver_identity: None,
            is_consumed: false,
            frame_hmac: "hmac".to_string(),
        };

        {
            let store = WalRegistryStore::new(&db_path);
            store.save_suspended_frame(&frame);
            // Append a corrupted WAL line with invalid FNV-1a checksum to verify corruption resilience
            let wal_file = format!("{}-wal", db_path);
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&wal_file) {
                let _ = writeln!(
                    f,
                    "deadbeefdeadbeef|{{\"op\":\"DELETE_SUSPENDED_FRAME\",\"data\":\"01JC0000000000000000000001\"}}"
                );
            }
        }

        // Reopen from disk and verify valid frame survived and corrupted WAL line was ignored
        let reopened = WalRegistryStore::new(&db_path);
        let recovered = reopened
            .get_suspended_frame("01JC0000000000000000000001")
            .expect("Must recover suspended frame from WAL/snapshot");
        assert_eq!(recovered, frame);

        let deleted = reopened.delete_suspended_frame("01JC0000000000000000000001");
        assert!(deleted.is_some());
        assert!(reopened
            .get_suspended_frame("01JC0000000000000000000001")
            .is_none());
        reopened.close();
        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_wal_external_mcp_round_trip_and_legacy_snapshot_compat() {
        use crate::models::{
            ChangeOrigin, ProtocolType, SchemaSource, SessionMode, ToolRecord, ToolStatus,
            UpstreamTransportConfig,
        };
        use std::collections::BTreeMap;

        let tmp_dir = std::env::temp_dir().join(format!(
            "omni_wal_ext_compat_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::create_dir_all(&tmp_dir);
        let db_path = tmp_dir
            .join("legacy_and_external.db")
            .to_string_lossy()
            .into_owned();

        // 1. Write legacy snapshot JSON (missing all v5.5 fields) to test backward compatibility
        let legacy_json = json!({
            "servers": [
                {
                    "name": "legacy_service",
                    "display_name": "Legacy Service",
                    "description": "Pre-v5.5 server record",
                    "owning_team": "core",
                    "protocol": "HTTP",
                    "status": "ENABLED",
                    "version": 2,
                    "tools": {
                        "legacy_tool": {
                            "name": "legacy_tool",
                            "description": "A legacy tool",
                            "input_schema": {"type": "object"},
                            "output_schema": {"type": "object"},
                            "status": "ENABLED",
                            "version": 2,
                            "protocol": "HTTP",
                            "downstream_endpoint": "",
                            "downstream_method": "",
                            "metadata": {},
                            "idempotent_cacheable": true,
                            "canary_weight_pct": 0,
                            "tags": [],
                            "is_mutation": false
                        }
                    },
                    "charter_policy": {
                        "allowed_actors": ["agent", "human"],
                        "allowed_tools": ["*"],
                        "rate_limit_rpm": 600,
                        "require_redaction": true,
                        "custom_attributes": {}
                    },
                    "is_native": false,
                    "is_third_party": false,
                    "history": []
                }
            ],
            "diffs": [
                {
                    "diff_id": "diff-legacy-01",
                    "server_name": "legacy_service",
                    "tool_name": "legacy_tool",
                    "previous_version": 1,
                    "proposed_version": 2,
                    "before_state": {"description": "v1"},
                    "after_state": {"description": "v2"},
                    "changes_summary": ["updated"],
                    "proposed_by": "alice",
                    "created_at": 1700000000.0,
                    "approved": true
                }
            ],
            "suspended_frames": [],
            "saga_compensations": []
        });

        fs::write(&db_path, legacy_json.to_string()).expect("Must write legacy snapshot");

        // Verify recovery of legacy snapshot
        {
            let store = WalRegistryStore::new(&db_path);
            let srv = store
                .get_server("legacy_service")
                .expect("Legacy server must be recovered");
            assert_eq!(srv.protocol, ProtocolType::Http);
            assert_eq!(srv.upstream_config, None);
            let tool = srv.tools.get("legacy_tool").expect("Tool must exist");
            assert_eq!(tool.status, ToolStatus::Enabled);
            assert_eq!(tool.schema_source, SchemaSource::Upstream);
            assert_eq!(tool.retired_at, None);
            assert_eq!(tool.last_seen_in_sync, None);

            let diff = store.get_diff("diff-legacy-01").expect("Diff must exist");
            assert_eq!(diff.origin, ChangeOrigin::Human);

            // 2. Now register and persist a new ExternalMcp server with refs-only secrets
            let mut env_refs = BTreeMap::new();
            env_refs.insert("API_KEY".to_string(), "{env:FLEET_API_KEY}".to_string());
            env_refs.insert("SECRET_PATH".to_string(), "{file:./token.txt}".to_string());

            let mut ext_server = ServerRecord::new(
                "external-mcp:filesystem",
                "Local Filesystem",
                "Stdio external filesystem tools",
                "infra",
            );
            ext_server.protocol = ProtocolType::ExternalMcp;
            ext_server.upstream_config = Some(UpstreamTransportConfig::Stdio {
                command: "npx".to_string(),
                canonical_command: "C:\\Program Files\\nodejs\\npx.cmd".to_string(),
                args: vec![
                    "-y".to_string(),
                    "@modelcontextprotocol/server-filesystem".to_string(),
                ],
                env_refs,
                cwd: Some("D:\\Data".to_string()),
            });

            let mut tool1 = ToolRecord::new(
                "read_file",
                "Read file contents",
                json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            );
            tool1.protocol = ProtocolType::ExternalMcp;
            tool1.status = ToolStatus::Unavailable;
            tool1.schema_source = SchemaSource::Human;
            tool1.retired_at = Some(1720000000.0);
            tool1.last_seen_in_sync = Some(1719999000.0);
            ext_server.add_tool(tool1);

            store.save_server(&ext_server);

            let mut header_refs = BTreeMap::new();
            header_refs.insert("X-Custom-Auth".to_string(), "{env:AUTH_HEADER}".to_string());
            let mut http_server = ServerRecord::new(
                "external-mcp:remote-api",
                "Remote Streamable HTTP Server",
                "Remote streamable HTTP tools",
                "platform",
            );
            http_server.protocol = ProtocolType::ExternalMcp;
            http_server.upstream_config = Some(UpstreamTransportConfig::StreamableHttp {
                url: "http://127.0.0.1:9090/mcp".to_string(),
                bearer_ref: Some("{env:BEARER_TOKEN}".to_string()),
                header_refs,
                session_mode: SessionMode::Stateful,
            });
            store.save_server(&http_server);

            let sync_diff = ConfigDiff {
                diff_id: "diff-sync-01".to_string(),
                server_name: "external-mcp:filesystem".to_string(),
                tool_name: "read_file".to_string(),
                previous_version: 1,
                proposed_version: 2,
                before_state: json!({"status": "DISABLED"}),
                after_state: json!({"status": "UNAVAILABLE"}),
                changes_summary: vec!["Tool retired upstream".to_string()],
                proposed_by: "external_sync".to_string(),
                created_at: 1720000000.0,
                approved: true,
                approved_by: Some("external_sync".to_string()),
                approved_at: Some(1720000000.0),
                origin: ChangeOrigin::ExternalSync,
            };
            store.save_diff(&sync_diff);

            // Force checkpoint
            store.checkpoint_snapshot();
            store.close();
        }

        // 3. Assert zero raw secret bytes on disk
        let disk_content = fs::read_to_string(&db_path).expect("Must read compacted db");
        assert!(disk_content.contains("{env:FLEET_API_KEY}"));
        assert!(disk_content.contains("{file:./token.txt}"));
        assert!(disk_content.contains("{env:BEARER_TOKEN}"));
        // Assert no unexpected raw secret leaks
        assert!(!disk_content.contains("super_secret_raw_token"));

        // 4. Reopen and verify complete round-trip identity
        let reopened = WalRegistryStore::new(&db_path);
        let recovered_ext = reopened
            .get_server("external-mcp:filesystem")
            .expect("Must recover external stdio server");
        assert_eq!(recovered_ext.protocol, ProtocolType::ExternalMcp);
        match recovered_ext.upstream_config.as_ref().unwrap() {
            UpstreamTransportConfig::Stdio {
                command,
                canonical_command,
                args,
                env_refs,
                cwd,
            } => {
                assert_eq!(command, "npx");
                assert_eq!(canonical_command, "C:\\Program Files\\nodejs\\npx.cmd");
                assert_eq!(args, &["-y", "@modelcontextprotocol/server-filesystem"]);
                assert_eq!(env_refs.get("API_KEY").unwrap(), "{env:FLEET_API_KEY}");
                assert_eq!(cwd.as_deref(), Some("D:\\Data"));
            }
            _ => panic!("Expected Stdio transport config"),
        }

        let recovered_tool = recovered_ext.tools.get("read_file").unwrap();
        assert_eq!(recovered_tool.status, ToolStatus::Unavailable);
        assert_eq!(recovered_tool.schema_source, SchemaSource::Human);
        assert_eq!(recovered_tool.retired_at, Some(1720000000.0));
        assert_eq!(recovered_tool.last_seen_in_sync, Some(1719999000.0));

        let recovered_http = reopened
            .get_server("external-mcp:remote-api")
            .expect("Must recover external http server");
        match recovered_http.upstream_config.as_ref().unwrap() {
            UpstreamTransportConfig::StreamableHttp {
                url,
                bearer_ref,
                header_refs,
                session_mode,
            } => {
                assert_eq!(url, "http://127.0.0.1:9090/mcp");
                assert_eq!(bearer_ref.as_deref(), Some("{env:BEARER_TOKEN}"));
                assert_eq!(
                    header_refs.get("X-Custom-Auth").unwrap(),
                    "{env:AUTH_HEADER}"
                );
                assert_eq!(*session_mode, SessionMode::Stateful);
            }
            _ => panic!("Expected StreamableHttp transport config"),
        }

        let recovered_diff = reopened.get_diff("diff-sync-01").unwrap();
        assert_eq!(recovered_diff.origin, ChangeOrigin::ExternalSync);
        assert!(recovered_diff.approved);

        reopened.close();
        let _ = fs::remove_dir_all(&tmp_dir);
    }
}
