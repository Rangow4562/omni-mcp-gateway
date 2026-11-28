# OmniMCP Gateway — Universal Coding Agent Instructions (`v6.8.5`)

This repository contains **OmniMCP Gateway** (`omni-mcp` / `@rangow2260/omni-mcp`), a high-performance pure-Rust enterprise Model Context Protocol (MCP) gateway and Code Mode runtime (`#![forbid(unsafe_code)]`).

## Bundled Agent Skills (`skills/`)

Install all 4 skills into your coding agent (`claude`, `antigravity`, `gemini`, `codex`, `cursor`, `copilot`, `vscode`, `windsurf`, `cline`, `opencode`, or `all`) with:

```bash
omni skills install --agent all --scope project
```

- `skills/omni-mcp/SKILL.md` — Progressive discovery, all 11 MCP tools, `_projection`, Resources, Prompts, SEP-2640 Skills, Tasks GA, Elicitation, and error remediation.
- `skills/omni-codemode/SKILL.md` — Sandboxed Code Mode (`codemode.<service>.<tool>(...)`), 0-turn BM25 types, `Promise.all` parallel waves, HITL approval/resume, DST chaos testing, and LIFO Saga rollbacks.
- `skills/omni-cli/SKILL.md` — Terminal & CLI-first workflows (`omni`, `aifx`, `omni-gateway`, `npx -y @rangow2260/omni-mcp`).
- `skills/omni-enterprise/SKILL.md` — Enterprise deployment, remote Streamable HTTP (`/omni/mcp`, `/groups/<slug>/mcp`), `OMNI_REQUIRE_MCP_AUTH`, OIDC/JWKS, RBAC, Postgres HA, and MDM plugin packaging.

## Key Operating Rules for AI Agents

1. **Progressive Discovery**: Call `discover_server` → `discover_tools` → `get_tool_schema` before invoking unfamiliar tools. Pass `_projection` on `invoke_tool` to keep responses compact.
2. **Code Mode Syntax**: Always use `codemode.<service>.<tool>(args)` (never `omni.<service>.<tool>(args)`). Fetch types first via `get_codemode_types` with `intent`.
3. **Enterprise Isolation**: When operating against a corporate gateway, use the Enterprise Plugin (`plugins/enterprise/` or `omni plugin init --mode enterprise`) pointing to `https://<gateway>/groups/<slug>/mcp` with Bearer/OIDC env var references (never commit plaintext secrets).
4. **Verification**: Validate plugin bundles with `omni plugin verify <dir>` and verify audit integrity via `GET /api/v1/audit/verify`.
