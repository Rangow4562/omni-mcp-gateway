//! Universal Coding-Agent Skills Installer & Enterprise Plugin Distribution Engine (v6.8.5).
//!
//! Pure safe Rust (`#![forbid(unsafe_code)]`, zero external crates).
//! Embeds the 4 canonical `skills/` bundles and `AGENTS.md` at compile time via `include_str!`
//! so offline CLI commands (`omni skills install`, `omni plugin init|export|verify|agents`)
//! execute hermetically with zero `~/.omni/data` or PostgreSQL side effects.

use crate::control_plane::MCPRegistry;
use crate::json::JsonValue;
use crate::models::{SkillFileEntry, SkillRecord};
use crate::storage::audit_log::sha256_hex;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Embedded `AGENTS.md` universal coding-agent instruction file.
pub const AGENTS_MD: &str = include_str!("../../AGENTS.md");

/// A single compile-time embedded file inside a built-in skill bundle.
#[derive(Clone, Copy, Debug)]
pub struct EmbeddedSkillFile {
    pub path: &'static str,
    pub mime_type: &'static str,
    pub content: &'static str,
}

/// A compile-time embedded skill bundle.
#[derive(Clone, Copy, Debug)]
pub struct EmbeddedSkillBundle {
    pub slug: &'static str,
    pub files: &'static [EmbeddedSkillFile],
}

/// All 4 canonical OmniMCP usage skills embedded directly in the binary.
pub const BUILTIN_SKILLS: &[EmbeddedSkillBundle] = &[
    EmbeddedSkillBundle {
        slug: "omni-mcp",
        files: &[
            EmbeddedSkillFile {
                path: "SKILL.md",
                mime_type: "text/markdown",
                content: include_str!("../../skills/omni-mcp/SKILL.md"),
            },
            EmbeddedSkillFile {
                path: "references/mcp-tools.md",
                mime_type: "text/markdown",
                content: include_str!("../../skills/omni-mcp/references/mcp-tools.md"),
            },
            EmbeddedSkillFile {
                path: "references/error-codes.md",
                mime_type: "text/markdown",
                content: include_str!("../../skills/omni-mcp/references/error-codes.md"),
            },
        ],
    },
    EmbeddedSkillBundle {
        slug: "omni-codemode",
        files: &[
            EmbeddedSkillFile {
                path: "SKILL.md",
                mime_type: "text/markdown",
                content: include_str!("../../skills/omni-codemode/SKILL.md"),
            },
            EmbeddedSkillFile {
                path: "references/codemode-patterns.ts",
                mime_type: "application/typescript",
                content: include_str!("../../skills/omni-codemode/references/codemode-patterns.ts"),
            },
        ],
    },
    EmbeddedSkillBundle {
        slug: "omni-cli",
        files: &[
            EmbeddedSkillFile {
                path: "SKILL.md",
                mime_type: "text/markdown",
                content: include_str!("../../skills/omni-cli/SKILL.md"),
            },
            EmbeddedSkillFile {
                path: "references/cli-cheatsheet.md",
                mime_type: "text/markdown",
                content: include_str!("../../skills/omni-cli/references/cli-cheatsheet.md"),
            },
        ],
    },
    EmbeddedSkillBundle {
        slug: "omni-enterprise",
        files: &[
            EmbeddedSkillFile {
                path: "SKILL.md",
                mime_type: "text/markdown",
                content: include_str!("../../skills/omni-enterprise/SKILL.md"),
            },
            EmbeddedSkillFile {
                path: "references/enterprise-env.md",
                mime_type: "text/markdown",
                content: include_str!("../../skills/omni-enterprise/references/enterprise-env.md"),
            },
        ],
    },
];

/// Canonical list of supported coding-agent targets.
pub const SUPPORTED_AGENTS: &[&str] = &[
    "claude",
    "antigravity",
    "gemini",
    "codex",
    "cursor",
    "copilot",
    "vscode",
    "windsurf",
    "cline",
    "opencode",
    "universal",
];

/// Builds and validates `SkillRecord`s for all compile-time embedded `BUILTIN_SKILLS`.
pub fn builtin_skill_records(group_slug: Option<String>) -> Result<Vec<SkillRecord>, String> {
    let mut out = Vec::with_capacity(BUILTIN_SKILLS.len());
    for bundle in BUILTIN_SKILLS {
        let files: Vec<SkillFileEntry> = bundle
            .files
            .iter()
            .map(|f| SkillFileEntry::new(f.path, f.mime_type, f.content))
            .collect();
        let mut rec = SkillRecord {
            slug: bundle.slug.to_string(),
            name: String::new(),
            description: String::new(),
            version: String::new(),
            group_slug: group_slug.clone(),
            server_scope: None,
            auto_generated: false,
            files,
            manifest_sha256: String::new(),
            updated_at: 0.0,
        };
        rec.validate()?;
        out.push(rec);
    }
    Ok(out)
}

/// Seeds the 4 built-in usage skills into an active `MCPRegistry` (used by `omni skills bootstrap`
/// and optional `OMNI_SEED_BUILTIN_SKILLS=true`).
pub fn seed_builtin_skills_into_registry(
    registry: &MCPRegistry,
    group_slug: Option<String>,
) -> Result<Vec<SkillRecord>, String> {
    let records = builtin_skill_records(group_slug)?;
    let mut saved = Vec::with_capacity(records.len());
    for rec in records {
        let s = registry.save_skill(rec)?;
        saved.push(s);
    }
    Ok(saved)
}

/// Returns metadata describing all supported coding agents for `omni plugin agents`.
pub fn supported_agents_json() -> JsonValue {
    let items: Vec<JsonValue> = vec![
        json!({"id": "claude", "name": "Claude Code", "skills_dir": ".claude/skills", "config_file": ".mcp.json", "marketplace": ".claude-plugin/marketplace.json"}),
        json!({"id": "antigravity", "name": "Antigravity", "skills_dir": ".agent/skills", "config_file": "mcp_config.json", "manifest": "plugins/antigravity/plugin.json"}),
        json!({"id": "gemini", "name": "Gemini CLI", "skills_dir": ".gemini/skills", "config_file": "gemini-extension.json"}),
        json!({"id": "codex", "name": "OpenAI Codex CLI", "skills_dir": ".codex/skills", "config_file": ".codex/config.toml"}),
        json!({"id": "cursor", "name": "Cursor", "skills_dir": ".cursor/skills", "config_file": ".cursor/mcp.json"}),
        json!({"id": "copilot", "name": "GitHub Copilot", "skills_dir": ".github/skills", "config_file": ".vscode/mcp.json"}),
        json!({"id": "vscode", "name": "VS Code", "skills_dir": ".github/skills", "config_file": ".vscode/mcp.json"}),
        json!({"id": "windsurf", "name": "Windsurf", "skills_dir": ".windsurf/skills", "config_file": ".windsurf/mcp_config.json"}),
        json!({"id": "cline", "name": "Cline", "skills_dir": ".cline/skills", "config_file": ".cline/mcp_settings.json"}),
        json!({"id": "opencode", "name": "OpenCode", "skills_dir": ".opencode/skills", "config_file": "opencode.json"}),
        json!({"id": "universal", "name": "Universal (.agents)", "skills_dir": ".agents/skills", "config_file": "AGENTS.md"}),
    ];
    JsonValue::Array(items)
}

#[derive(Clone, Debug)]
pub struct SkillInstallOptions {
    pub agent: String,
    pub scope: String,
    pub dir: Option<PathBuf>,
    pub only: Vec<String>,
    pub dry_run: bool,
    pub force: bool,
}

#[derive(Clone, Debug)]
pub struct SkillInstallReport {
    pub dry_run: bool,
    pub installed_files: Vec<String>,
    pub skipped_files: Vec<String>,
    pub targets: Vec<String>,
}

fn agent_skills_rel_dir(agent: &str, is_user_scope: bool) -> Result<&'static str, String> {
    match agent {
        "claude" => Ok(".claude/skills"),
        "cursor" => Ok(".cursor/skills"),
        "codex" => Ok(".codex/skills"),
        "antigravity" => {
            if is_user_scope {
                Ok(".gemini/antigravity/skills")
            } else {
                Ok(".agent/skills")
            }
        }
        "gemini" => Ok(".gemini/skills"),
        "copilot" | "vscode" => Ok(".github/skills"),
        "windsurf" => Ok(".windsurf/skills"),
        "cline" => Ok(".cline/skills"),
        "opencode" => Ok(".opencode/skills"),
        "universal" => Ok(".agents/skills"),
        other => Err(format!(
            "Unsupported agent '{}'. Valid agents: {}, all",
            other,
            SUPPORTED_AGENTS.join(", ")
        )),
    }
}

fn expand_agents(agent: &str) -> Result<Vec<&'static str>, String> {
    let lower = agent.trim().to_ascii_lowercase();
    if lower == "all" {
        return Ok(vec![
            "claude",
            "antigravity",
            "gemini",
            "codex",
            "cursor",
            "copilot",
            "windsurf",
            "cline",
            "opencode",
            "universal",
        ]);
    }
    for &a in SUPPORTED_AGENTS {
        if a == lower {
            return Ok(vec![a]);
        }
    }
    Err(format!(
        "Unsupported agent '{}'. Valid agents: {}, all",
        agent,
        SUPPORTED_AGENTS.join(", ")
    ))
}

fn resolve_base_dir(scope: &str, dir_override: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(d) = dir_override {
        return Ok(d.to_path_buf());
    }
    match scope.trim().to_ascii_lowercase().as_str() {
        "project" | "" => std::env::current_dir()
            .map_err(|e| format!("Failed to determine current working directory: {}", e)),
        "user" => std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .map(PathBuf::from)
            .map_err(|_| {
                "Cannot determine user home directory (HOME/USERPROFILE unset)".to_string()
            }),
        other => Err(format!(
            "Invalid scope '{}': expected 'project' or 'user'",
            other
        )),
    }
}

/// Offline installer writing the embedded `skills/` bundles into coding-agent directories.
pub fn install_skills(opts: &SkillInstallOptions) -> Result<SkillInstallReport, String> {
    let is_user_scope = opts.scope.eq_ignore_ascii_case("user");
    let base_dir = resolve_base_dir(&opts.scope, opts.dir.as_deref())?;
    let agents = expand_agents(&opts.agent)?;

    let mut selected_bundles: Vec<&EmbeddedSkillBundle> = Vec::new();
    if opts.only.is_empty() {
        selected_bundles.extend(BUILTIN_SKILLS.iter());
    } else {
        for slug in &opts.only {
            let trimmed = slug.trim();
            if trimmed.is_empty() {
                continue;
            }
            match BUILTIN_SKILLS.iter().find(|b| b.slug == trimmed) {
                Some(b) => selected_bundles.push(b),
                None => {
                    let valid: Vec<&str> = BUILTIN_SKILLS.iter().map(|b| b.slug).collect();
                    return Err(format!(
                        "Unknown built-in skill '{}'. Available skills: {}",
                        trimmed,
                        valid.join(", ")
                    ));
                }
            }
        }
    }

    let mut installed_files = Vec::new();
    let mut skipped_files = Vec::new();
    let mut targets = Vec::new();
    let mut seen_rel_dirs = BTreeSet::new();

    for agent in agents {
        let rel_dir = agent_skills_rel_dir(agent, is_user_scope)?;
        if !seen_rel_dirs.insert(rel_dir) {
            continue;
        }
        let target_root = base_dir.join(rel_dir);
        targets.push(target_root.to_string_lossy().into_owned());

        for bundle in &selected_bundles {
            let skill_root = target_root.join(bundle.slug);
            for f in bundle.files {
                let dest = skill_root.join(f.path);
                let dest_str = dest.to_string_lossy().into_owned();
                if dest.exists() && !opts.force {
                    skipped_files.push(dest_str);
                    continue;
                }
                if !opts.dry_run {
                    if let Some(parent) = dest.parent() {
                        fs::create_dir_all(parent).map_err(|e| {
                            format!("Failed to create directory '{}': {}", parent.display(), e)
                        })?;
                    }
                    fs::write(&dest, f.content.as_bytes())
                        .map_err(|e| format!("Failed to write '{}': {}", dest.display(), e))?;
                }
                installed_files.push(dest_str);
            }
        }
    }

    Ok(SkillInstallReport {
        dry_run: opts.dry_run,
        installed_files,
        skipped_files,
        targets,
    })
}

/// Scans text for known plaintext secret prefixes or literal Bearer tokens.
pub fn detect_plaintext_secret(text: &str) -> Option<&'static str> {
    const PREFIXES: &[(&str, &str)] = &[
        ("sk-", "OpenAI/Anthropic-style secret key (sk-)"),
        ("ghp_", "GitHub personal access token (ghp_)"),
        ("gho_", "GitHub OAuth token (gho_)"),
        ("npm_", "NPM access token (npm_)"),
        ("AKIA", "AWS access key ID (AKIA)"),
        ("glpat-", "GitLab personal access token (glpat-)"),
        ("xoxb-", "Slack bot token (xoxb-)"),
        ("xoxp-", "Slack user token (xoxp-)"),
    ];
    for &(prefix, label) in PREFIXES {
        let mut offset = 0;
        while let Some(rel_idx) = text[offset..].find(prefix) {
            let idx = offset + rel_idx;
            // Require a non-identifier boundary before the prefix so benign hyphenated words
            // like `disk-snapshot` or `task-orchestration` do not false-positive on `sk-`.
            let boundary_ok = idx == 0
                || text[..idx]
                    .chars()
                    .next_back()
                    .map_or(true, |c| !(c.is_ascii_alphanumeric() || c == '_'));
            if boundary_ok {
                let tail = &text[idx + prefix.len()..];
                let token_len = tail
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
                    .count();
                if token_len >= 8 {
                    return Some(label);
                }
            }
            offset = idx + prefix.len();
        }
    }
    // Check for literal `Bearer <secret>` where `<secret>` is not an env placeholder (`${...}`, `{env:...}`, `$VAR`)
    let mut search = text;
    while let Some(pos) = search.find("Bearer ") {
        let rest = &search[pos + "Bearer ".len()..];
        let tok: String = rest
            .chars()
            .take_while(|c| !c.is_whitespace() && !matches!(c, '"' | '\'' | ',' | '}' | ']'))
            .collect();
        let is_www_auth_param = matches!(
            tok.split('=').next().unwrap_or(""),
            "resource_metadata" | "realm" | "error" | "error_description" | "scope"
        ) && tok.contains('=');
        if !tok.is_empty()
            && !is_www_auth_param
            && !tok.starts_with("${")
            && !tok.starts_with("{env:")
            && !tok.starts_with('$')
            && !tok.starts_with('<')
            && tok.len() >= 10
        {
            return Some("Literal Bearer token");
        }
        search = &search[pos + "Bearer ".len()..];
    }
    None
}

/// Validates an optional Tool Group slug to prevent path traversal or TOML/CLI injection.
pub fn validate_group_slug(group_slug: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = group_slug else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.len() > 64
        || !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(format!(
            "Invalid --group '{}': must be 1..=64 ASCII alphanumeric, '-', or '_' characters",
            trimmed
        ));
    }
    Ok(Some(trimmed.to_string()))
}

/// Validates that `--auth-env` is a safe environment variable name (not a plaintext secret).
pub fn validate_auth_env_name(auth_env: &str) -> Result<String, String> {
    let trimmed = auth_env.trim();
    if trimmed.is_empty() {
        return Err("auth_env cannot be empty".to_string());
    }
    if let Some(kind) = detect_plaintext_secret(trimmed) {
        return Err(format!(
            "Refusing plaintext secret in --auth-env ({}). Pass an environment variable name such as OMNI_AUTH_TOKEN.",
            kind
        ));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        || trimmed.chars().next().is_some_and(|c| c.is_ascii_digit())
    {
        return Err(format!(
            "Invalid --auth-env '{}': must be an UPPER_SNAKE_CASE environment variable name (e.g. OMNI_AUTH_TOKEN), never a literal secret.",
            trimmed
        ));
    }
    Ok(trimmed.to_string())
}

/// Validates an enterprise gateway URL (enforcing `https://` unless `allow_insecure_http`).
pub fn validate_gateway_url(
    url: &str,
    enterprise: bool,
    allow_insecure_http: bool,
) -> Result<String, String> {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("Gateway URL cannot be empty".to_string());
    }
    if let Some(kind) = detect_plaintext_secret(trimmed) {
        return Err(format!(
            "Refusing plaintext secret embedded in --gateway-url ({})",
            kind
        ));
    }
    // Reject embedded userinfo credentials in URL (e.g. https://user:pass@host)
    if let Some(after_scheme) = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
    {
        let authority = after_scheme.split('/').next().unwrap_or("");
        if authority.contains('@') {
            return Err("Refusing embedded credentials in --gateway-url".to_string());
        }
    }
    if enterprise && !allow_insecure_http && !trimmed.starts_with("https://") {
        return Err(format!(
            "Enterprise mode requires an https:// --gateway-url (got '{}'). Pass --allow-insecure-http only for isolated test environments.",
            trimmed
        ));
    }
    if !trimmed.starts_with("https://") && !trimmed.starts_with("http://") {
        return Err(format!(
            "Invalid --gateway-url '{}': must begin with https:// or http://",
            trimmed
        ));
    }
    Ok(trimmed.to_string())
}

/// Computes the MCP endpoint path for a gateway URL and optional Tool Group slug.
pub fn build_mcp_endpoint(gateway_url: &str, group_slug: Option<&str>) -> String {
    let base = gateway_url.trim().trim_end_matches('/');
    match group_slug.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some(slug) => format!("{}/groups/{}/mcp", base, slug),
        None => format!("{}/omni/mcp", base),
    }
}

/// Shared manifest generator used by both the CLI and `GET /api/v1/plugin/manifest`.
pub fn build_plugin_manifest_json(
    agent: &str,
    mode: &str,
    gateway_url: &str,
    group_slug: Option<&str>,
    auth_env: &str,
    transport: Option<&str>,
    allow_insecure_http: bool,
) -> Result<JsonValue, String> {
    let norm_agent = agent.trim().to_ascii_lowercase();
    let norm_agent = if norm_agent.is_empty() {
        "claude".to_string()
    } else {
        norm_agent
    };
    if norm_agent != "all" && !SUPPORTED_AGENTS.contains(&norm_agent.as_str()) {
        return Err(format!(
            "Unsupported agent '{}'. Valid agents: {}, all",
            norm_agent,
            SUPPORTED_AGENTS.join(", ")
        ));
    }

    let norm_mode = mode.trim().to_ascii_lowercase();
    let is_enterprise = match norm_mode.as_str() {
        "enterprise" => true,
        "standard" | "" => false,
        other => {
            return Err(format!(
                "Unsupported mode '{}': expected 'standard' or 'enterprise'",
                other
            ))
        }
    };

    let clean_auth_env = validate_auth_env_name(if auth_env.trim().is_empty() {
        "OMNI_AUTH_TOKEN"
    } else {
        auth_env
    })?;

    let clean_group = validate_group_slug(group_slug)?;

    let resolved_transport = match transport.map(|t| t.trim().to_ascii_lowercase()).as_deref() {
        Some("stdio") => "stdio",
        Some("http") => "http",
        Some("mcp-remote") => "mcp-remote",
        Some("") | None => {
            if is_enterprise {
                "http"
            } else {
                "stdio"
            }
        }
        Some(other) => {
            return Err(format!(
                "Unsupported transport '{}': expected 'stdio', 'http', or 'mcp-remote'",
                other
            ))
        }
    };

    if is_enterprise && resolved_transport == "stdio" {
        return Err(
            "Enterprise mode disallows local stdio transport; use 'http' or 'mcp-remote'."
                .to_string(),
        );
    }

    let clean_gateway = if is_enterprise || resolved_transport != "stdio" {
        validate_gateway_url(gateway_url, is_enterprise, allow_insecure_http)?
    } else {
        gateway_url.trim().trim_end_matches('/').to_string()
    };

    let endpoint = build_mcp_endpoint(&clean_gateway, clean_group.as_deref());
    let server_key = if is_enterprise {
        match clean_group.as_deref() {
            Some(g) => format!("omni-mcp-{}", g),
            None => "omni-mcp-enterprise".to_string(),
        }
    } else {
        "omni-mcp".to_string()
    };

    let mut stdio_args = vec![
        JsonValue::from("-y"),
        JsonValue::from("@rangow2260/omni-mcp"),
        JsonValue::from("--stdio"),
    ];
    if let Some(ref g) = clean_group {
        stdio_args.push(JsonValue::from("--group"));
        stdio_args.push(JsonValue::from(g.as_str()));
    }

    let auth_header_val = format!("Bearer ${{{}}}", clean_auth_env);
    let (config_format, config_filename, config_snippet, config_object) = match norm_agent.as_str()
    {
        "codex" => {
            let toml = if resolved_transport == "stdio" {
                let args_toml = match clean_group.as_deref() {
                    Some(g) => format!(
                        "[\"-y\", \"@rangow2260/omni-mcp\", \"--stdio\", \"--group\", \"{}\"]",
                        g
                    ),
                    None => "[\"-y\", \"@rangow2260/omni-mcp\", \"--stdio\"]".to_string(),
                };
                format!(
                    "[mcp_servers.{}]\ncommand = \"npx\"\nargs = {}\n",
                    server_key, args_toml
                )
            } else if resolved_transport == "mcp-remote" {
                format!(
                    "[mcp_servers.{}]\ncommand = \"npx\"\nargs = [\"-y\", \"mcp-remote\", \"{}\", \"--header\", \"Authorization: {}\"]\n",
                    server_key, endpoint, auth_header_val
                )
            } else {
                format!(
                    "[mcp_servers.{}]\nurl = \"{}\"\nbearer_token_env_var = \"{}\"\n",
                    server_key, endpoint, clean_auth_env
                )
            };
            ("toml", ".codex/config.toml", toml, JsonValue::Null)
        }
        "vscode" | "copilot" => {
            let entry = if resolved_transport == "stdio" {
                json!({
                    "type": "stdio",
                    "command": "npx",
                    "args": JsonValue::Array(stdio_args.clone())
                })
            } else if resolved_transport == "mcp-remote" {
                json!({
                    "type": "stdio",
                    "command": "npx",
                    "args": ["-y", "mcp-remote", endpoint.clone(), "--header", format!("Authorization: {}", auth_header_val)]
                })
            } else {
                json!({
                    "type": "http",
                    "url": endpoint.clone(),
                    "headers": {
                        "Authorization": auth_header_val.clone()
                    }
                })
            };
            let mut servers = JsonValue::object();
            servers.insert(&server_key, entry);
            let obj = json!({ "servers": servers });
            let snippet = obj.to_string_pretty();
            ("json", ".vscode/mcp.json", snippet, obj)
        }
        "opencode" => {
            let entry = if resolved_transport == "stdio" {
                let mut cmd = vec![
                    JsonValue::from("npx"),
                    JsonValue::from("-y"),
                    JsonValue::from("@rangow2260/omni-mcp"),
                    JsonValue::from("--stdio"),
                ];
                if let Some(ref g) = clean_group {
                    cmd.push(JsonValue::from("--group"));
                    cmd.push(JsonValue::from(g.as_str()));
                }
                json!({
                    "type": "local",
                    "command": JsonValue::Array(cmd),
                    "enabled": true
                })
            } else {
                json!({
                    "type": "remote",
                    "url": endpoint.clone(),
                    "headers": {
                        "Authorization": auth_header_val.clone()
                    },
                    "enabled": true
                })
            };
            let mut mcp_map = JsonValue::object();
            mcp_map.insert(&server_key, entry);
            let obj = json!({
                "$schema": "https://opencode.ai/config.json",
                "mcp": mcp_map
            });
            let snippet = obj.to_string_pretty();
            ("json", "opencode.json", snippet, obj)
        }
        "gemini" => {
            let entry = if resolved_transport == "stdio" {
                json!({
                    "command": "npx",
                    "args": JsonValue::Array(stdio_args.clone())
                })
            } else if resolved_transport == "mcp-remote" {
                json!({
                    "command": "npx",
                    "args": ["-y", "mcp-remote", endpoint.clone(), "--header", format!("Authorization: {}", auth_header_val)]
                })
            } else {
                json!({
                    "type": "http",
                    "url": endpoint.clone(),
                    "headers": {
                        "Authorization": auth_header_val.clone()
                    }
                })
            };
            let mut mcp_servers = JsonValue::object();
            mcp_servers.insert(&server_key, entry);
            let obj = json!({
                "name": if is_enterprise { "omni-mcp-enterprise" } else { "omni-mcp" },
                "version": env!("CARGO_PKG_VERSION"),
                "description": if is_enterprise {
                    "Enterprise governed OmniMCP Gateway extension over remote Streamable HTTP"
                } else {
                    "OmniMCP Gateway extension with Code Mode, Skills, Tasks GA, and Merkle audit trail"
                },
                "contextFileName": "AGENTS.md",
                "mcpServers": mcp_servers
            });
            let snippet = obj.to_string_pretty();
            ("json", "gemini-extension.json", snippet, obj)
        }
        _ => {
            let entry = if resolved_transport == "stdio" {
                json!({
                    "command": "npx",
                    "args": JsonValue::Array(stdio_args.clone())
                })
            } else if resolved_transport == "mcp-remote" {
                json!({
                    "command": "npx",
                    "args": ["-y", "mcp-remote", endpoint.clone(), "--header", format!("Authorization: {}", auth_header_val)]
                })
            } else {
                json!({
                    "type": "http",
                    "url": endpoint.clone(),
                    "headers": {
                        "Authorization": auth_header_val.clone()
                    }
                })
            };
            let mut mcp_servers = JsonValue::object();
            mcp_servers.insert(&server_key, entry);
            let obj = json!({ "mcpServers": mcp_servers });
            let filename = match norm_agent.as_str() {
                "antigravity" => "mcp_config.json",
                "cursor" => ".cursor/mcp.json",
                "windsurf" => ".windsurf/mcp_config.json",
                "cline" => ".cline/mcp_settings.json",
                _ => ".mcp.json",
            };
            let snippet = obj.to_string_pretty();
            ("json", filename, snippet, obj)
        }
    };

    let install_skills_cmd = format!("omni skills install --agent {} --scope project", norm_agent);
    let init_plugin_cmd = if is_enterprise {
        let mut cmd = format!(
            "omni plugin init --mode enterprise --agent {} --gateway-url {} --auth-env {}",
            norm_agent, clean_gateway, clean_auth_env
        );
        if let Some(ref g) = clean_group {
            cmd.push_str(&format!(" --group {}", g));
        }
        if resolved_transport != "http" {
            cmd.push_str(&format!(" --transport {}", resolved_transport));
        }
        cmd.push_str(" --out ./omni-enterprise-plugin");
        cmd
    } else {
        let mut cmd = format!("omni plugin init --mode standard --agent {}", norm_agent);
        if let Some(ref g) = clean_group {
            cmd.push_str(&format!(" --group {}", g));
        }
        cmd.push_str(" --out ./omni-plugin");
        cmd
    };

    let policy = if is_enterprise {
        let allowed_groups = match clean_group.as_deref() {
            Some(g) => vec![JsonValue::from(g)],
            None => Vec::new(),
        };
        json!({
            "schemaVersion": "1.0",
            "pluginVersion": env!("CARGO_PKG_VERSION"),
            "mode": "enterprise",
            "gatewayUrl": clean_gateway.clone(),
            "endpoint": endpoint.clone(),
            "allowedToolGroups": JsonValue::Array(allowed_groups),
            "requiredAuthMode": "oidc",
            "authTokenEnv": clean_auth_env.clone(),
            "requireSkillSha256Verification": true,
            "allowLocalStdio": false,
            "auditMode": "merkle"
        })
    } else {
        JsonValue::Null
    };

    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "agent": norm_agent,
        "mode": if is_enterprise { "enterprise" } else { "standard" },
        "transport": resolved_transport,
        "gateway_url": clean_gateway,
        "group_slug": clean_group.map(JsonValue::from).unwrap_or(JsonValue::Null),
        "endpoint": endpoint,
        "auth_env": clean_auth_env,
        "config_format": config_format,
        "config_filename": config_filename,
        "config_snippet": config_snippet,
        "config_object": config_object,
        "install_skills_cmd": install_skills_cmd,
        "init_plugin_cmd": init_plugin_cmd,
        "policy": policy
    }))
}

#[derive(Clone, Debug)]
pub struct PluginInitOptions {
    pub mode: String,
    pub agent: String,
    pub gateway_url: Option<String>,
    pub group_slug: Option<String>,
    pub auth_env: String,
    pub transport: Option<String>,
    pub out_dir: PathBuf,
    pub allow_insecure_http: bool,
    pub dry_run: bool,
}

#[derive(Clone, Debug)]
pub struct PluginInitReport {
    pub dry_run: bool,
    pub mode: String,
    pub out_dir: String,
    pub files: Vec<String>,
    pub manifest_sha256: Option<String>,
}

fn write_bundle_map(
    out_dir: &Path,
    files: &BTreeMap<String, String>,
    checksum_filename: Option<&str>,
    dry_run: bool,
) -> Result<(Vec<String>, Option<String>), String> {
    let mut checksum_lines = String::new();
    let mut written = Vec::new();
    for (rel, content) in files {
        let digest = sha256_hex(content.as_bytes());
        checksum_lines.push_str(&format!("{}  {}\n", digest, rel));
        let dest = out_dir.join(rel);
        written.push(rel.clone());
        if !dry_run {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    format!("Failed to create directory '{}': {}", parent.display(), e)
                })?;
            }
            fs::write(&dest, content.as_bytes())
                .map_err(|e| format!("Failed to write '{}': {}", dest.display(), e))?;
        }
    }
    let manifest_hash = if let Some(cs_name) = checksum_filename {
        let hash = sha256_hex(checksum_lines.as_bytes());
        written.push(cs_name.to_string());
        if !dry_run {
            let dest = out_dir.join(cs_name);
            fs::write(&dest, checksum_lines.as_bytes())
                .map_err(|e| format!("Failed to write '{}': {}", dest.display(), e))?;
        }
        Some(hash)
    } else {
        None
    };
    Ok((written, manifest_hash))
}

fn add_builtin_skills_to_map(files: &mut BTreeMap<String, String>) {
    for bundle in BUILTIN_SKILLS {
        for f in bundle.files {
            files.insert(
                format!("skills/{}/{}", bundle.slug, f.path),
                f.content.to_string(),
            );
        }
    }
}

/// Generates a Standard or Enterprise plugin bundle on disk (or dry-run plan).
pub fn init_plugin(opts: &PluginInitOptions) -> Result<PluginInitReport, String> {
    let is_enterprise = match opts.mode.trim().to_ascii_lowercase().as_str() {
        "enterprise" => true,
        "standard" | "" => false,
        other => {
            return Err(format!(
                "Invalid --mode '{}': expected 'standard' or 'enterprise'",
                other
            ))
        }
    };

    let gateway_url = match opts.gateway_url.as_deref() {
        Some(u) if !u.trim().is_empty() => u.to_string(),
        _ if is_enterprise => {
            return Err(
                "Enterprise mode requires --gateway-url https://<your-gateway-host>".to_string(),
            )
        }
        _ => "http://127.0.0.1:8080".to_string(),
    };

    let manifest = build_plugin_manifest_json(
        &opts.agent,
        if is_enterprise {
            "enterprise"
        } else {
            "standard"
        },
        &gateway_url,
        opts.group_slug.as_deref(),
        &opts.auth_env,
        opts.transport.as_deref(),
        opts.allow_insecure_http,
    )?;

    let mut files: BTreeMap<String, String> = BTreeMap::new();
    files.insert("AGENTS.md".to_string(), AGENTS_MD.to_string());
    add_builtin_skills_to_map(&mut files);

    let plugin_name = if is_enterprise {
        "omni-mcp-enterprise"
    } else {
        "omni-mcp"
    };
    let config_filename = manifest["config_filename"]
        .as_str()
        .unwrap_or(".mcp.json")
        .to_string();
    let config_snippet = manifest["config_snippet"]
        .as_str()
        .unwrap_or("{}")
        .to_string();

    files.insert(
        config_filename.clone(),
        format!("{}\n", config_snippet.trim_end()),
    );

    // Always include Claude plugin manifest + Antigravity plugin manifest for cross-agent usability
    let mut claude_manifest = json!({
        "name": plugin_name,
        "version": env!("CARGO_PKG_VERSION"),
        "description": if is_enterprise {
            "OmniMCP Enterprise Plugin (governed remote Streamable HTTP + OIDC/Bearer + Tool Group isolation)"
        } else {
            "OmniMCP Standard Plugin (local stdio + Code Mode + 4 agent skills)"
        },
        "skills": "./skills"
    });
    if config_filename.ends_with(".json") && manifest["config_object"].get("mcpServers").is_some() {
        claude_manifest.insert(
            "mcpServers",
            JsonValue::from(format!("./{}", config_filename)),
        );
    }
    files.insert(
        ".claude-plugin/plugin.json".to_string(),
        format!("{}\n", claude_manifest.to_string_pretty()),
    );

    let ag_manifest = json!({
        "name": plugin_name,
        "displayName": if is_enterprise { "Omni MCP Enterprise Gateway" } else { "Omni MCP Gateway" },
        "version": env!("CARGO_PKG_VERSION"),
        "description": if is_enterprise {
            "Governed Enterprise OmniMCP Gateway plugin over remote Streamable HTTP"
        } else {
            "OmniMCP Gateway v6.8.5 with Code Mode, Skills, Tasks GA, and Merkle audit trail"
        },
        "suggestedPrompts": [
            "Discover available MCP microservices and tools using Omni MCP",
            "Generate 0-turn Code Mode TypeScript types and run a parallel workflow",
            "Verify the cryptographic SHA-256 Merkle audit trail"
        ]
    });
    files.insert(
        "plugin.json".to_string(),
        format!("{}\n", ag_manifest.to_string_pretty()),
    );

    if is_enterprise {
        let policy_str = manifest["policy"].to_string_pretty();
        files.insert(
            "enterprise-policy.json".to_string(),
            format!("{}\n", policy_str),
        );
    }

    let checksum_file = if is_enterprise {
        Some("MANIFEST.sha256")
    } else {
        None
    };

    let (written, manifest_sha256) =
        write_bundle_map(&opts.out_dir, &files, checksum_file, opts.dry_run)?;

    Ok(PluginInitReport {
        dry_run: opts.dry_run,
        mode: if is_enterprise {
            "enterprise".to_string()
        } else {
            "standard".to_string()
        },
        out_dir: opts.out_dir.to_string_lossy().into_owned(),
        files: written,
        manifest_sha256,
    })
}

#[derive(Clone, Debug)]
pub struct EnterpriseExportOptions {
    pub gateway_url: String,
    pub group_slug: Option<String>,
    pub auth_env: String,
    pub out_dir: PathBuf,
    pub allow_insecure_http: bool,
    pub include_registry_skills: bool,
    pub dry_run: bool,
}

#[derive(Clone, Debug)]
pub struct EnterpriseExportReport {
    pub dry_run: bool,
    pub out_dir: String,
    pub files: Vec<String>,
    pub skills_exported: Vec<String>,
    pub sha256sums_digest: String,
}

/// Generates a complete air-gapped IT/MDM Enterprise Plugin distribution bundle
/// containing all agent manifests, Claude dual marketplace, built-in skills,
/// optional live registry skills, `enterprise-policy.json`, and `SHA256SUMS`.
pub fn export_enterprise_bundle(
    opts: &EnterpriseExportOptions,
    extra_skills: &[SkillRecord],
) -> Result<EnterpriseExportReport, String> {
    let clean_gateway = validate_gateway_url(&opts.gateway_url, true, opts.allow_insecure_http)?;
    let clean_auth_env = validate_auth_env_name(&opts.auth_env)?;
    let clean_group = validate_group_slug(opts.group_slug.as_deref())?;
    let endpoint = build_mcp_endpoint(&clean_gateway, clean_group.as_deref());
    let auth_header_val = format!("Bearer ${{{}}}", clean_auth_env);

    let mut files: BTreeMap<String, String> = BTreeMap::new();
    let mut skills_exported: BTreeSet<String> = BTreeSet::new();

    files.insert("AGENTS.md".to_string(), AGENTS_MD.to_string());
    add_builtin_skills_to_map(&mut files);
    for b in BUILTIN_SKILLS {
        skills_exported.insert(b.slug.to_string());
    }

    for sk in extra_skills {
        let mut cloned = sk.clone();
        cloned.validate()?;
        for f in &cloned.files {
            files.insert(
                format!("skills/{}/{}", cloned.slug, f.path),
                f.content.clone(),
            );
        }
        skills_exported.insert(cloned.slug);
    }

    // Enterprise HTTP MCP config + mcp-remote bridge
    let http_mcp = json!({
        "mcpServers": {
            "omni-mcp-enterprise": {
                "type": "http",
                "url": endpoint.clone(),
                "headers": {
                    "Authorization": auth_header_val.clone()
                }
            }
        }
    });
    files.insert(
        "plugins/enterprise/mcp_config.json".to_string(),
        format!("{}\n", http_mcp.to_string_pretty()),
    );

    let bridge_mcp = json!({
        "mcpServers": {
            "omni-mcp-enterprise": {
                "command": "npx",
                "args": [
                    "-y",
                    "mcp-remote",
                    endpoint.clone(),
                    "--header",
                    format!("Authorization: {}", auth_header_val)
                ]
            }
        }
    });
    files.insert(
        "plugins/enterprise/mcp_config.remote-bridge.json".to_string(),
        format!("{}\n", bridge_mcp.to_string_pretty()),
    );

    // Claude Code plugin + marketplace
    let claude_ent = json!({
        "name": "omni-mcp-enterprise",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Enterprise governed OmniMCP Gateway plugin over remote Streamable HTTP",
        "skills": "../../skills",
        "mcpServers": "./mcp_config.json"
    });
    files.insert(
        "plugins/enterprise/.claude-plugin/plugin.json".to_string(),
        format!("{}\n", claude_ent.to_string_pretty()),
    );

    let marketplace = json!({
        "name": "omni-mcp-enterprise-marketplace",
        "version": env!("CARGO_PKG_VERSION"),
        "plugins": [
            {
                "name": "omni-mcp-enterprise",
                "version": env!("CARGO_PKG_VERSION"),
                "description": "Enterprise governed OmniMCP plugin (remote Streamable HTTP + OIDC/Bearer)",
                "source": "./plugins/enterprise"
            }
        ]
    });
    files.insert(
        ".claude-plugin/marketplace.json".to_string(),
        format!("{}\n", marketplace.to_string_pretty()),
    );

    // Antigravity enterprise plugin manifest
    let ag_ent = json!({
        "name": "omni-mcp-enterprise",
        "displayName": "Omni MCP Enterprise Gateway",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Governed Enterprise OmniMCP Gateway plugin over remote Streamable HTTP",
        "suggestedPrompts": [
            "Discover tools available inside my enterprise Tool Group",
            "Run a governed Code Mode workflow with HITL mutation approval",
            "Verify the enterprise Merkle audit chain"
        ]
    });
    files.insert(
        "plugins/enterprise/plugin.json".to_string(),
        format!("{}\n", ag_ent.to_string_pretty()),
    );

    // Gemini CLI extension
    let gemini_ext = json!({
        "name": "omni-mcp-enterprise",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Enterprise governed OmniMCP Gateway extension",
        "contextFileName": "AGENTS.md",
        "mcpServers": {
            "omni-mcp-enterprise": {
                "type": "http",
                "url": endpoint.clone(),
                "headers": {
                    "Authorization": auth_header_val.clone()
                }
            }
        }
    });
    files.insert(
        "gemini-extension.json".to_string(),
        format!("{}\n", gemini_ext.to_string_pretty()),
    );

    // VS Code / Copilot, Codex, OpenCode configs
    for agent in &["vscode", "codex", "opencode", "cursor", "windsurf"] {
        let m = build_plugin_manifest_json(
            agent,
            "enterprise",
            &clean_gateway,
            opts.group_slug.as_deref(),
            &clean_auth_env,
            Some("http"),
            opts.allow_insecure_http,
        )?;
        let fname = format!(
            "configs/{}",
            m["config_filename"]
                .as_str()
                .unwrap_or("mcp.json")
                .trim_start_matches('.')
                .replace('/', "_")
        );
        let snippet = m["config_snippet"].as_str().unwrap_or("{}").to_string();
        files.insert(fname, format!("{}\n", snippet.trim_end()));
    }

    // Enterprise policy
    let allowed_groups: Vec<JsonValue> = opts
        .group_slug
        .as_deref()
        .map(|g| vec![JsonValue::from(g)])
        .unwrap_or_default();
    let policy = json!({
        "schemaVersion": "1.0",
        "pluginVersion": env!("CARGO_PKG_VERSION"),
        "mode": "enterprise",
        "gatewayUrl": clean_gateway,
        "endpoint": endpoint,
        "allowedToolGroups": JsonValue::Array(allowed_groups),
        "requiredAuthMode": "oidc",
        "authTokenEnv": clean_auth_env,
        "requireSkillSha256Verification": true,
        "allowLocalStdio": false,
        "auditMode": "merkle"
    });
    files.insert(
        "enterprise-policy.json".to_string(),
        format!("{}\n", policy.to_string_pretty()),
    );

    let (written, sha_digest) =
        write_bundle_map(&opts.out_dir, &files, Some("SHA256SUMS"), opts.dry_run)?;

    Ok(EnterpriseExportReport {
        dry_run: opts.dry_run,
        out_dir: opts.out_dir.to_string_lossy().into_owned(),
        files: written,
        skills_exported: skills_exported.into_iter().collect(),
        sha256sums_digest: sha_digest.unwrap_or_default(),
    })
}

#[derive(Clone, Debug)]
pub struct PluginVerifyReport {
    pub valid: bool,
    pub checked_files: usize,
    pub validated_skills: Vec<String>,
    pub verified_checksum_file: Option<String>,
    pub errors: Vec<String>,
}

fn is_valid_semver(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    if parts.len() != 3 {
        return false;
    }
    parts
        .iter()
        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

fn collect_files_recursive(
    root: &Path,
    current: &Path,
    out: &mut BTreeMap<String, PathBuf>,
    errors: &mut Vec<String>,
) {
    let entries = match fs::read_dir(current) {
        Ok(e) => e,
        Err(e) => {
            errors.push(format!(
                "Cannot read directory '{}': {}",
                current.display(),
                e
            ));
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                errors.push(format!("Cannot stat '{}': {}", path.display(), e));
                continue;
            }
        };
        if meta.file_type().is_symlink() {
            errors.push(format!(
                "Symlink rejected in plugin bundle: {}",
                path.display()
            ));
            continue;
        }
        if meta.is_dir() {
            collect_files_recursive(root, &path, out, errors);
        } else if meta.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                out.insert(rel_str, path);
            }
        }
    }
}

/// Cryptographically verifies a plugin bundle directory:
/// - rejects symlinks
/// - validates all JSON manifests & semver fields
/// - validates every skill bundle under `skills/<slug>/` via `SkillRecord::validate()`
/// - verifies `MANIFEST.sha256` or `SHA256SUMS` tamper integrity
/// - scans all text files for plaintext secret leaks
pub fn verify_plugin_bundle(dir: &Path) -> Result<PluginVerifyReport, String> {
    if !dir.exists() || !dir.is_dir() {
        return Err(format!(
            "Plugin bundle directory '{}' does not exist",
            dir.display()
        ));
    }

    let mut errors = Vec::new();
    let mut files = BTreeMap::new();
    collect_files_recursive(dir, dir, &mut files, &mut errors);

    let mut validated_skills = Vec::new();
    let mut skill_groups: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut enterprise_requires_checksum = false;

    for (rel, full_path) in &files {
        let bytes = match fs::read(full_path) {
            Ok(b) => b,
            Err(e) => {
                errors.push(format!("Failed to read '{}': {}", rel, e));
                continue;
            }
        };
        let content = String::from_utf8_lossy(&bytes).into_owned();

        // 1. Secret leak scan
        if let Some(kind) = detect_plaintext_secret(&content) {
            errors.push(format!(
                "Plaintext secret leak detected in '{}': {}",
                rel, kind
            ));
        }

        // 2. JSON manifest & semver validation
        if rel.ends_with(".json") {
            match JsonValue::parse(&content) {
                Ok(val) => {
                    if rel == "enterprise-policy.json" {
                        let mode_ent =
                            val.get("mode").and_then(|v| v.as_str()) == Some("enterprise");
                        let req_sha = val
                            .get("requireSkillSha256Verification")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if mode_ent || req_sha {
                            enterprise_requires_checksum = true;
                        }
                    }
                    if let Some(ver) = val.get("version").and_then(|v| v.as_str()) {
                        if !is_valid_semver(ver) {
                            errors.push(format!("Invalid semver '{}' in '{}'", ver, rel));
                        }
                    }
                    if let Some(ver) = val.get("pluginVersion").and_then(|v| v.as_str()) {
                        if !is_valid_semver(ver) {
                            errors.push(format!(
                                "Invalid pluginVersion semver '{}' in '{}'",
                                ver, rel
                            ));
                        }
                    }
                }
                Err(e) => {
                    errors.push(format!("Invalid JSON in '{}': {}", rel, e));
                }
            }
        }

        // 3. Collect skills/<slug>/<file> for SEP-2640 validation
        if let Some(rest) = rel.strip_prefix("skills/") {
            if let Some((slug, inner_path)) = rest.split_once('/') {
                skill_groups
                    .entry(slug.to_string())
                    .or_default()
                    .push((inner_path.to_string(), content));
            }
        }
    }

    for (slug, entries) in skill_groups {
        let skill_files: Vec<SkillFileEntry> = entries
            .into_iter()
            .map(|(p, c)| {
                let mime = if p.ends_with(".md") {
                    "text/markdown"
                } else if p.ends_with(".ts") {
                    "application/typescript"
                } else if p.ends_with(".json") {
                    "application/json"
                } else {
                    "text/plain"
                };
                SkillFileEntry::new(p, mime, c)
            })
            .collect();
        let mut rec = SkillRecord {
            slug: slug.clone(),
            name: String::new(),
            description: String::new(),
            version: String::new(),
            group_slug: None,
            server_scope: None,
            auto_generated: false,
            files: skill_files,
            manifest_sha256: String::new(),
            updated_at: 0.0,
        };
        match rec.validate() {
            Ok(()) => validated_skills.push(slug),
            Err(e) => errors.push(format!(
                "Skill '{}' failed SEP-2640 validation: {}",
                slug, e
            )),
        }
    }

    // 4. Verify MANIFEST.sha256 or SHA256SUMS
    let checksum_name = if files.contains_key("MANIFEST.sha256") {
        Some("MANIFEST.sha256")
    } else if files.contains_key("SHA256SUMS") {
        Some("SHA256SUMS")
    } else {
        None
    };

    if let Some(cs_name) = checksum_name {
        let cs_path = &files[cs_name];
        let cs_text = fs::read_to_string(cs_path)
            .map_err(|e| format!("Failed to read '{}': {}", cs_name, e))?;
        let mut listed = BTreeSet::new();
        for (idx, line) in cs_text.lines().enumerate() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            let parts: Vec<&str> = t.splitn(2, "  ").collect();
            if parts.len() != 2 {
                errors.push(format!(
                    "Malformed checksum line {} in '{}': {}",
                    idx + 1,
                    cs_name,
                    t
                ));
                continue;
            }
            let expected_hash = parts[0].trim();
            let rel_path = parts[1].trim().replace('\\', "/");
            listed.insert(rel_path.clone());
            match files.get(&rel_path) {
                Some(full) => match fs::read(full) {
                    Ok(bytes) => {
                        let actual_hash = sha256_hex(&bytes);
                        if actual_hash != expected_hash {
                            errors.push(format!(
                                "SHA-256 mismatch for '{}': expected {}, got {}",
                                rel_path, expected_hash, actual_hash
                            ));
                        }
                    }
                    Err(e) => errors.push(format!("Cannot read '{}': {}", rel_path, e)),
                },
                None => {
                    errors.push(format!(
                        "File '{}' listed in '{}' is missing from bundle",
                        rel_path, cs_name
                    ));
                }
            }
        }
        // Ensure no unexpected unlisted files were injected
        for rel in files.keys() {
            if rel == "MANIFEST.sha256" || rel == "SHA256SUMS" {
                continue;
            }
            if !listed.contains(rel) {
                errors.push(format!(
                    "Unlisted file '{}' found in bundle (not covered by '{}')",
                    rel, cs_name
                ));
            }
        }
    } else if enterprise_requires_checksum {
        errors.push(
            "Enterprise bundle requires MANIFEST.sha256 or SHA256SUMS for tamper verification"
                .to_string(),
        );
    }

    Ok(PluginVerifyReport {
        valid: errors.is_empty(),
        checked_files: files.len(),
        validated_skills,
        verified_checksum_file: checksum_name.map(|s| s.to_string()),
        errors,
    })
}
