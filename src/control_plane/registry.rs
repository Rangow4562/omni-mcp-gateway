//! MCP Registry - Single source of truth for discovery, ownership, versioning, diff approval, and enablement.
//! Supports both high-throughput concurrent in-memory state and durable WAL storage.

use crate::data_plane::codemode::CodeModeTypeGenerator;
use crate::json::JsonValue;
use crate::logging::now_epoch_secs;
use crate::models::{
    ChangeOrigin, ConfigDiff, FileBlobRecord, HistoryEntry, McpTaskRecord, McpTaskStatus,
    OAuthFlowRecord, OAuthTokenRecord, PendingUpstreamCallback, ProtocolType, SchemaSource,
    ServerRecord, SkillRecord, SuspendedExecutionFrame, ToolGroupRecord, ToolRecord, ToolStatus,
};
use crate::storage::{RegistryStorageBackend, WalRegistryStore};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

static DIFF_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq)]
pub struct IdentifierCollision {
    pub sanitized: String,
    pub first_raw: String,
    pub second_raw: String,
}

impl IdentifierCollision {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "error": "IDENTIFIER_COLLISION",
            "sanitized": self.sanitized.clone(),
            "first_raw": self.first_raw.clone(),
            "second_raw": self.second_raw.clone(),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExternalSyncResult {
    pub added: usize,
    pub updated: usize,
    pub retired: usize,
    pub unchanged: usize,
    pub pending_human_conflicts: usize,
}

impl ExternalSyncResult {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "added": self.added as i64,
            "updated": self.updated as i64,
            "retired": self.retired as i64,
            "unchanged": self.unchanged as i64,
            "pending_human_conflicts": self.pending_human_conflicts as i64,
        })
    }
}

struct RegistryInner {
    servers: BTreeMap<String, ServerRecord>,
    diffs: BTreeMap<String, ConfigDiff>,
    suspended_frames: BTreeMap<String, SuspendedExecutionFrame>,
    tool_groups: BTreeMap<String, ToolGroupRecord>,
    skills: BTreeMap<String, SkillRecord>,
    mcp_tasks: BTreeMap<String, McpTaskRecord>,
    oauth_flows: BTreeMap<String, OAuthFlowRecord>,
    oauth_tokens: BTreeMap<String, OAuthTokenRecord>,
    upstream_callbacks: BTreeMap<String, PendingUpstreamCallback>,
    file_blobs: BTreeMap<String, FileBlobRecord>,
    version_history: HashMap<String, Vec<JsonValue>>,
}

#[derive(Clone)]
pub struct MCPRegistry {
    inner: Arc<RwLock<RegistryInner>>,
    store: Option<Arc<dyn RegistryStorageBackend>>,
    reload_counter: Arc<AtomicU64>,
}

impl Default for MCPRegistry {
    fn default() -> Self {
        Self::new(None)
    }
}

impl MCPRegistry {
    pub fn new(persistent_store: Option<Arc<WalRegistryStore>>) -> Self {
        let backend: Option<Arc<dyn RegistryStorageBackend>> =
            persistent_store.map(|s| s as Arc<dyn RegistryStorageBackend>);
        Self::with_optional_backend(backend)
    }

    pub fn with_backend(backend: Arc<dyn RegistryStorageBackend>) -> Self {
        Self::with_optional_backend(Some(backend))
    }

    fn with_optional_backend(backend: Option<Arc<dyn RegistryStorageBackend>>) -> Self {
        let mut servers = BTreeMap::new();
        let mut diffs = BTreeMap::new();
        let mut suspended_frames = BTreeMap::new();
        let mut tool_groups = BTreeMap::new();
        let mut skills = BTreeMap::new();
        let mut mcp_tasks = BTreeMap::new();
        let oauth_flows = BTreeMap::new();
        let oauth_tokens = BTreeMap::new();
        let mut upstream_callbacks = BTreeMap::new();
        let file_blobs = BTreeMap::new();
        let mut version_history: HashMap<String, Vec<JsonValue>> = HashMap::new();

        if let Some(store) = &backend {
            for s in store.load_all_servers() {
                let snap = Self::snapshot_server(&s);
                version_history.insert(s.name.clone(), vec![snap]);
                servers.insert(s.name.clone(), s);
            }
            for d in store.load_all_diffs() {
                diffs.insert(d.diff_id.clone(), d);
            }
            for f in store.load_all_suspended_frames() {
                suspended_frames.insert(f.execution_id.clone(), f);
            }
            for g in store.load_all_tool_groups() {
                tool_groups.insert(g.slug.clone(), g);
            }
            for sk in store.load_all_skills() {
                skills.insert(sk.slug.clone(), sk);
            }
            for tk in store.load_all_mcp_tasks() {
                mcp_tasks.insert(tk.task_id.clone(), tk);
            }
            for cb in store.load_all_upstream_callbacks() {
                upstream_callbacks.insert(cb.callback_id.clone(), cb);
            }
        }

        Self {
            inner: Arc::new(RwLock::new(RegistryInner {
                servers,
                diffs,
                suspended_frames,
                tool_groups,
                skills,
                mcp_tasks,
                oauth_flows,
                oauth_tokens,
                upstream_callbacks,
                file_blobs,
                version_history,
            })),
            store: backend,
            reload_counter: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn backend(&self) -> Option<Arc<dyn RegistryStorageBackend>> {
        self.store.clone()
    }

    pub fn reload_count(&self) -> u64 {
        self.reload_counter.load(Ordering::Relaxed)
    }

    /// Reloads servers, diffs, suspended frames, tool groups, and skills from the backing
    /// distributed storage engine (invoked on PostgreSQL `LISTEN omni_registry_changed`).
    pub fn reload_from_backend(&self) {
        self.reload_counter.fetch_add(1, Ordering::Relaxed);
        let Some(store) = &self.store else {
            return;
        };
        let loaded_servers = store.load_all_servers();
        let loaded_diffs = store.load_all_diffs();
        let loaded_frames = store.load_all_suspended_frames();
        let loaded_groups = store.load_all_tool_groups();
        let loaded_skills = store.load_all_skills();
        let mut guard = self.inner.write().unwrap();
        guard.servers.clear();
        for s in loaded_servers {
            guard.servers.insert(s.name.clone(), s);
        }
        guard.diffs.clear();
        for d in loaded_diffs {
            guard.diffs.insert(d.diff_id.clone(), d);
        }
        guard.suspended_frames.clear();
        for f in loaded_frames {
            guard.suspended_frames.insert(f.execution_id.clone(), f);
        }
        guard.tool_groups.clear();
        for g in loaded_groups {
            guard.tool_groups.insert(g.slug.clone(), g);
        }
        guard.skills.clear();
        for sk in loaded_skills {
            guard.skills.insert(sk.slug.clone(), sk);
        }
    }

    pub fn preserve_tool_governance_fields(new_tool: &mut ToolRecord, existing: &ToolRecord) {
        if existing.status == ToolStatus::Enabled {
            new_tool.status = ToolStatus::Enabled;
        } else if existing.status == ToolStatus::Disabled {
            new_tool.status = ToolStatus::Disabled;
        }
        if new_tool.charter_policy.is_none() && existing.charter_policy.is_some() {
            new_tool.charter_policy = existing.charter_policy.clone();
        }
        if !new_tool.is_mutation && existing.is_mutation {
            new_tool.is_mutation = true;
        }
        if new_tool.compensation_spec.is_none() && existing.compensation_spec.is_some() {
            new_tool.compensation_spec = existing.compensation_spec.clone();
        }
        if new_tool.canary_version.is_none() && existing.canary_version.is_some() {
            new_tool.canary_version = existing.canary_version;
            new_tool.canary_weight_pct = existing.canary_weight_pct;
        }
        if new_tool.fallback_tool.is_none() && existing.fallback_tool.is_some() {
            new_tool.fallback_tool = existing.fallback_tool.clone();
        }
    }

    pub fn check_identifier_collisions(
        &self,
        candidate_server_name: &str,
        candidate_tools: &[ToolRecord],
    ) -> Result<(), IdentifierCollision> {
        let guard = self.inner.read().unwrap();
        let cand_slug = candidate_server_name
            .strip_prefix("external-mcp:")
            .unwrap_or(candidate_server_name);
        let cand_srv_san = CodeModeTypeGenerator::sanitize_ident(cand_slug);

        // Level 1: Server-level collision across all servers in registry
        for existing in guard.servers.values() {
            if existing.name != candidate_server_name {
                let exist_slug = existing
                    .name
                    .strip_prefix("external-mcp:")
                    .unwrap_or(&existing.name);
                let existing_san = CodeModeTypeGenerator::sanitize_ident(exist_slug);
                if existing_san == cand_srv_san {
                    return Err(IdentifierCollision {
                        sanitized: cand_srv_san,
                        first_raw: existing.name.clone(),
                        second_raw: candidate_server_name.to_string(),
                    });
                }
            }
        }

        // Level 2: Tool-level collision within server
        let mut seen_tools: BTreeMap<String, String> = BTreeMap::new();
        for tool in candidate_tools {
            let tool_san = CodeModeTypeGenerator::sanitize_ident(&tool.name);
            if let Some(first_raw) = seen_tools.get(&tool_san) {
                return Err(IdentifierCollision {
                    sanitized: tool_san,
                    first_raw: first_raw.clone(),
                    second_raw: tool.name.clone(),
                });
            }
            seen_tools.insert(tool_san, tool.name.clone());

            // Level 3: Property-level collision within tool input_schema
            if let Some(props) = tool
                .input_schema
                .get("properties")
                .and_then(|p| p.as_object())
            {
                let mut seen_props: BTreeMap<String, String> = BTreeMap::new();
                for (prop_name, _) in props {
                    let prop_san = CodeModeTypeGenerator::sanitize_ident(prop_name);
                    if let Some(first_prop) = seen_props.get(&prop_san) {
                        return Err(IdentifierCollision {
                            sanitized: prop_san,
                            first_raw: first_prop.clone(),
                            second_raw: prop_name.clone(),
                        });
                    }
                    seen_props.insert(prop_san, prop_name.clone());
                }
            }
        }

        Ok(())
    }

    pub fn register_server(&self, server: ServerRecord) -> ServerRecord {
        let mut guard = self.inner.write().unwrap();
        let target = if let Some(existing) = guard.servers.get_mut(&server.name) {
            for (t_name, mut tool) in server.tools {
                if let Some(prev) = existing.tools.get(&t_name) {
                    Self::preserve_tool_governance_fields(&mut tool, prev);
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

    /// Update external sync telemetry (`health`, `status`, `external_resources`, `external_prompts`,
    /// `heartbeat_timestamp`) for an existing server and persist to WAL/snapshot.
    pub fn update_server_sync_state(
        &self,
        name: &str,
        health: crate::models::ServerHealth,
        status: Option<ToolStatus>,
        external_resources: Option<Vec<crate::models::ExternalResourceRecord>>,
        external_prompts: Option<Vec<crate::models::ExternalPromptRecord>>,
        heartbeat_timestamp: Option<f64>,
    ) {
        let updated = {
            let mut guard = self.inner.write().unwrap();
            if let Some(existing) = guard.servers.get_mut(name) {
                existing.health = health;
                if let Some(st) = status {
                    existing.status = st;
                }
                if let Some(res) = external_resources {
                    existing.external_resources = res;
                }
                if let Some(prm) = external_prompts {
                    existing.external_prompts = prm;
                }
                if heartbeat_timestamp.is_some() {
                    existing.heartbeat_timestamp = heartbeat_timestamp;
                }
                Some(existing.clone())
            } else {
                None
            }
        };
        if let Some(srv) = updated {
            if let Some(store) = &self.store {
                store.save_server(&srv);
            }
        }
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
            .filter(|s| filter_status.map_or(true, |st| s.status == st))
            .cloned()
            .collect()
    }

    pub fn list_all_tools(&self, status: Option<ToolStatus>) -> Vec<(ServerRecord, ToolRecord)> {
        let guard = self.inner.read().unwrap();
        let mut out = Vec::new();
        for s in guard.servers.values() {
            for t in s.tools.values() {
                if status.map_or(true, |st| t.status == st) {
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
            origin: ChangeOrigin::Human,
        };

        guard.diffs.insert(diff_id, diff.clone());
        if let Some(store) = &self.store {
            store.save_diff(&diff);
        }

        Ok(diff)
    }

    pub fn approve_diff(&self, diff_id: &str, approved_by: &str) -> Result<bool, String> {
        let mut guard = self.inner.write().unwrap();
        let diff = match guard.diffs.get(diff_id).cloned() {
            Some(d) if !d.approved => d,
            _ => return Ok(false),
        };

        let (updated_server, updated_diff) = {
            let server = match guard.servers.get_mut(&diff.server_name) {
                Some(s) => s,
                None => return Ok(false),
            };
            let tool = match server.tools.get_mut(&diff.tool_name) {
                Some(t) => t,
                None => return Ok(false),
            };

            // CAS version check
            if tool.version != diff.previous_version {
                return Err(
                    "CONFLICT: Tool version has advanced since diff was proposed".to_string(),
                );
            }

            if let Some(desc) = diff.after_state.get("description").and_then(|x| x.as_str()) {
                tool.description = desc.to_string();
            }
            if let Some(schema) = diff.after_state.get("input_schema") {
                tool.input_schema = schema.clone();
            }
            tool.version = diff.proposed_version;
            tool.schema_source = SchemaSource::Human;
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
        Ok(true)
    }

    pub fn apply_external_sync(
        &self,
        server_name: &str,
        fetched_tools: Vec<ToolRecord>,
    ) -> Result<ExternalSyncResult, String> {
        let mut guard = self.inner.write().unwrap();
        let server = guard
            .servers
            .get_mut(server_name)
            .ok_or_else(|| format!("Server '{}' not found in registry", server_name))?;

        let now = now_epoch_secs();
        let mut added = 0;
        let mut updated = 0;
        let mut retired = 0;
        let mut unchanged = 0;
        let mut pending_human_conflicts = 0;

        let mut incoming_map: BTreeMap<String, ToolRecord> = BTreeMap::new();
        for t in fetched_tools {
            incoming_map.insert(t.name.clone(), t);
        }

        let mut diffs_to_save: Vec<ConfigDiff> = Vec::new();

        // 1. Process incoming tools (Added & Existing)
        for (t_name, incoming_tool) in incoming_map.iter() {
            if let Some(existing_tool) = server.tools.get_mut(t_name) {
                // Existing tool
                if existing_tool.status == ToolStatus::Unavailable {
                    // Revival policy: if schema and description match, restore prior_status; else PendingReview
                    let schema_match = existing_tool.input_schema == incoming_tool.input_schema
                        && existing_tool.description == incoming_tool.description;
                    let prior_status = existing_tool
                        .metadata
                        .get("prior_status")
                        .and_then(|x| x.as_str())
                        .map(ToolStatus::from_str)
                        .unwrap_or(ToolStatus::Disabled);

                    let before_state = existing_tool.to_json();
                    if schema_match {
                        existing_tool.status = prior_status;
                    } else {
                        existing_tool.status = ToolStatus::PendingReview;
                        existing_tool.description = incoming_tool.description.clone();
                        existing_tool.input_schema = incoming_tool.input_schema.clone();
                    }
                    existing_tool.retired_at = None;
                    existing_tool.version += 1;
                    existing_tool.last_seen_in_sync = Some(now);

                    let after_state = existing_tool.to_json();
                    let seq = DIFF_COUNTER.fetch_add(1, Ordering::Relaxed);
                    let diff_id = format!(
                        "diff-{:08x}",
                        ((now * 1000.0) as u64 ^ (seq * 0x9e3779b9)) as u32
                    );
                    let diff = ConfigDiff {
                        diff_id,
                        server_name: server_name.to_string(),
                        tool_name: t_name.clone(),
                        previous_version: existing_tool.version - 1,
                        proposed_version: existing_tool.version,
                        before_state,
                        after_state,
                        changes_summary: vec![format!("Revived tool '{}'", t_name)],
                        proposed_by: "external_sync".to_string(),
                        created_at: now,
                        approved: true,
                        approved_by: Some("external_sync".to_string()),
                        approved_at: Some(now),
                        origin: ChangeOrigin::ExternalSync,
                    };
                    diffs_to_save.push(diff);
                    updated += 1;
                } else {
                    // Existing active tool
                    let desc_identical = existing_tool.description == incoming_tool.description;
                    let schema_identical = existing_tool.input_schema == incoming_tool.input_schema;

                    if desc_identical && schema_identical {
                        existing_tool.last_seen_in_sync = Some(now);
                        unchanged += 1;
                    } else if existing_tool.schema_source == SchemaSource::Upstream {
                        // Upstream source: auto-apply change and write auto-approved diff
                        let before_state = existing_tool.to_json();
                        existing_tool.description = incoming_tool.description.clone();
                        existing_tool.input_schema = incoming_tool.input_schema.clone();
                        existing_tool.version += 1;
                        existing_tool.last_seen_in_sync = Some(now);
                        let after_state = existing_tool.to_json();

                        let seq = DIFF_COUNTER.fetch_add(1, Ordering::Relaxed);
                        let diff_id = format!(
                            "diff-{:08x}",
                            ((now * 1000.0) as u64 ^ (seq * 0x9e3779b9)) as u32
                        );
                        let diff = ConfigDiff {
                            diff_id,
                            server_name: server_name.to_string(),
                            tool_name: t_name.clone(),
                            previous_version: existing_tool.version - 1,
                            proposed_version: existing_tool.version,
                            before_state,
                            after_state,
                            changes_summary: vec![format!(
                                "Upstream schema sync for tool '{}'",
                                t_name
                            )],
                            proposed_by: "external_sync".to_string(),
                            created_at: now,
                            approved: true,
                            approved_by: Some("external_sync".to_string()),
                            approved_at: Some(now),
                            origin: ChangeOrigin::ExternalSync,
                        };
                        diffs_to_save.push(diff);
                        updated += 1;
                    } else {
                        // Human source: DO NOT overwrite! Create unapproved diff in review queue
                        existing_tool.last_seen_in_sync = Some(now);
                        let before_state = existing_tool.to_json();
                        let mut prospective_after = incoming_tool.to_json();
                        prospective_after.insert("version", (existing_tool.version + 1) as i64);

                        let seq = DIFF_COUNTER.fetch_add(1, Ordering::Relaxed);
                        let diff_id = format!(
                            "diff-{:08x}",
                            ((now * 1000.0) as u64 ^ (seq * 0x9e3779b9)) as u32
                        );
                        let diff = ConfigDiff {
                            diff_id,
                            server_name: server_name.to_string(),
                            tool_name: t_name.clone(),
                            previous_version: existing_tool.version,
                            proposed_version: existing_tool.version + 1,
                            before_state,
                            after_state: prospective_after,
                            changes_summary: vec![format!(
                                "Upstream conflict with human-edited tool '{}'",
                                t_name
                            )],
                            proposed_by: "external_sync".to_string(),
                            created_at: now,
                            approved: false,
                            approved_by: None,
                            approved_at: None,
                            origin: ChangeOrigin::ExternalSync,
                        };
                        diffs_to_save.push(diff);
                        pending_human_conflicts += 1;
                    }
                }
            } else {
                // Net-new tool
                let mut new_tool = incoming_tool.clone();
                new_tool.status = ToolStatus::Disabled;
                new_tool.version = 1;
                new_tool.protocol = ProtocolType::ExternalMcp;
                new_tool.schema_source = SchemaSource::Upstream;
                new_tool.last_seen_in_sync = Some(now);

                let after_state = new_tool.to_json();
                let seq = DIFF_COUNTER.fetch_add(1, Ordering::Relaxed);
                let diff_id = format!(
                    "diff-{:08x}",
                    ((now * 1000.0) as u64 ^ (seq * 0x9e3779b9)) as u32
                );
                let diff = ConfigDiff {
                    diff_id,
                    server_name: server_name.to_string(),
                    tool_name: t_name.clone(),
                    previous_version: 0,
                    proposed_version: 1,
                    before_state: JsonValue::Null,
                    after_state,
                    changes_summary: vec![format!("Discovered new tool '{}'", t_name)],
                    proposed_by: "external_sync".to_string(),
                    created_at: now,
                    approved: true,
                    approved_by: Some("external_sync".to_string()),
                    approved_at: Some(now),
                    origin: ChangeOrigin::ExternalSync,
                };
                diffs_to_save.push(diff);
                server.tools.insert(t_name.clone(), new_tool);
                added += 1;
            }
        }

        // 2. Process missing tools (Retirement)
        for (t_name, existing_tool) in server.tools.iter_mut() {
            if !incoming_map.contains_key(t_name) && existing_tool.status != ToolStatus::Unavailable
            {
                let prior_status = existing_tool.status;
                let before_state = existing_tool.to_json();
                existing_tool.status = ToolStatus::Unavailable;
                existing_tool.retired_at = Some(now);
                existing_tool
                    .metadata
                    .insert("prior_status", JsonValue::from(prior_status.as_str()));
                existing_tool.version += 1;
                let after_state = existing_tool.to_json();

                let seq = DIFF_COUNTER.fetch_add(1, Ordering::Relaxed);
                let diff_id = format!(
                    "diff-{:08x}",
                    ((now * 1000.0) as u64 ^ (seq * 0x9e3779b9)) as u32
                );
                let diff = ConfigDiff {
                    diff_id,
                    server_name: server_name.to_string(),
                    tool_name: t_name.clone(),
                    previous_version: existing_tool.version - 1,
                    proposed_version: existing_tool.version,
                    before_state,
                    after_state,
                    changes_summary: vec![format!("Retired missing upstream tool '{}'", t_name)],
                    proposed_by: "external_sync".to_string(),
                    created_at: now,
                    approved: true,
                    approved_by: Some("external_sync".to_string()),
                    approved_at: Some(now),
                    origin: ChangeOrigin::ExternalSync,
                };
                diffs_to_save.push(diff);
                retired += 1;
            }
        }

        // 3. Finalize server history and clone
        server.version += 1;
        let sync_summary = format!(
            "External sync: +{} ~{} -{} ={} !{}",
            added, updated, retired, unchanged, pending_human_conflicts
        );
        server.history.push(HistoryEntry {
            action: sync_summary.clone(),
            version: server.version,
            time: now,
        });

        let mut snap = Self::snapshot_server(server);
        snap.insert("action", sync_summary);
        let srv_clone = server.clone();

        // 4. Persist diffs and history into registry state
        for diff in &diffs_to_save {
            guard.diffs.insert(diff.diff_id.clone(), diff.clone());
        }
        guard
            .version_history
            .entry(server_name.to_string())
            .or_default()
            .push(snap);

        if let Some(store) = &self.store {
            for diff in &diffs_to_save {
                store.save_diff(diff);
            }
            store.save_server(&srv_clone);
        }

        Ok(ExternalSyncResult {
            added,
            updated,
            retired,
            unchanged,
            pending_human_conflicts,
        })
    }

    pub fn remove_server(&self, name: &str) -> Option<ServerRecord> {
        let mut guard = self.inner.write().unwrap();
        let removed = guard.servers.remove(name);
        guard.version_history.remove(name);
        if let Some(store) = &self.store {
            store.delete_server(name);
        }
        removed
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
            .filter(|d| !d.approved && server_name.map_or(true, |sn| d.server_name == sn))
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

    /// Atomic CAS under write lock: checks that frame is not yet consumed, marks consumed with approver.
    pub fn consume_frame(
        &self,
        execution_id: &str,
        approver: &str,
    ) -> Result<SuspendedExecutionFrame, &'static str> {
        if let Some(store) = &self.store {
            let res = store.consume_frame_cas(execution_id, approver).map_err(|e| {
                if e.contains("already been consumed") || e.contains("single-use") {
                    "Approval has already been consumed (single-use violated)"
                } else if e.contains("separation") || e.contains("Separation") {
                    "Approval denied: Requester cannot approve their own mutation (separation of duties)"
                } else if e.contains("expired") {
                    "Approval frame has expired"
                } else {
                    "Approval continuation frame not found or expired"
                }
            })?;
            let mut guard = self.inner.write().unwrap();
            guard
                .suspended_frames
                .insert(execution_id.to_string(), res.clone());
            return Ok(res);
        }
        let mut guard = self.inner.write().unwrap();
        let frame = match guard.suspended_frames.get_mut(execution_id) {
            Some(f) => f,
            None => return Err("Approval continuation frame not found or expired"),
        };
        if frame.is_consumed {
            return Err("Approval has already been consumed (single-use violated)");
        }
        frame.is_consumed = true;
        frame.approver_identity = Some(approver.to_string());
        Ok(frame.clone())
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
            return store.load_all_suspended_frames();
        }
        let guard = self.inner.read().unwrap();
        guard
            .suspended_frames
            .values()
            .filter(|f| !f.is_consumed)
            .cloned()
            .collect()
    }

    pub fn save_tool_group(&self, group: ToolGroupRecord) -> ToolGroupRecord {
        {
            let mut guard = self.inner.write().unwrap();
            guard.tool_groups.insert(group.slug.clone(), group.clone());
        }
        if let Some(store) = &self.store {
            store.save_tool_group(&group);
        }
        group
    }

    pub fn get_tool_group(&self, slug: &str) -> Option<ToolGroupRecord> {
        let guard = self.inner.read().unwrap();
        guard.tool_groups.get(slug).cloned()
    }

    pub fn list_tool_groups(&self) -> Vec<ToolGroupRecord> {
        let guard = self.inner.read().unwrap();
        guard.tool_groups.values().cloned().collect()
    }

    pub fn delete_tool_group(&self, slug: &str) -> Option<ToolGroupRecord> {
        let removed = {
            let mut guard = self.inner.write().unwrap();
            guard.tool_groups.remove(slug)
        };
        if let Some(store) = &self.store {
            let _ = store.delete_tool_group(slug);
        }
        removed
    }

    // --- v6.8.0 Skills over MCP ---

    pub fn save_skill(&self, mut skill: SkillRecord) -> Result<SkillRecord, String> {
        skill.validate()?;
        {
            let mut guard = self.inner.write().unwrap();
            guard.skills.insert(skill.slug.clone(), skill.clone());
        }
        if let Some(store) = &self.store {
            store.save_skill(&skill);
        }
        Ok(skill)
    }

    pub fn get_skill(&self, slug: &str) -> Option<SkillRecord> {
        let guard = self.inner.read().unwrap();
        guard.skills.get(slug).cloned()
    }

    pub fn list_skills(&self) -> Vec<SkillRecord> {
        let guard = self.inner.read().unwrap();
        guard.skills.values().cloned().collect()
    }

    pub fn delete_skill(&self, slug: &str) -> Option<SkillRecord> {
        let removed = {
            let mut guard = self.inner.write().unwrap();
            guard.skills.remove(slug)
        };
        if let Some(store) = &self.store {
            let _ = store.delete_skill(slug);
        }
        removed
    }

    // --- v6.8.0 HA MCP Tasks ---

    pub fn save_mcp_task(&self, task: &McpTaskRecord) {
        {
            let mut guard = self.inner.write().unwrap();
            guard.mcp_tasks.insert(task.task_id.clone(), task.clone());
        }
        if let Some(store) = &self.store {
            store.save_mcp_task(task);
        }
    }

    pub fn get_mcp_task(&self, task_id: &str) -> Option<McpTaskRecord> {
        if let Some(store) = &self.store {
            if let Some(t) = store.get_mcp_task(task_id) {
                return Some(t);
            }
        }
        let guard = self.inner.read().unwrap();
        guard.mcp_tasks.get(task_id).cloned()
    }

    pub fn list_mcp_tasks(&self) -> Vec<McpTaskRecord> {
        if let Some(store) = &self.store {
            return store.load_all_mcp_tasks();
        }
        let guard = self.inner.read().unwrap();
        guard.mcp_tasks.values().cloned().collect()
    }

    pub fn update_mcp_task_cas(
        &self,
        task_id: &str,
        expected_status: McpTaskStatus,
        updated: &McpTaskRecord,
    ) -> Result<McpTaskRecord, String> {
        if let Some(store) = &self.store {
            let res = store.update_mcp_task_cas(task_id, expected_status, updated)?;
            let mut guard = self.inner.write().unwrap();
            guard.mcp_tasks.insert(task_id.to_string(), res.clone());
            return Ok(res);
        }
        let mut guard = self.inner.write().unwrap();
        let existing = guard
            .mcp_tasks
            .get(task_id)
            .ok_or_else(|| format!("Task '{}' not found", task_id))?;
        if existing.status != expected_status {
            return Err(format!(
                "Task '{}' status mismatch: expected '{}', found '{}'",
                task_id,
                expected_status.as_str(),
                existing.status.as_str()
            ));
        }
        guard.mcp_tasks.insert(task_id.to_string(), updated.clone());
        Ok(updated.clone())
    }

    pub fn request_mcp_task_cancel(&self, task_id: &str) -> Option<McpTaskRecord> {
        if let Some(store) = &self.store {
            if let Some(t) = store.request_mcp_task_cancel(task_id) {
                let mut guard = self.inner.write().unwrap();
                guard.mcp_tasks.insert(task_id.to_string(), t.clone());
                return Some(t);
            }
        }
        let mut guard = self.inner.write().unwrap();
        let mut task = guard.mcp_tasks.get(task_id)?.clone();
        task.cancel_requested = true;
        if !task.status.is_terminal() {
            task.status = McpTaskStatus::Cancelled;
            task.status_message = Some("Cancelled by client".to_string());
        }
        task.updated_at = now_epoch_secs();
        guard.mcp_tasks.insert(task_id.to_string(), task.clone());
        Some(task)
    }

    pub fn purge_expired_mcp_tasks(&self, now_epoch: f64) -> usize {
        let mut removed = 0;
        if let Some(store) = &self.store {
            removed = store.purge_expired_mcp_tasks(now_epoch);
        }
        let mut guard = self.inner.write().unwrap();
        let before = guard.mcp_tasks.len();
        guard.mcp_tasks.retain(|_, t| t.expires_at > now_epoch);
        removed.max(before.saturating_sub(guard.mcp_tasks.len()))
    }

    // --- v6.8.0 Multi-Pod HA OAuth Flows, Tokens & Upstream Callbacks ---

    pub fn save_oauth_flow(&self, flow: &OAuthFlowRecord) {
        {
            let mut guard = self.inner.write().unwrap();
            guard.oauth_flows.insert(flow.state.clone(), flow.clone());
        }
        if let Some(store) = &self.store {
            store.save_oauth_flow(flow);
        }
    }

    pub fn take_oauth_flow(&self, state: &str) -> Option<OAuthFlowRecord> {
        let mem = {
            let mut guard = self.inner.write().unwrap();
            guard.oauth_flows.remove(state)
        };
        if let Some(store) = &self.store {
            if let Some(f) = store.take_oauth_flow(state) {
                return Some(f);
            }
        }
        mem
    }

    pub fn save_oauth_token(&self, token: &OAuthTokenRecord) {
        {
            let mut guard = self.inner.write().unwrap();
            guard
                .oauth_tokens
                .insert(token.server_name.clone(), token.clone());
        }
        if let Some(store) = &self.store {
            store.save_oauth_token(token);
        }
    }

    pub fn get_oauth_token(&self, server_name: &str) -> Option<OAuthTokenRecord> {
        if let Some(store) = &self.store {
            if let Some(t) = store.get_oauth_token(server_name) {
                return Some(t);
            }
        }
        let guard = self.inner.read().unwrap();
        guard.oauth_tokens.get(server_name).cloned()
    }

    pub fn delete_oauth_token(&self, server_name: &str) -> Option<OAuthTokenRecord> {
        let mem = {
            let mut guard = self.inner.write().unwrap();
            guard.oauth_tokens.remove(server_name)
        };
        if let Some(store) = &self.store {
            if let Some(t) = store.delete_oauth_token(server_name) {
                return Some(t);
            }
        }
        mem
    }

    pub fn save_upstream_callback(&self, cb: &PendingUpstreamCallback) {
        {
            let mut guard = self.inner.write().unwrap();
            guard
                .upstream_callbacks
                .insert(cb.callback_id.clone(), cb.clone());
        }
        if let Some(store) = &self.store {
            store.save_upstream_callback(cb);
        }
    }

    pub fn list_upstream_callbacks(&self) -> Vec<PendingUpstreamCallback> {
        if let Some(store) = &self.store {
            return store.load_all_upstream_callbacks();
        }
        let guard = self.inner.read().unwrap();
        guard.upstream_callbacks.values().cloned().collect()
    }

    pub fn delete_upstream_callback(&self, request_id: &str) -> Option<PendingUpstreamCallback> {
        let mem = {
            let mut guard = self.inner.write().unwrap();
            guard.upstream_callbacks.remove(request_id)
        };
        if let Some(store) = &self.store {
            if let Some(cb) = store.delete_upstream_callback(request_id) {
                return Some(cb);
            }
        }
        mem
    }

    // --- v6.8.0 File Blobs ---

    pub fn save_file_blob(&self, blob: &FileBlobRecord) {
        {
            let mut guard = self.inner.write().unwrap();
            guard.file_blobs.insert(blob.file_id.clone(), blob.clone());
        }
        if let Some(store) = &self.store {
            store.save_file_blob(blob);
        }
    }

    pub fn get_file_blob(&self, file_id: &str) -> Option<FileBlobRecord> {
        if let Some(store) = &self.store {
            if let Some(b) = store.get_file_blob(file_id) {
                return Some(b);
            }
        }
        let guard = self.inner.read().unwrap();
        guard.file_blobs.get(file_id).cloned()
    }

    pub fn delete_file_blob(&self, file_id: &str) -> Option<FileBlobRecord> {
        let mem = {
            let mut guard = self.inner.write().unwrap();
            guard.file_blobs.remove(file_id)
        };
        if let Some(store) = &self.store {
            if let Some(b) = store.delete_file_blob(file_id) {
                return Some(b);
            }
        }
        mem
    }

    pub fn purge_expired_file_blobs(&self, now_epoch: f64) -> Vec<FileBlobRecord> {
        let mut removed = Vec::new();
        if let Some(store) = &self.store {
            removed = store.purge_expired_file_blobs(now_epoch);
        }
        let mut guard = self.inner.write().unwrap();
        let mem_expired: Vec<FileBlobRecord> = guard
            .file_blobs
            .values()
            .filter(|b| b.expires_at <= now_epoch)
            .cloned()
            .collect();
        guard.file_blobs.retain(|_, b| b.expires_at > now_epoch);
        if removed.is_empty() {
            mem_expired
        } else {
            removed
        }
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
