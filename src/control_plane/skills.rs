//! Fleet-aware Skills over MCP Engine (`io.modelcontextprotocol/skills`, SEP-2640, v6.8.0).
//!
//! Supports both curated / Git-synced skill bundles and auto-synthesized Tool Group / Service
//! Code Mode Playbooks (`SKILL.md`, `references/types.d.ts`, `references/examples.ts`) whose
//! `version` and `manifest_sha256` remain strictly deterministic across gateway restarts unless
//! underlying tool schemas change.

use crate::control_plane::MCPRegistry;
use crate::data_plane::codemode::CodeModeTypeGenerator;
use crate::json::JsonValue;
use crate::logging::now_epoch_secs;
use crate::models::{
    ServerRecord, SkillFileEntry, SkillRecord, ToolGroupRecord, ToolStatus, SKILL_MAX_FILES,
    SKILL_MAX_TOTAL_BYTES,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// Scope filter for `skills/list`, `skills/get`, `resources/read`, and `resources/directory/read`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillAccessScope {
    pub group_slug: Option<String>,
    pub server_scope: Option<String>,
}

/// Fleet-aware Skill Engine combining persisted curated skills with deterministically
/// auto-synthesized Tool Group / Virtual Service Code Mode Playbooks.
#[derive(Clone)]
pub struct SkillEngine {
    registry: MCPRegistry,
}

impl SkillEngine {
    pub fn new(registry: MCPRegistry) -> Self {
        Self { registry }
    }

    /// Synthesizes a deterministic Code Mode Playbook skill for a Tool Group.
    /// The `manifest_sha256` and `version` depend exclusively on the group's tools and schemas,
    /// so restarts never trigger spurious client re-approval prompts.
    pub fn synthesize_group_playbook(
        &self,
        group: &ToolGroupRecord,
        servers: &[ServerRecord],
    ) -> SkillRecord {
        let mut included_servers: Vec<ServerRecord> = Vec::new();
        for entry in &group.entries {
            if let Some(srv) = servers.iter().find(|s| s.name == entry.server_name) {
                let mut filtered = srv.clone();
                filtered.tools.retain(|t_name, tool| {
                    tool.status == ToolStatus::Enabled
                        && entry
                            .tool_name
                            .as_ref()
                            .map(|only| only == t_name)
                            .unwrap_or(true)
                });
                if !filtered.tools.is_empty() {
                    if let Some(existing) = included_servers
                        .iter_mut()
                        .find(|s| s.name == filtered.name)
                    {
                        for (k, v) in filtered.tools {
                            existing.tools.insert(k, v);
                        }
                    } else {
                        included_servers.push(filtered);
                    }
                }
            }
        }
        included_servers.sort_by(|a, b| a.name.cmp(&b.name));

        let ts_decls = CodeModeTypeGenerator::generate_all_declarations(&included_servers);
        let mut examples = String::from("// Auto-synthesized OmniMCP Code Mode Examples\n");
        let mut tool_summary = String::new();
        for srv in &included_servers {
            let san_srv = CodeModeTypeGenerator::sanitize_ident(
                srv.name.strip_prefix("external-mcp:").unwrap_or(&srv.name),
            );
            let mut tool_names: Vec<&String> = srv.tools.keys().collect();
            tool_names.sort();
            for t_name in tool_names {
                if let Some(t) = srv.tools.get(t_name) {
                    let san_tool = CodeModeTypeGenerator::sanitize_ident(t_name);
                    tool_summary.push_str(&format!(
                        "- `{}.{}`: {}\n",
                        san_srv, san_tool, t.description
                    ));
                    examples.push_str(&format!(
                        "// {}\nconst res_{} = await codemode.{}.{}({{}});\n",
                        t.description, san_tool, san_srv, san_tool
                    ));
                }
            }
        }

        let slug = format!("playbook-group-{}", group.slug);
        let title = if group.display_name.trim().is_empty() {
            format!("{} Playbook", group.slug)
        } else {
            format!("{} Playbook", group.display_name)
        };
        let desc = if group.description.trim().is_empty() {
            format!(
                "Auto-synthesized Code Mode playbook for Tool Group '{}'",
                group.slug
            )
        } else {
            group.description.clone()
        };

        let skill_md = format!(
            "---\nname: {}\ndescription: {}\n---\n\n# {}\n\n{}\n\n## Available Tools\n\n{}\n## References\n\n- `references/types.d.ts`: Strict TypeScript declarations for Omni Code Mode.\n- `references/examples.ts`: Starter Code Mode invocation snippets.\n",
            title,
            desc.replace('\n', " "),
            title,
            desc,
            if tool_summary.is_empty() {
                "_No enabled tools currently bound to this group._\n".to_string()
            } else {
                tool_summary
            }
        );

        let files = vec![
            SkillFileEntry::new("SKILL.md", "text/markdown", skill_md),
            SkillFileEntry::new("references/types.d.ts", "application/typescript", ts_decls),
            SkillFileEntry::new("references/examples.ts", "application/typescript", examples),
        ];

        let mut skill = SkillRecord {
            slug,
            name: title,
            description: desc,
            version: String::new(),
            group_slug: Some(group.slug.clone()),
            server_scope: None,
            auto_generated: true,
            files,
            manifest_sha256: String::new(),
            updated_at: 0.0,
        };
        let _ = skill.validate();
        skill
    }

    /// Synthesizes a deterministic Code Mode Playbook skill for a single Virtual Service.
    pub fn synthesize_service_playbook(&self, server: &ServerRecord) -> SkillRecord {
        let mut filtered = server.clone();
        filtered
            .tools
            .retain(|_, tool| tool.status == ToolStatus::Enabled);
        let ts_decls = CodeModeTypeGenerator::generate_all_declarations(&[filtered.clone()]);
        let san_srv = CodeModeTypeGenerator::sanitize_ident(
            server
                .name
                .strip_prefix("external-mcp:")
                .unwrap_or(&server.name),
        );
        let mut examples = String::from("// Auto-synthesized Service Code Mode Examples\n");
        let mut tool_summary = String::new();
        let mut tool_names: Vec<&String> = filtered.tools.keys().collect();
        tool_names.sort();
        for t_name in tool_names {
            if let Some(t) = filtered.tools.get(t_name) {
                let san_tool = CodeModeTypeGenerator::sanitize_ident(t_name);
                tool_summary.push_str(&format!(
                    "- `{}.{}`: {}\n",
                    san_srv, san_tool, t.description
                ));
                examples.push_str(&format!(
                    "// {}\nconst res_{} = await codemode.{}.{}({{}});\n",
                    t.description, san_tool, san_srv, san_tool
                ));
            }
        }

        let clean_slug: String = server
            .name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let slug = format!("playbook-service-{}", clean_slug);
        let desc = if server.description.trim().is_empty() {
            format!(
                "Auto-synthesized Code Mode playbook for service '{}'",
                server.name
            )
        } else {
            server.description.clone()
        };
        let skill_md = format!(
            "---\nname: {} Playbook\ndescription: {}\n---\n\n# {} Playbook\n\n{}\n\n## Tools\n\n{}\n",
            server.display_name,
            desc.replace('\n', " "),
            server.display_name,
            desc,
            tool_summary
        );

        let files = vec![
            SkillFileEntry::new("SKILL.md", "text/markdown", skill_md),
            SkillFileEntry::new("references/types.d.ts", "application/typescript", ts_decls),
            SkillFileEntry::new("references/examples.ts", "application/typescript", examples),
        ];

        let mut skill = SkillRecord {
            slug,
            name: format!("{} Playbook", server.display_name),
            description: desc,
            version: String::new(),
            group_slug: None,
            server_scope: Some(server.name.clone()),
            auto_generated: true,
            files,
            manifest_sha256: String::new(),
            updated_at: 0.0,
        };
        let _ = skill.validate();
        skill
    }

    /// Lists all skills visible within `scope`, merging curated persisted skills with
    /// deterministically auto-synthesized playbooks. Curated skills with the same slug
    /// override auto-synthesized ones.
    pub fn list_skills(&self, scope: &SkillAccessScope) -> Vec<SkillRecord> {
        let mut by_slug: BTreeMap<String, SkillRecord> = BTreeMap::new();
        let servers = self.registry.list_servers(None);

        // 1. Auto-synthesized Tool Group playbooks
        for group in self.registry.list_tool_groups() {
            let playbook = self.synthesize_group_playbook(&group, &servers);
            if Self::is_visible_in_scope(&playbook, scope) {
                by_slug.insert(playbook.slug.clone(), playbook);
            }
        }

        // 2. Auto-synthesized Virtual Service playbook if scoped to a specific service
        if let Some(ref srv_name) = scope.server_scope {
            if let Some(srv) = servers.iter().find(|s| s.name == *srv_name) {
                let playbook = self.synthesize_service_playbook(srv);
                by_slug.insert(playbook.slug.clone(), playbook);
            }
        }

        // 3. Curated persisted skills (override auto-generated on slug collision)
        for skill in self.registry.list_skills() {
            if Self::is_visible_in_scope(&skill, scope) {
                by_slug.insert(skill.slug.clone(), skill);
            }
        }

        by_slug.into_values().collect()
    }

    /// Fetches a single skill by slug, enforcing `SkillAccessScope` isolation.
    pub fn get_skill(&self, slug: &str, scope: &SkillAccessScope) -> Option<SkillRecord> {
        self.list_skills(scope).into_iter().find(|s| s.slug == slug)
    }

    /// Checks whether `skill` is visible to a caller operating under `scope`.
    pub fn is_visible_in_scope(skill: &SkillRecord, scope: &SkillAccessScope) -> bool {
        if let Some(ref req_group) = scope.group_slug {
            // Tool group scope: only skills bound to this group OR explicitly global (no group & no server scope)
            // Wait: to enforce strict cross-group isolation, skills bound to ANOTHER group are denied!
            if let Some(ref sk_group) = skill.group_slug {
                if sk_group != req_group {
                    return false;
                }
            }
            if let Some(ref sk_srv) = skill.server_scope {
                // Only allow server-scoped skill if we know nothing restricts it? Better: deny other server scopes unless global.
                let _ = sk_srv;
                return false;
            }
            return true;
        }
        if let Some(ref req_srv) = scope.server_scope {
            if let Some(ref sk_srv) = skill.server_scope {
                return sk_srv == req_srv;
            }
            if skill.group_slug.is_some() {
                return false;
            }
            return true;
        }
        true
    }

    /// Reads a file from a `skill://<slug>/<path>` URI with scope enforcement.
    pub fn read_skill_resource(
        &self,
        uri: &str,
        scope: &SkillAccessScope,
    ) -> Result<(String, String, String), String> {
        let rest = uri
            .strip_prefix("skill://")
            .ok_or_else(|| format!("Invalid skill URI '{}'", uri))?;
        let (slug, rel_path) = match rest.split_once('/') {
            Some((s, p)) if !p.is_empty() => (s, p),
            Some((s, _)) => (s, "SKILL.md"),
            None => (rest, "SKILL.md"),
        };
        let skill = self
            .get_skill(slug, scope)
            .ok_or_else(|| format!("Skill '{}' not found or out of scope", slug))?;
        let file = skill
            .files
            .iter()
            .find(|f| f.path == rel_path)
            .ok_or_else(|| format!("File '{}' not found in skill '{}'", rel_path, slug))?;
        Ok((
            format!("skill://{}/{}", skill.slug, file.path),
            file.mime_type.clone(),
            file.content.clone(),
        ))
    }

    /// Implements `resources/directory/read` lazy child listing for `skill://<slug>` or `skill://<slug>/<subdir>`.
    pub fn read_skill_directory(
        &self,
        uri: &str,
        scope: &SkillAccessScope,
    ) -> Result<Vec<JsonValue>, String> {
        let rest = uri
            .strip_prefix("skill://")
            .ok_or_else(|| format!("Invalid skill directory URI '{}'", uri))?
            .trim_end_matches('/');
        let (slug, prefix) = match rest.split_once('/') {
            Some((s, p)) => (s, format!("{}/", p.trim_matches('/'))),
            None => (rest, String::new()),
        };
        let skill = self
            .get_skill(slug, scope)
            .ok_or_else(|| format!("Skill '{}' not found or out of scope", slug))?;

        let mut dirs = std::collections::BTreeSet::new();
        let mut entries: Vec<JsonValue> = Vec::new();
        for f in &skill.files {
            if !f.path.starts_with(&prefix) {
                continue;
            }
            let remainder = &f.path[prefix.len()..];
            if let Some((subdir, _)) = remainder.split_once('/') {
                if dirs.insert(subdir.to_string()) {
                    let dir_path = format!("{}{}", prefix, subdir);
                    entries.push(json!({
                        "uri": format!("skill://{}/{}", skill.slug, dir_path),
                        "name": subdir,
                        "type": "directory"
                    }));
                }
            } else {
                entries.push(json!({
                    "uri": format!("skill://{}/{}", skill.slug, f.path),
                    "name": remainder,
                    "type": "file",
                    "mimeType": f.mime_type.clone(),
                    "sizeBytes": f.size_bytes as i64,
                    "sha256": f.sha256.clone()
                }));
            }
        }
        Ok(entries)
    }

    /// Loads a skill bundle from a local directory containing `SKILL.md` and supporting files,
    /// validates SEP-2640 limits (512 files / 16 MiB), and persists it to the registry.
    pub fn publish_from_dir(
        &self,
        dir_path: &Path,
        slug_override: Option<&str>,
        group_slug: Option<String>,
        server_scope: Option<String>,
    ) -> Result<SkillRecord, String> {
        if !dir_path.is_dir() {
            return Err(format!(
                "Skill directory '{}' does not exist",
                dir_path.display()
            ));
        }
        let slug = slug_override
            .map(String::from)
            .or_else(|| {
                dir_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(String::from)
            })
            .ok_or_else(|| "Unable to derive skill slug from directory path".to_string())?;

        let mut files: Vec<SkillFileEntry> = Vec::new();
        let mut total_bytes: usize = 0;
        Self::collect_dir_files(dir_path, dir_path, &mut files, &mut total_bytes)?;

        let mut record = SkillRecord {
            slug,
            name: String::new(),
            description: String::new(),
            version: String::new(),
            group_slug,
            server_scope,
            auto_generated: false,
            files,
            manifest_sha256: String::new(),
            updated_at: now_epoch_secs(),
        };
        record.validate()?;
        self.registry.save_skill(record)
    }

    fn collect_dir_files(
        root: &Path,
        current: &Path,
        out: &mut Vec<SkillFileEntry>,
        total_bytes: &mut usize,
    ) -> Result<(), String> {
        let mut entries: Vec<_> = fs::read_dir(current)
            .map_err(|e| format!("Failed to read directory '{}': {}", current.display(), e))?
            .filter_map(Result::ok)
            .collect();
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let path = entry.path();
            let file_name = entry.file_name();
            let name_str = file_name.to_string_lossy();
            if name_str.starts_with('.') {
                continue;
            }
            if path.is_dir() {
                Self::collect_dir_files(root, &path, out, total_bytes)?;
            } else if path.is_file() {
                if out.len() >= SKILL_MAX_FILES {
                    return Err(format!(
                        "Skill directory exceeds maximum of {} files",
                        SKILL_MAX_FILES
                    ));
                }
                let rel = path
                    .strip_prefix(root)
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .replace('\\', "/");
                let bytes = fs::read(&path)
                    .map_err(|e| format!("Failed to read file '{}': {}", path.display(), e))?;
                *total_bytes = total_bytes.saturating_add(bytes.len());
                if *total_bytes > SKILL_MAX_TOTAL_BYTES {
                    return Err(format!(
                        "Skill directory exceeds 16 MiB ({} bytes) total size limit",
                        SKILL_MAX_TOTAL_BYTES
                    ));
                }
                let content = String::from_utf8_lossy(&bytes).into_owned();
                let mime = guess_skill_mime(&rel);
                out.push(SkillFileEntry::new(rel, mime, content));
            }
        }
        Ok(())
    }

    /// Formats a `SkillRecord` for `skills/list` (summary metadata without full file bodies).
    pub fn to_mcp_summary(skill: &SkillRecord) -> JsonValue {
        json!({
            "slug": skill.slug.clone(),
            "name": skill.name.clone(),
            "description": skill.description.clone(),
            "version": skill.version.clone(),
            "uri": format!("skill://{}", skill.slug),
            "entryUri": format!("skill://{}/SKILL.md", skill.slug),
            "manifestSha256": skill.manifest_sha256.clone(),
            "fileCount": skill.files.len() as i64,
            "totalBytes": skill.total_bytes() as i64,
            "autoGenerated": skill.auto_generated,
            "groupSlug": skill.group_slug.as_ref().map(|s| JsonValue::from(s.as_str())).unwrap_or(JsonValue::Null),
            "serverScope": skill.server_scope.as_ref().map(|s| JsonValue::from(s.as_str())).unwrap_or(JsonValue::Null),
        })
    }

    /// Formats a `SkillRecord` for `skills/get` (manifest + file listing + `SKILL.md` body).
    pub fn to_mcp_detail(skill: &SkillRecord) -> JsonValue {
        let files_meta: Vec<JsonValue> = skill
            .files
            .iter()
            .map(|f| {
                json!({
                    "path": f.path.clone(),
                    "uri": format!("skill://{}/{}", skill.slug, f.path),
                    "mimeType": f.mime_type.clone(),
                    "sizeBytes": f.size_bytes as i64,
                    "sha256": f.sha256.clone(),
                })
            })
            .collect();
        let entry_markdown = skill
            .files
            .iter()
            .find(|f| f.path == "SKILL.md")
            .map(|f| f.content.as_str())
            .unwrap_or("");
        json!({
            "slug": skill.slug.clone(),
            "name": skill.name.clone(),
            "description": skill.description.clone(),
            "version": skill.version.clone(),
            "uri": format!("skill://{}", skill.slug),
            "manifestSha256": skill.manifest_sha256.clone(),
            "fileCount": skill.files.len() as i64,
            "totalBytes": skill.total_bytes() as i64,
            "autoGenerated": skill.auto_generated,
            "skillMarkdown": entry_markdown,
            "files": JsonValue::Array(files_meta),
            "resultType": "complete"
        })
    }
}

fn guess_skill_mime(path: &str) -> &'static str {
    if path.ends_with(".md") {
        "text/markdown"
    } else if path.ends_with(".ts") || path.ends_with(".d.ts") {
        "application/typescript"
    } else if path.ends_with(".js") {
        "application/javascript"
    } else if path.ends_with(".json") {
        "application/json"
    } else if path.ends_with(".yaml") || path.ends_with(".yml") {
        "application/yaml"
    } else if path.ends_with(".py") {
        "text/x-python"
    } else if path.ends_with(".sh") {
        "text/x-shellscript"
    } else {
        "text/plain"
    }
}
