# OmniMCP Enterprise Plugin Distribution Runbook (`v6.8.5`)

This package provides a governed, zero-local-stdio OmniMCP plugin for enterprise coding-agent fleets (Claude Code, Antigravity, Gemini CLI, Codex, Cursor, VS Code / GitHub Copilot, Windsurf, Cline, and OpenCode).

## 1. Gateway Prerequisites

Deploy OmniMCP Gateway behind HTTPS with authentication enforced:

```bash
export OMNI_HOST=0.0.0.0
export OMNI_PORT=8080
export OMNI_PUBLIC_BASE_URL=https://mcp.corp.example.com
export OMNI_REQUIRE_MCP_AUTH=true
# Configure at least one auth mechanism (OIDC JWKS, HS256 JWT, or static RBAC secrets):
export OMNI_OIDC_JWKS_URI=https://idp.corp.example.com/.well-known/jwks.json
export OMNI_OIDC_ISSUER=https://idp.corp.example.com/
export OMNI_OIDC_AUDIENCE=https://mcp.corp.example.com
# Optional HA PostgreSQL registry + task queue:
export OMNI_STORAGE_BACKEND=postgres
export OMNI_DATABASE_URL=postgres://omni:secret@pg.corp.internal:5432/omni
```

## 2. Generate & Verify an Enterprise Plugin Bundle

```bash
omni plugin init \
  --mode enterprise \
  --agent all \
  --gateway-url https://mcp.corp.example.com \
  --group billing \
  --auth-env OMNI_AUTH_TOKEN \
  --out ./dist/omni-enterprise-billing

omni plugin verify ./dist/omni-enterprise-billing
```

## 3. Air-Gapped MDM / Private Git Marketplace Export

```bash
omni plugin export \
  --enterprise \
  --gateway-url https://mcp.corp.example.com \
  --group billing \
  --auth-env OMNI_AUTH_TOKEN \
  --include-registry-skills \
  --out ./dist/mdm-bundle

omni plugin verify ./dist/mdm-bundle
```

Distribute `./dist/mdm-bundle` via Jamf/Intune/Fleet MDM or publish it to your internal Git marketplace (`.claude-plugin/marketplace.json`).

## 4. Client Environment Setup

Developers only need their short-lived OIDC/Bearer token in their environment (never committed to config files):

```bash
export OMNI_GATEWAY_URL=https://mcp.corp.example.com
export OMNI_AUTH_TOKEN="$(corp-sso token --audience https://mcp.corp.example.com)"
```

## 5. Governance Studio Self-Service

Authenticated users (`AuthRole::Agent` or higher) can also generate agent-specific configs from the **Governance Studio UI → Skills Studio → Install & Enterprise Plugin** tab, backed by `GET /api/v1/plugin/manifest`.
