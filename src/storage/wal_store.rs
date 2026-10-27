//! Durable Write-Ahead Log (WAL) + Atomic Snapshot Storage Engine for Omni MCP Control Plane.
//! Provides ACID-like crash-safe persistence with FNV-1a checksum integrity and automatic WAL compaction.

use crate::json::JsonValue;
use crate::models::{ConfigDiff, ServerRecord, SuspendedExecutionFrame};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::RwLock;

pub type SQLiteRegistryStore = WalRegistryStore;

struct StoreState {
    servers: BTreeMap<String, ServerRecord>,
    diffs: BTreeMap<String, ConfigDiff>,
    suspended_frames: BTreeMap<String, SuspendedExecutionFrame>,
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
                                guard.suspended_frames.insert(frame.execution_id.clone(), frame);
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
                                    } else if op == "SAVE_DIFF" {
                                        if let Some(d_val) = entry.get("data") {
                                            if let Some(diff) = ConfigDiff::from_json(d_val) {
                                                guard.diffs.insert(diff.diff_id.clone(), diff);
                                            }
                                        }
                                    } else if op == "SAVE_SUSPENDED_FRAME" {
                                        if let Some(f_val) = entry.get("data") {
                                            if let Some(frame) = SuspendedExecutionFrame::from_json(f_val) {
                                                guard.suspended_frames.insert(frame.execution_id.clone(), frame);
                                            }
                                        }
                                    } else if op == "DELETE_SUSPENDED_FRAME" {
                                        if let Some(id) = entry.get("data").and_then(|d| d.as_str()) {
                                            guard.suspended_frames.remove(id);
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
        let snapshot = json!({
            "engine": "omni-rust-wal-v4.5",
            "servers": JsonValue::Array(servers_arr),
            "diffs": JsonValue::Array(diffs_arr),
            "suspended_frames": JsonValue::Array(frames_arr),
        });
        let tmp_path = format!("{}.tmp", self.db_path);
        if fs::write(&tmp_path, snapshot.to_string_compact()).is_ok() {
            let _ = fs::rename(&tmp_path, &self.db_path);
            // Truncate WAL after atomic snapshot commit
            let _ = File::create(self.wal_path());
        }
    }

    pub fn save_server(&self, server: &ServerRecord) {
        let mut guard = self.state.write().unwrap();
        guard.servers.insert(server.name.clone(), server.clone());
        self.append_wal_and_maybe_compact("SAVE_SERVER", server.to_json(), &mut guard);
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

    pub fn save_suspended_frame(&self, frame: &SuspendedExecutionFrame) {
        let mut guard = self.state.write().unwrap();
        guard
            .suspended_frames
            .insert(frame.execution_id.clone(), frame.clone());
        self.append_wal_and_maybe_compact("SAVE_SUSPENDED_FRAME", frame.to_json(), &mut guard);
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
        guard.suspended_frames.values().cloned().collect()
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
