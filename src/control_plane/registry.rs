//! MCP Registry - Single source of truth for discovery, ownership, versioning, diff approval, and enablement.
//! Supports both high-throughput concurrent in-memory state and durable WAL storage.

use crate::json::JsonValue;
use crate::logging::now_epoch_secs;
use crate::models::{
    ConfigDiff, HistoryEntry, ServerRecord, SuspendedExecutionFrame, ToolRecord, ToolStatus,
};
use crate::storage::WalRegistryStore;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

static DIFF_COUNTER: AtomicU64 = AtomicU64::new(1);

struct RegistryInner {
    servers: BTreeMap<String, ServerRecord>,
    diffs: BTreeMap<String, ConfigDiff>,
    suspended_frames: BTreeMap<String, SuspendedExecutionFrame>,
    version_history: HashMap<String, Vec<JsonValue>>,
}

#[derive(Clone)]
pub struct MCPRegistry {
    inner: Arc<RwLock<RegistryInner>>,
    store: Option<Arc<WalRegistryStore>>,
}

impl Default for MCPRegistry {
    fn default() -> Self {
        Self::new(None)
    }
}

impl MCPRegistry {
    pub fn new(persistent_store: Option<Arc<WalRegistryStore>>) -> Self {
        let mut servers = BTreeMap::new();
        let mut diffs = BTreeMap::new();
        let mut suspended_frames = BTreeMap::new();
        let mut version_history: HashMap<String, Vec<JsonValue>> = HashMap::new();

        if let Some(store) = &persistent_store {
            for s in store.list_all_servers() {
                let snap = Self::snapshot_server(&s);
                version_history.insert(s.name.clone(), vec![snap]);
                servers.insert(s.name.clone(), s);
            }
            for d in store.list_diffs(false) {
                diffs.insert(d.diff_id.clone(), d);
            }
            for f in store.list_suspended_frames() {
                suspended_frames.insert(f.execution_id.clone(), f);
            }
        }

        Self {
            inner: Arc::new(RwLock::new(RegistryInner {
                servers,
                diffs,
                suspended_frames,
                version_history,
            })),
            store: persistent_store,
        }
    }

    pub fn register_server(&self, server: ServerRecord) -> ServerRecord {
        let mut guard = self.inner.write().unwrap();
        let target = if let Some(existing) = guard.servers.get_mut(&server.name) {
            for (t_name, mut tool) in server.tools {
                if let Some(prev) = existing.tools.get(&t_name) {
                    if prev.status == ToolStatus::Enabled && tool.status == ToolStatus::Disabled {
                        tool.status = ToolStatus::Enabled;
                    }
                    if tool.compensation_spec.is_none() && prev.compensation_spec.is_some() {
                        tool.compensation_spec = prev.compensation_spec.clone();
                    }
                    if !tool.is_mutation && prev.is_mutation {
                        tool.is_mutation = true;
                    }
                }
                existing.tools.insert(t_name, tool);
            }
            if server.heartbeat_timestamp.is_some() {
                existing.heartbeat_timestamp = server.heartbeat_timestamp;
            }
            existing.clone()
        } else {
            let snap = Self::snapshot_server(&server);
            guard
                .version_history
                .insert(server.name.clone(), vec![snap]);
            guard.servers.insert(server.name.clone(), server.clone());
            server
        };

        if let Some(store) = &self.store {
            store.save_server(&target);
        }
        target
    }

    pub fn get_server(&self, name: &str) -> Option<ServerRecord> {
        let guard = self.inner.read().unwrap();
        guard.servers.get(name).cloned()
    }

    pub fn list_servers(&self, filter_status: Option<ToolStatus>) -> Vec<ServerRecord> {
        let guard = self.inner.read().unwrap();
        guard
            .servers
            .values()
            .filter(|s| filter_status.is_none_or(|st| s.status == st))
            .cloned()
            .collect()
    }

    pub fn list_all_tools(&self, status: Option<ToolStatus>) -> Vec<(ServerRecord, ToolRecord)> {
        let guard = self.inner.read().unwrap();
        let mut out = Vec::new();
        for s in guard.servers.values() {
            for t in s.tools.values() {
                if status.is_none_or(|st| t.status == st) {
                    out.push((s.clone(), t.clone()));
                }
            }
        }
        out
    }

    pub fn propose_tool_update(
        &self,
        server_name: &str,
        tool_name: &str,
        new_description: Option<&str>,
        new_input_schema: Option<JsonValue>,
        proposed_by: &str,
    ) -> Result<ConfigDiff, String> {
        let mut guard = self.inner.write().unwrap();
        let (snap, tool_clone) = {
            let server = guard
                .servers
                .get(server_name)
                .ok_or_else(|| format!("Server '{}' not found", server_name))?;
            let tool = server.tools.get(tool_name).ok_or_else(|| {
                format!("Tool '{}' not found on server '{}'", tool_name, server_name)
            })?;
            (Self::snapshot_server(server), tool.clone())
        };

        // Ensure initial v1 snapshot exists in version_history
        let hist = guard
            .version_history
            .entry(server_name.to_string())
            .or_default();
        if hist.is_empty() {
            hist.push(snap);
        } else {
            let last_idx = hist.len() - 1;
            if let Some(tools_obj) = hist[last_idx].get_mut("tools") {
                if !tools_obj.contains_key(tool_name) {
                    tools_obj.insert(
                        tool_name.to_string(),
                        json!({
                            "name": tool_clone.name.clone(),
                            "description": tool_clone.description.clone(),
                            "input_schema": tool_clone.input_schema.clone(),
                            "version": tool_clone.version as i64,
                            "status": tool_clone.status.as_str(),
                        }),
                    );
                }
            }
        }

        let tool = &tool_clone;

        let before_state = json!({
            "description": tool.description.clone(),
            "input_schema": tool.input_schema.clone(),
            "version": tool.version as i64,
            "status": tool.status.as_str(),
        });

        let mut changes = Vec::new();
        let mut after_state = before_state.clone();
        after_state.insert("version", (tool.version + 1) as i64);

        if let Some(desc) = new_description {
            if !desc.is_empty() && desc != tool.description {
                changes.push(format!(
                    "Description updated: '{}' -> '{}'",
                    tool.description, desc
                ));
                after_state.insert("description", desc);
            }
        }

        if let Some(schema) = new_input_schema {
            if schema != tool.input_schema {
                changes.push("Input schema updated".to_string());
                after_state.insert("input_schema", schema);
            }
        }

        if changes.is_empty() {
            changes.push("No changes detected".to_string());
        }

        let seq = DIFF_COUNTER.fetch_add(1, Ordering::Relaxed);
        let diff_id = format!(
            "diff-{:08x}",
            ((now_epoch_secs() * 1000.0) as u64 ^ (seq * 0x9e3779b9)) as u32
        );

        let diff = ConfigDiff {
            diff_id: diff_id.clone(),
            server_name: server_name.to_string(),
            tool_name: tool_name.to_string(),
            previous_version: tool.version,
            proposed_version: tool.version + 1,
            before_state,
            after_state,
            changes_summary: changes,
            proposed_by: proposed_by.to_string(),
            created_at: now_epoch_secs(),
            approved: false,
            approved_by: None,
            approved_at: None,
        };

        guard.diffs.insert(diff_id, diff.clone());
        if let Some(store) = &self.store {
            store.save_diff(&diff);
        }

        Ok(diff)
    }

    pub fn approve_diff(&self, diff_id: &str, approved_by: &str) -> bool {
        let mut guard = self.inner.write().unwrap();
        let diff = match guard.diffs.get(diff_id).cloned() {
            Some(d) if !d.approved => d,
            _ => return false,
        };

        let (updated_server, updated_diff) = {
            let server = match guard.servers.get_mut(&diff.server_name) {
                Some(s) => s,
                None => return false,
            };
            let tool = match server.tools.get_mut(&diff.tool_name) {
                Some(t) => t,
                None => return false,
            };

            if let Some(desc) = diff.after_state.get("description").and_then(|x| x.as_str()) {
                tool.description = desc.to_string();
            }
            if let Some(schema) = diff.after_state.get("input_schema") {
                tool.input_schema = schema.clone();
            }
            tool.version = diff.proposed_version;
            let t_name = tool.name.clone();
            let t_ver = tool.version;

            let action = format!("Tool '{}' updated to v{} by {}", t_name, t_ver, approved_by);
            server.version += 1;
            server.history.push(HistoryEntry {
                action: action.clone(),
                version: server.version,
                time: now_epoch_secs(),
            });
            let mut snap = Self::snapshot_server(server);
            snap.insert("action", action);
            let srv_clone = server.clone();

            guard
                .version_history
                .entry(diff.server_name.clone())
                .or_default()
                .push(snap);

            let d_mut = guard.diffs.get_mut(diff_id).unwrap();
            d_mut.approved = true;
            d_mut.approved_by = Some(approved_by.to_string());
            d_mut.approved_at = Some(now_epoch_secs());
            (srv_clone, d_mut.clone())
        };

        if let Some(store) = &self.store {
            store.save_server(&updated_server);
            store.save_diff(&updated_diff);
        }
        true
    }

    pub fn rollback_tool(&self, server_name: &str, tool_name: &str, target_version: u32) -> bool {
        let mut guard = self.inner.write().unwrap();
        let mut found_candidate: Option<(String, JsonValue)> = None;

        if let Some(history) = guard.version_history.get(server_name) {
            for snap in history.iter().rev() {
                if let Some(tools_snap) = snap.get("tools") {
                    if let Some(cand) = tools_snap.get(tool_name) {
                        if cand.get("version").and_then(|x| x.as_u64())
                            == Some(target_version as u64)
                        {
                            let desc = cand
                                .get("description")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_string();
                            let schema = cand
                                .get("input_schema")
                                .cloned()
                                .unwrap_or_else(JsonValue::object);
                            found_candidate = Some((desc, schema));
                            break;
                        }
                    }
                }
            }
        }

        // Also check diff before_states if history came from disk restart
        if found_candidate.is_none() {
            for d in guard.diffs.values() {
                if d.server_name == server_name
                    && d.tool_name == tool_name
                    && d.previous_version == target_version
                {
                    let desc = d
                        .before_state
                        .get("description")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let schema = d
                        .before_state
                        .get("input_schema")
                        .cloned()
                        .unwrap_or_else(JsonValue::object);
                    found_candidate = Some((desc, schema));
                    break;
                }
            }
        }

        let (desc, schema) = match found_candidate {
            Some(pair) => pair,
            None => return false,
        };

        let updated_server = {
            let server = match guard.servers.get_mut(server_name) {
                Some(s) => s,
                None => return false,
            };
            let tool = match server.tools.get_mut(tool_name) {
                Some(t) => t,
                None => return false,
            };
            tool.description = desc;
            tool.input_schema = schema;
            tool.version = target_version;
            let action = format!("Tool '{}' rolled back to v{}", tool_name, target_version);
            server.version += 1;
            server.history.push(HistoryEntry {
                action: action.clone(),
                version: server.version,
                time: now_epoch_secs(),
            });
            let mut snap = Self::snapshot_server(server);
            snap.insert("action", action);
            let srv_clone = server.clone();
            guard
                .version_history
                .entry(server_name.to_string())
                .or_default()
                .push(snap);
            srv_clone
        };

        if let Some(store) = &self.store {
            store.save_server(&updated_server);
        }
        true
    }

    pub fn toggle_tool_status(
        &self,
        server_name: &str,
        tool_name: &str,
        new_status: ToolStatus,
    ) -> bool {
        let mut guard = self.inner.write().unwrap();
        let updated_server = {
            let server = match guard.servers.get_mut(server_name) {
                Some(s) => s,
                None => return false,
            };
            let tool = match server.tools.get_mut(tool_name) {
                Some(t) => t,
                None => return false,
            };
            tool.status = new_status;
            let action = format!(
                "Tool '{}' status changed to {}",
                tool_name,
                new_status.as_str()
            );
            server.version += 1;
            server.history.push(HistoryEntry {
                action: action.clone(),
                version: server.version,
                time: now_epoch_secs(),
            });
            let mut snap = Self::snapshot_server(server);
            snap.insert("action", action);
            let srv_clone = server.clone();
            guard
                .version_history
                .entry(server_name.to_string())
                .or_default()
                .push(snap);
            srv_clone
        };

        if let Some(store) = &self.store {
            store.save_server(&updated_server);
        }
        true
    }

    pub fn toggle_server_status(&self, server_name: &str, new_status: ToolStatus) -> bool {
        let mut guard = self.inner.write().unwrap();
        let updated_server = {
            let server = match guard.servers.get_mut(server_name) {
                Some(s) => s,
                None => return false,
            };
            server.status = new_status;
            let action = format!("Server status changed to {}", new_status.as_str());
            server.version += 1;
            server.history.push(HistoryEntry {
                action: action.clone(),
                version: server.version,
                time: now_epoch_secs(),
            });
            let mut snap = Self::snapshot_server(server);
            snap.insert("action", action);
            let srv_clone = server.clone();
            guard
                .version_history
                .entry(server_name.to_string())
                .or_default()
                .push(snap);
            srv_clone
        };

        if let Some(store) = &self.store {
            store.save_server(&updated_server);
        }
        true
    }

    pub fn get_diff(&self, diff_id: &str) -> Option<ConfigDiff> {
        let guard = self.inner.read().unwrap();
        guard.diffs.get(diff_id).cloned()
    }

    pub fn list_pending_diffs(&self, server_name: Option<&str>) -> Vec<ConfigDiff> {
        let guard = self.inner.read().unwrap();
        guard
            .diffs
            .values()
            .filter(|d| !d.approved && server_name.is_none_or(|sn| d.server_name == sn))
            .cloned()
            .collect()
    }

    pub fn save_suspended_frame(&self, frame: SuspendedExecutionFrame) {
        {
            let mut guard = self.inner.write().unwrap();
            guard
                .suspended_frames
                .insert(frame.execution_id.clone(), frame.clone());
        }
        if let Some(store) = &self.store {
            store.save_suspended_frame(&frame);
        }
    }

    pub fn get_suspended_frame(&self, execution_id: &str) -> Option<SuspendedExecutionFrame> {
        if let Some(store) = &self.store {
            if let Some(f) = store.get_suspended_frame(execution_id) {
                return Some(f);
            }
        }
        let guard = self.inner.read().unwrap();
        guard.suspended_frames.get(execution_id).cloned()
    }

    pub fn delete_suspended_frame(&self, execution_id: &str) -> Option<SuspendedExecutionFrame> {
        let removed_mem = {
            let mut guard = self.inner.write().unwrap();
            guard.suspended_frames.remove(execution_id)
        };
        if let Some(store) = &self.store {
            if let Some(f) = store.delete_suspended_frame(execution_id) {
                return Some(f);
            }
        }
        removed_mem
    }

    pub fn list_suspended_frames(&self) -> Vec<SuspendedExecutionFrame> {
        if let Some(store) = &self.store {
            let frames = store.list_suspended_frames();
            if !frames.is_empty() {
                return frames;
            }
        }
        let guard = self.inner.read().unwrap();
        guard.suspended_frames.values().cloned().collect()
    }

    fn snapshot_server(server: &ServerRecord) -> JsonValue {
        let mut tools_obj = JsonValue::object();
        for (t_name, t) in &server.tools {
            tools_obj.insert(
                t_name.clone(),
                json!({
                    "name": t.name.clone(),
                    "description": t.description.clone(),
                    "input_schema": t.input_schema.clone(),
                    "version": t.version as i64,
                    "status": t.status.as_str(),
                }),
            );
        }
        json!({
            "version": server.version as i64,
            "timestamp": now_epoch_secs(),
            "status": server.status.as_str(),
            "tools": tools_obj,
        })
    }
}
