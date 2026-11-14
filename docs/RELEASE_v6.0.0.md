# OmniMCP v6.0.0 Release Notes

**Release Date**: 2026-11-14  
**Upgrade Tier**: Major (`5.5.1` -> `6.0.0`)  
**NPM Package**: `@rangow2260/omni-mcp@6.0.0`  
**GitHub Release**: [`v6.0.0`](https://github.com/Rangow4562/omni-mcp-gateway/releases/tag/v6.0.0)

---

## Highlights

### 1. Async Runtime & Native `rustls` TLS/mTLS (`async-runtime`, `tls-rustls`)
- **`tokio` Multi-Threaded Async Server**: `serve_async_listener` and `run_async_server` enable high-concurrency async TCP/TLS acceptance.
- **Native `rustls` TLS 1.2/1.3 & mTLS**: Server-side TLS and mutual TLS client-certificate verification alongside `rustls` + `webpki-roots` HTTPS upstream/downstream connections.
- **OS CSPRNG (`os-csprng`)**: Enabled by default via `getrandom` for cryptographic ULIDs, PKCE verifiers, and continuation secrets.

### 2. Asymmetric OIDC / JWKS Auth & Token Exchange
- **OIDC / JWKS (`RS256`, `ES256`, `EdDSA`)**: Full asymmetric JWT signature verification alongside HS256 JWT validation with RFC 8707 `aud` checking (`OMNI_JWT_SECRET`, `OMNI_JWT_ISSUER`, `OMNI_JWT_AUDIENCE`, `OMNI_JWKS_URL`).
- **RFC 8693 3P OAuth Token Exchange**: Production token exchange via `OMNI_3P_TOKEN_ENDPOINT` (with synthetic demo tokens strictly gated behind `testing-mocks`).

### 3. SQLite WAL Storage, CAS & Segmented Merkle Audit (`storage-sqlite`)
- **SQLite WAL Persistence (`WalRegistryStore` / `RegistryStorageBackend`)**: Durable SQLite Write-Ahead Log storage with atomic compare-and-swap (CAS) continuation consumption.
- **Segmented Audit Trail & `AuditSink`**: Bounded sliding-window `MerkleAuditStore` with automatic JSONL segment rotation, HMAC-SHA256 signed `.ckpt` checkpoints, and SQLite audit sink persistence.

### 4. Wasm Sandbox (`sandbox-wasm`) & OS Process Containment (`crates/omni-process-guard`)
- **`wasmi` Fuel-Metered Wasm Sandbox**: Isolated WebAssembly execution engine (`src/data_plane/codemode/wasm_sandbox.rs`) alongside the sub-millisecond AST Code Mode VM.
- **Process Containment Crate (`crates/omni-process-guard`)**: Cross-platform child process tree cleanup using Windows Job Objects (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) and Linux `PR_SET_PDEATHSIG` + process-group `killpg`.

### 5. Unified External MCP Fleet, Virtual Tool Groups & Studio UI
- **Curated Virtual Tool Groups**: Manage groups via `/api/v1/groups` and invoke scoped group endpoints at `POST /groups/<slug>/mcp`.
- **External Resources & Prompts Aggregation**: Unified `resources/*` (`ext://<slug>/<uri>`) and `prompts/*` (`<slug>:<prompt>`) across external stdio and Streamable HTTP (`text/event-stream`) upstreams, plus opt-in `AutoSyncScheduler`.
- **Fleet & Studio UI + Live SSE**: React 18 + Vite UI (`ToolGroupsStudio`, `CodeModeStudio`) connected to `GET /api/v1/events` SSE telemetry, verified with Playwright E2E (`ui/tests/e2e/fleet.spec.ts`).

### 6. Modular Code Mode v2, OpenTelemetry & CLI Onboarding
- **Modularized `src/data_plane/codemode/` & `src/telemetry/`**: Dedicated modules for parser, VM, SSA auto-parallelizer, columnar predicate pushdown, TypeScript `.d.ts` generator, Wasm sandbox, and OTLP exporter.
- **Code Mode v2 Typegen & VM**: Recursive `$ref`/`$defs`, `enum`, `oneOf`/`anyOf`, typed array `.d.ts` output, plus VM support for `try/catch/finally`, `throw`, object/array destructuring, template literals, and `Promise.all`.
- **CLI Onboarding**: `omni import <path/to/mcp.json>` and `omni migrate --to-v6`.

---

## Multi-OS Release Assets

Pre-built release binaries for `omni-gateway`, `omni`, and `aifx` (with Cargo SBOMs and `SHA256SUMS`):

| OS | Architecture | Gateway Binary |
| :--- | :--- | :--- |
| **Linux (glibc)** | `x86_64` | `omni-gateway-linux-x64` |
| **Linux (musl)** | `x86_64` | `omni-gateway-linux-x64-musl` |
| **Linux (glibc)** | `aarch64` | `omni-gateway-linux-arm64` |
| **Linux (musl)** | `aarch64` | `omni-gateway-linux-arm64-musl` |
| **macOS** | Apple Silicon (`arm64`) | `omni-gateway-darwin-arm64` |
| **macOS** | Intel (`x64`) | `omni-gateway-darwin-x64` |
| **Windows** | `x64` | `omni-gateway-win32-x64.exe` |
| **Windows** | `arm64` | `omni-gateway-win32-arm64.exe` |

---

## Quick Install

```bash
# NPX zero-install (Windows, macOS, Linux)
npx -y @rangow2260/omni-mcp --stdio
npx -y @rangow2260/omni-mcp 8080

# Docker
docker build -t omni-mcp:v6.0.0 .
docker run --rm -e OMNI_AUTH_SECRET="your-secret" -p 8080:8080 omni-mcp:v6.0.0
```

**Full Changelog**: https://github.com/Rangow4562/omni-mcp-gateway/compare/v5.5.1...v6.0.0
