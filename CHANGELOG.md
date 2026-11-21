# Changelog

All notable changes to Omni MCP. Per-release narrative notes live in [`docs/`](docs/).

## [6.5.0] - 2026-11-21

### Added
- **Real PostgreSQL Wire Protocol v3.0 Cluster Backend (`src/storage/postgres_store.rs`)**:
  - Pure safe Rust (`#![forbid(unsafe_code)]`) PostgreSQL v3.0 wire protocol client (`SCRAM-SHA-256`, optional `rustls` SSLRequest, parameterized Extended Queries), idempotent `SCHEMA_SQL`, distributed atomic CAS (`DELETE FROM omni_suspended_frames WHERE execution_id = $1 RETURNING payload::text`), `LISTEN / NOTIFY` cluster cache invalidation, and cryptographic `AuditSink::verify_integrity()`.
  - Dynamic `OMNI_STORAGE_BACKEND=wal|sqlite|postgres` + `OMNI_DATABASE_URL` runtime wiring in `ProductionGatewayContext::new()` and `omni migrate --to-v6.5`.
- **Host-Bridged Wasm Plugins & Omni Wasm Guest ABI v1 (`src/data_plane/codemode/wasm_sandbox.rs`)**:
  - SHA-256 compiled `wasmi::Module` bounded LRU cache, `omni_alloc` / `omni_run` JSON linear-memory ABI, `omni::log_utf8` (PII-redacted), and governed `omni::call_tool` host bridge.
  - Native `.wasm` plugin tools (`ProtocolType::WasmPlugin`) registered via `POST /api/v1/wasm-tools` or `omni wasm register`.
- **Non-Blocking Async TLS (`AsyncTlsStream`), Full REST Verbs/Templates & Live HTTP/2 gRPC / Connect-RPC**:
  - Native non-blocking `tokio` + `rustls` `AsyncTlsStream` replacing `stream.into_std()` + `spawn_blocking` downgrades, serving concurrent HTTPS `/omni/mcp`, mTLS, and long-lived HTTPS `/api/v1/events` SSE streams.
  - `HttpDownstreamClient::request()` supporting `GET`/`POST`/`PUT`/`PATCH`/`DELETE`, OpenAPI `{param}` URL templates with path-traversal rejection, and live `https://` routing in `ProtocolTranslator`.
  - Live RFC 7540 HTTP/2 gRPC (`h2c` / TLS `h2`) & Connect-RPC client (`src/data_plane/downstream_grpc.rs`).
- **Bidirectional Server-Initiated MCP (`sampling`, `elicitation`, `roots`, `progress`, `cancelled`)**:
  - `UpstreamCallbackHub` coordinating reverse server-to-client requests across `StdioUpstreamSession` and `HttpUpstreamSession` SSE under `Deny`, `RelayToClient`, and `ApprovalGate` policies, plus `notifications/progress` SSE forwarding and `notifications/cancelled` bridging.
- **Hybrid BM25 + Dense Vector RRF Discovery & Canonical Semantic Cache (`src/data_plane/semantic_index.rs`)**:
  - Zero-external-API 256-dim local semantic embedder + optional `OMNI_EMBEDDING_URL` fused with Okapi BM25 via RRF, plus recursively key-canonicalized `SemanticResponseCache` with per-tool `cache_ttl_sec`.
- **`ChaCha20-Poly1305` AEAD Token Vault, Auto-Refresh OAuth, Argument ABAC & Tenant Quotas**:
  - `ring::aead::CHACHA20_POLY1305` `v2` token store with transparent `v6.0.0` read fallback, automatic `401 -> refresh_token` rotation, PKCE browser endpoints (`/api/v1/oauth/authorize/:server`, `/api/v1/oauth/callback`), declarative `ArgumentPolicyRule` guardrails, and sliding-window `TenantQuotaTracker`.
- **`src/server/` Modular Decomposition & Live Batching OTLP Worker (`src/telemetry/otlp.rs`)**:
  - Decomposed `src/server.rs` into `src/server/{mod,auth,http_codec,rest_api,mcp_router,sse,listener}.rs` and wired background `OtlpBatchWorker` with $p_{50}/p_{95}/p_{99}$ latency histograms.
- **Governance & Observability Studio v3 (`ui/src/`)**:
  - Added `ApprovalInboxModal`, `ObservabilityStudio` (W3C Trace Waterfall + Merkle Audit Explorer), `ImportConfigModal` (`mcp.json` wizard), and 1-click OAuth PKCE Connect on `ExternalServerInspector`.

## [6.0.0] - 2026-11-13

### Added
- **Feature-Gated Hybrid Runtime (`Cargo.toml`)**:
  - Added `os-csprng` (`getrandom`) in default features for OS CSPRNG entropy in `gather_entropy()`.
  - Added optional `tls-rustls` (`rustls` + `webpki-roots`) for native HTTPS upstream/downstream TLS without breaking cross-compilation.
  - Gated synthetic 3P demo token minting behind `#[cfg(any(feature = "testing-mocks", test))]` and added RFC 8693 OAuth token exchange (`OMNI_3P_TOKEN_ENDPOINT`).
- **Storage & Audit v2**:
  - Pluggable `RegistryStorageBackend` trait in `src/storage/wal_store.rs` with `ToolGroupRecord` WAL & snapshot persistence.
  - Bounded `VecDeque` sliding-window Merkle audit log with automatic segment rotation and HMAC-SHA256 signed `.ckpt` checkpoints verified by `verify_chain()`.
- **Full MCP Aggregation & Tool Groups**:
  - Virtual Tool Groups (`/api/v1/groups`, `POST /groups/<slug>/mcp`).
  - External `resources/*` (`ext://<slug>/<uri>`) and `prompts/*` (`<slug>:<prompt>`) aggregation in `OmniMCPServer`.
  - Concurrent ID-multiplexed stdio reader (`src/upstream/stdio.rs`) with per-tool `timeout_ms`.
  - Streamable HTTP SSE (`text/event-stream`) client response parsing and `Last-Event-ID` tracking (`src/upstream/http.rs`).
  - Opt-in background `AutoSyncScheduler` when `sync_interval_sec` is set.
- **Auth & Live Events**:
  - HS256 JWT validation with `exp`, `nbf`, `iss`, and RFC 8707 `aud` checks (`OMNI_JWT_SECRET`).
  - `GET /api/v1/events` SSE endpoint and CIMD metadata in `/.well-known/oauth-protected-resource`.
- **Code Mode v2 & Modularization**:
  - Split monolithic `codemode.rs` into `src/data_plane/codemode/{mod,typegen,parser,parallelizer,columnar,vm,tests}.rs`.
  - Recursive `$ref`/`$defs`, `enum`, `oneOf`/`anyOf`, typed arrays, and JSDoc comments in `CodeModeTypeGenerator`.
  - `try/catch/finally`, `throw`, object/array destructuring, template literals, and `Promise.all` parallel batching in the Code Mode VM.
- **CLI Onboarding**:
  - `omni import <mcp.json>` (converting plaintext `env` secrets to `{env:KEY}` refs) and `omni migrate --to-v6`.

## [5.0.0] - 2026-11-01

### Added
- **Full MCP `2026-07-28` Protocol Conformance (B1–B10)**:
  - Required `"resultType": "complete"` on successful responses and `"resultType": "input_required"` with `requestState` for multi-round-trip requests (B1).
  - Deterministic tool, resource, and prompt sorting with caching directives (`ttlMs` and `cacheScope: "workspace"`) on catalog endpoints (B2).
  - Streamable HTTP `Mcp-Method` and `Mcp-Name` header enforcement and echo on POST `/omni/mcp` and `/<service>/mcp` (B3).
  - Strict client capability validation (`-32021`) on `initialize`, and pruning of deprecated endpoints (`ping`, `logging/setLevel`, `roots/list_changed`) under `2026-07-28` (B4).
  - Spec-reserved error code allocation (`-32020..-32099` for spec definitions, `-32000..-32019` for gateway implementation codes) (B5).
  - RFC 9728 protected-resource metadata (`/.well-known/oauth-protected-resource`), HTTP 401 `WWW-Authenticate: Bearer resource_metadata=...`, and HTTP 403 `insufficient_scope` (B6).
  - Tasks extension (`io.modelcontextprotocol/tasks`) with `tasks/get` and `tasks/update` endpoints (B7).
  - Feature-gated `/sse` transport returning `410 Gone` by default (enabled via optional `--features legacy-sse`) (B8).
  - Structural response validation via `validate_mcp_response` across all supported protocol revisions (B10).
- **Architecture, Stability & Tooling (D7, D8, C1)**:
  - $O(1)$ ring-buffer trace eviction via `VecDeque<TraceSpan>` in production gateway context (D7).
  - Versioned REST routes `/api/v1/*` aliased to `/api/*` and OpenAPI 3.1 specification at `/openapi.json` and `/api/v1/openapi.json` (D8).
  - Database and configuration state migration tool via `omni-gateway migrate` (D8).
  - Subcommand execution supporting direct commands (`list`, `search`, `call`, `bench`, `codemode`, `types`, `resume`, `simulate`, `migrate`) without requiring `mcp` prefix (C1).

### Changed
- **Breaking**: Gateway implementation error codes renumbered from `-32029..-32038` to `-32004..-32013`, with backward-compatible `data.legacy_code` preserved in responses.
- **Breaking**: All `/api/*` and `/api/v1/*` endpoints require Bearer authentication with role-based access control (`Agent`, `Operator`, `Approver`, `Admin`).
- **Breaking**: Default bind address is `127.0.0.1`. Non-loopback network binds strictly require `OMNI_AUTH_SECRET`.

## [5.0.0-alpha.2] - 2026-10-31

### Added
- Phase 1 & 2 Trust Boundary Reset:
  - Atomic compare-and-swap (CAS) frame consumption under write lock to eliminate concurrent double-resume vulnerability (A5).
  - Approver argument digest verification preventing argument manipulation between suspension and resumption (A5).
  - Strict HTTP request framing: persistent `BufReader` across keep-alive loops, digit-only Content-Length validation, rejection of chunked transfer-encoding without content-length with 501, and connection termination on body size limit violations (A9).
  - Worker pool resilience: `std::panic::catch_unwind` boundary preventing worker thread death on panic, and bounded queue with 503 Service Unavailable when saturated (A8, A14).
  - Hardened authentication principal extraction: rate-limiting keys and actor identities derived strictly from authenticated claims, discarding client-supplied `X-Caller-Actor` (A12).
  - Bounded rate limiter state with TTL-based and LRU eviction limiting memory consumption (A12).
  - Fsync durability: added `flush()` and `sync_all()` on WAL mutations, snapshot renames, and audit trail appends (A13).
  - Protocol negotiation honesty: `2026-07-28` set to `implemented: false` until Part B protocol changes are fully implemented, falling back cleanly to `2025-11-25`.

### Changed
- CLI enhancement: Subcommand dispatch for `--version`, `-V`, `version`, and `aifx ...` commands (C1).
- Scrubbed inaccurate Rhai compatibility claims; documented as JS/TS syntax with closure support (C3).
- Neutral branding & demo catalog: Scrubbed Uber-specific naming references and "Rust 2.0" branding across codebase, adopting neutral fleet/rides service naming with backward-compatible aliases (C6).

## [4.7.1] - 2026-10-31

### Fixed
- Hotfix for Step 0 & Phase 1 trust boundary items (A1, A2, A3, A4, A7, A10, A11 default bind & secret validation).

## [4.7.0] - 2026-10-30

### Added
- Per-revision MCP protocol capability table and `negotiate_protocol_version()` (only revisions flagged `implemented` are offered).
- Error codes `-32036` MEMORY_EXHAUSTED, `-32037` TIMEOUT, `-32038` APPROVAL_INVALID (reserved/defined).
- Cargo package metadata (license, repository, homepage, keywords, categories).
- Version drift guard v2 (README links, Dockerfile label, npm wrapper, Cargo.lock, `docs/` release notes).

### Changed
- Release/roadmap documents moved to `docs/`.
- README: wire translation and Muttley delegation relabelled **Simulated** (in-memory); `/sse` marked deprecated.

## [4.6.0]
See [docs/RELEASE_v4.6.0.md](docs/RELEASE_v4.6.0.md).

## [4.5.0]
See [docs/RELEASE_v4.5.0.md](docs/RELEASE_v4.5.0.md).
