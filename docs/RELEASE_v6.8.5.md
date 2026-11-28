# OmniMCP `v6.8.5` Release Notes
## Universal CLI/Coding-Agent Skills Suite & Governed Enterprise Plugin Option

**Release Date**: 2026-11-28  
**Crate / NPM**: `omni-mcp` `6.8.5` / `@rangow2260/omni-mcp@6.8.5`  
**Safety**: `#![forbid(unsafe_code)]`, zero new Cargo dependencies.

---

## Highlights

### 1. Universal Agent Skills Suite (`skills/` + `AGENTS.md`)
Four canonical skill bundles compliant with both `agentskills.io` and OmniMCP SEP-2640 (`SkillRecord::validate()`):
- `skills/omni-mcp/` — All 11 MCP gateway tools, `_projection`, Resources, Prompts, SEP-2640 Skills, Tasks GA, Elicitation, Streaming File Uploads, and `-320xx` error remediation.
- `skills/omni-codemode/` — `get_codemode_types` 0-turn BM25 declarations, strict `codemode.<service>.<tool>(...)` syntax, `Promise.all` parallel waves, HITL `APPROVAL_REQUIRED` + `resume_codemode`, DST chaos simulation, and LIFO Saga rollbacks.
- `skills/omni-cli/` — Complete CLI reference for `omni`, `aifx`, `omni-gateway`, and `npx -y @rangow2260/omni-mcp`.
- `skills/omni-enterprise/` — Enterprise Streamable HTTP, `OMNI_REQUIRE_MCP_AUTH`, OIDC/JWKS + RFC 9728, RBAC, Tool Group isolation, Postgres HA, Multi-Cloud Blobs, Wasm ABI v2, OTLP tracing, and Merkle audit verification.

### 2. Standard & Enterprise Plugin Distribution
- **Claude Code**: `.claude-plugin/plugin.json` and dual `.claude-plugin/marketplace.json` (`omni-mcp` Standard local stdio + `omni-mcp-enterprise` Enterprise remote HTTP).
- **Antigravity**: `plugins/antigravity/plugin.json` + `plugins/enterprise/plugin.json`.
- **Gemini CLI**: Root `gemini-extension.json` pointing to `AGENTS.md`.
- **Universal Config Templates**: `examples/mcp-configs/` for VS Code / Copilot, Codex (TOML), Windsurf, OpenCode, Cursor rules (`.mdc`), and Enterprise Streamable HTTP.
- **Enterprise CLI**:
  - `omni skills install --agent <target|all> --scope project|user [--dir <path>] [--only <slug,...>] [--dry-run] [--force]` (pure offline)
  - `omni skills bootstrap [--group <slug>]` (opt-in live registry seeding; `OMNI_SEED_BUILTIN_SKILLS` defaults to `false`)
  - `omni plugin init --mode standard|enterprise ...` (enforces HTTPS, blocks plaintext secrets, emits `enterprise-policy.json` + `MANIFEST.sha256`)
  - `omni plugin export --enterprise [--include-registry-skills]` (air-gapped MDM bundle + `SHA256SUMS`)
  - `omni plugin verify <dir>` (JSON/semver, SEP-2640 skill validation, SHA-256 tamper check, secret leak scan)
- **Governance Studio UI**: New **Skills Studio → Install & Enterprise Plugin** tab backed by `GET /api/v1/plugin/manifest`.

### 3. Foundational Fixes
1. Fixed `omni-gateway` CLI dispatch for `skills` and `plugin` subcommands.
2. Unified Tool Group isolation across HTTP `POST /groups/<slug>/mcp` and `--stdio --group <slug>`.
3. Added `OMNI_REQUIRE_MCP_AUTH` (defaults to `true` on non-loopback binds) and fixed `validate_bind()` for `OMNI_JWT_SECRET` and `OMNI_OIDC_JWKS_URI`.
4. Added `GET /groups/<slug>/mcp` JSON discovery metadata and real `build_server_capabilities()` output on `GET /omni/mcp`.
5. Eliminated `~/.omni/data` side effects on pure-offline CLI commands.
6. Fixed auto-synthesized playbook snippets to emit `codemode.<srv>.<tool>({})`.
7. Hardened `parse_skill_frontmatter()` for YAML `>` / `|` multi-line scalars and nested keys.
8. Updated `Dockerfile`, `.github/workflows/release.yml`, `ui/package-lock.json`, and the single-sourcing drift guard test.
