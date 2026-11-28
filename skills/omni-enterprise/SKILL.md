---
name: omni-enterprise
description: Deploy, govern, and operate OmniMCP v6.8.5 in enterprise environments with remote Streamable HTTP, OMNI_REQUIRE_MCP_AUTH, OIDC/JWKS + RFC 9728 auth, RBAC, Tool Group isolation, Postgres HA, Multi-Cloud File Blobs, Wasm ABI v2, OTLP tracing, and Merkle audit verification.
---

# OmniMCP Enterprise Operations & Governance Skill (`v6.8.5`)

Use this skill when deploying, securing, or distributing OmniMCP across enterprise teams and coding-agent fleets.

## 1. Remote Streamable HTTP & Strict Authentication

When binding to a non-loopback interface (`OMNI_HOST=0.0.0.0`):
- `validate_bind()` requires at least one of `OMNI_AUTH_SECRET`, `OMNI_JWT_SECRET`, or `OMNI_OIDC_JWKS_URI`.
- `OMNI_REQUIRE_MCP_AUTH` defaults to **`true`** on non-loopback hosts (and `false` on loopback unless explicitly enabled).
- Unauthenticated requests to `/omni/mcp`, `/groups/<slug>/mcp`, or `/<service>/mcp` return HTTP `401 Unauthorized` with `WWW-Authenticate: Bearer resource_metadata="/.well-known/oauth-protected-resource"` (RFC 9728 / RFC 8707).

### RBAC Hierarchy
- `Agent` (lowest): MCP execution (`/omni/mcp`, `/groups/<slug>/mcp`), `/api/v1/servers`, `/api/v1/groups`, `/api/v1/skills` (GET), `/api/v1/plugin/manifest` (GET), `/api/v1/codemode/eval`.
- `Operator`: Observability (`/api/v1/traces`, `/api/v1/audit`, `/api/v1/audit/verify`, `/api/v1/tasks` GET).
- `Approver`: HITL approvals & elicitation (`/api/v1/codemode/resume`, `/api/v1/callbacks/*`, `/api/v1/tasks/*` POST). Separation of Duties enforces that the approver must differ from the initiating actor.
- `Admin`: Registry mutations, external server onboarding, skill publishing/deletion, Wasm tool registration.

## 2. Tool Group Isolation

Create scoped Tool Groups (e.g., `billing`, `read-only-analytics`) and point enterprise coding agents to:
- Remote HTTP: `https://mcp.corp.example.com/groups/<slug>/mcp`
- Local/Bastion Stdio: `omni-gateway --stdio --group <slug>`

Both transports enforce identical isolation across `tools/list`, `tools/call`, `resources/list`, `resources/read`, `prompts/list`, `prompts/get`, `skills/list`, `skills/get`, and `tasks/*`.

## 3. Enterprise Plugin Distribution & Air-Gapped MDM

1. **Generate an Enterprise Plugin**:
   ```bash
   omni plugin init --mode enterprise --agent all \
     --gateway-url https://mcp.corp.example.com \
     --group billing \
     --auth-env OMNI_AUTH_TOKEN \
     --out ./enterprise-plugin
   ```
   Guardrails: enforces `https://`, rejects plaintext secrets in `--auth-env` or `--gateway-url`, emits `enterprise-policy.json` and `MANIFEST.sha256`.
2. **Export an Air-Gapped MDM Bundle** (includes all agent manifests, Claude dual marketplace, 4 built-in skills, optional live registry skills, `enterprise-policy.json`, and `SHA256SUMS`):
   ```bash
   omni plugin export --enterprise \
     --gateway-url https://mcp.corp.example.com \
     --include-registry-skills \
     --out ./mdm-bundle
   ```
3. **Verify Cryptographic Integrity Before Rollout**:
   ```bash
   omni plugin verify ./mdm-bundle
   ```
4. **Self-Service Manifest API**: Authenticated agents or Governance Studio UI can fetch live client configs via `GET /api/v1/plugin/manifest?agent=claude&mode=enterprise&group=billing`.

## 4. High Availability, Storage, Blobs, Telemetry & Audit

- **PostgreSQL HA**: Set `OMNI_STORAGE_BACKEND=postgres`, `OMNI_DATABASE_URL`, and `OMNI_PG_POOL_SIZE` (default `8`). Multi-pod convergence runs over `PgWirePool` + `LISTEN/NOTIFY` (`omni_registry_changed`, `omni_task_changed`).
- **Multi-Cloud File Blobs**: `OMNI_FILE_STORAGE_BACKEND=local|s3|gcs|azure|postgres|http`.
- **Wasm Guest ABI v2**: Register zero-trust Wasm tools via `omni wasm register` with deterministic fuel limits.
- **Observability & Audit**: Configure `OMNI_OTLP_ENDPOINT` for W3C Trace Context spans and verify tamper-evident logs via `GET /api/v1/audit/verify` (SHA-256 Merkle chain).

See `references/enterprise-env.md` for the complete environment variable matrix.
