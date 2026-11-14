# Omni MCP v6.0.0 Release Notes

**Release Date**: 2026-11-13  
**Upgrade Tier**: Major (`5.5.1` -> `6.0.0`)

## Highlights

1. **Feature-Gated Hybrid Dependencies & Native TLS (`tls-rustls`)**:
   - `os-csprng` (`getrandom`) enabled by default for cryptographic ULIDs and continuation secrets.
   - Optional `tls-rustls` (`rustls` + `webpki-roots`) enables native `https://` upstream and downstream connections while keeping default cross-compilation targets lean.
2. **Storage & Segmented Audit v2**:
   - `RegistryStorageBackend` trait abstraction over `WalRegistryStore`.
   - Bounded sliding-window `MerkleAuditStore` with automatic JSONL segment rotation and HMAC-SHA256 signed `.ckpt` checkpoints.
3. **Full MCP Aggregation & Virtual Tool Groups**:
   - Curated Tool Groups (`/api/v1/groups`, `POST /groups/<slug>/mcp`).
   - External `resources/*` (`ext://<slug>/<uri>`) and `prompts/*` (`<slug>:<prompt>`) aggregation.
   - Concurrent ID-multiplexed stdio client with per-tool `timeout_ms`, Streamable HTTP `text/event-stream` client parsing, and opt-in `AutoSyncScheduler`.
4. **Auth Hardening & Live Events**:
   - HS256 JWT validation with RFC 8707 `aud` checking (`OMNI_JWT_SECRET`, `OMNI_JWT_ISSUER`, `OMNI_JWT_AUDIENCE`).
   - RFC 8693 3P OAuth token exchange (`OMNI_3P_TOKEN_ENDPOINT`), with synthetic demo tokens gated behind `testing-mocks`.
   - `GET /api/v1/events` SSE telemetry endpoint.
5. **Code Mode v2**:
   - Modularized `src/data_plane/codemode/` engine.
   - Recursive `$ref`/`$defs`, `enum`, `oneOf`/`anyOf`, and typed array TypeScript declarations.
   - VM support for `try/catch/finally`, `throw`, object/array destructuring, template literals, and `Promise.all`.
6. **CLI Onboarding**:
   - `omni import <path/to/mcp.json>` and `omni migrate --to-v6`.
