# OmniMCP `v6.5.0` Release Notes — Distributed Cluster HA, Host-Bridged Wasm Plugins, Live Multi-Protocol Wire, Bidirectional MCP & Governance Studio

**Release Date:** 2026-11-21

## Highlights

OmniMCP `v6.5.0` closes the remaining single-node and simulated runtime ceilings from `v6.0.0`, delivering a horizontally scalable, Wasm-extensible, multi-protocol enterprise MCP gateway with full bidirectional server-to-client governance and an upgraded Governance & Observability Studio.

### 1. Real PostgreSQL Wire Protocol v3.0 Cluster Backend & Runtime Storage Wiring (`src/storage/postgres_store.rs`)
- Pure safe Rust (`#![forbid(unsafe_code)]`) PostgreSQL v3.0 wire protocol client with `SCRAM-SHA-256`, cleartext/trust authentication, optional `rustls` SSLRequest upgrade, and parameterized Extended Queries (`Parse`/`Bind`/`Execute`/`Sync`).
- Idempotent `SCHEMA_SQL` initialization (`omni_servers`, `omni_diffs`, `omni_tool_groups`, `omni_suspended_frames`, `omni_audit_log`).
- Distributed atomic Compare-And-Swap (`DELETE FROM omni_suspended_frames WHERE execution_id = $1 RETURNING payload::text`) guaranteeing single-winner continuation resumption across $N$ horizontal gateway replicas.
- Dedicated `LISTEN omni_registry_changed` / `NOTIFY` cluster cache invalidation listener refreshing `MCPRegistry` + `InMemoryConfigCache` in real time.
- Cryptographic `AuditSink::verify_integrity()` streaming `omni_audit_log ORDER BY idx ASC` and verifying every SHA-256 `prev_hash -> current_hash` chain link.
- Dynamic `OMNI_STORAGE_BACKEND=wal|sqlite|postgres` + `OMNI_DATABASE_URL` runtime selection in `ProductionGatewayContext::new()` and `omni migrate --to-v6.5` with SHA-256 checksum verification.

### 2. Host-Bridged Wasm Plugins & Omni Wasm Guest ABI v1 (`src/data_plane/codemode/wasm_sandbox.rs`)
- Shared SHA-256 compiled `wasmi::Module` bounded LRU cache (64 entries) eliminating cold compilation overhead on warm invocations.
- **Omni Wasm Guest ABI v1**: guest `omni_alloc(i32) -> i32` + `omni_run(i32, i32) -> i64` over linear memory, paired with host imports `omni::log_utf8` (PII-redacted) and `omni::call_tool` (governed ABAC + Argument Guardrails + Schema Preflight + HITL mutation gates).
- Native zero-process `.wasm` tool plugins (`ProtocolType::WasmPlugin`) registered via `POST /api/v1/wasm-tools` or `omni wasm register`.

### 3. Non-Blocking Async TLS (`AsyncTlsStream`), Full REST Verbs/Templates & Live HTTP/2 gRPC / Connect-RPC
- Replaced blocking `stream.into_std()` + `spawn_blocking` TLS fallback with 100% non-blocking `AsyncTlsStream` (`tokio::io::AsyncRead` / `AsyncWrite` over `rustls::ServerConnection`), supporting concurrent HTTPS `/omni/mcp`, mTLS, and long-lived HTTPS `/api/v1/events` SSE streams.
- `run_server()` defaults to the non-blocking `tokio` async listener whenever `async-runtime` is compiled in.
- Fixed `ProtocolTranslator` to route both `http://` and `https://` REST endpoints through `HttpDownstreamClient::request()` with full `GET`/`POST`/`PUT`/`PATCH`/`DELETE` verbs, OpenAPI `{param}` URL template interpolation, query/body binding, and strict path-traversal rejection.
- Added `src/data_plane/downstream_grpc.rs` (`GrpcDownstreamClient`) implementing live RFC 7540 HTTP/2 gRPC (`h2c` & TLS `h2` ALPN) and Connect-RPC (`Connect-Protocol-Version: 1`, `application/connect+json`) wire execution feeding `CircuitBreakerRegistry` and Sagas.

### 4. Bidirectional Server-Initiated MCP (`sampling`, `elicitation`, `roots`, `progress`, `cancelled`)
- Added `UpstreamCallbackHub` coordinating reverse server-to-client `sampling/createMessage`, `elicitation/create`, and `roots/list` requests in both `StdioUpstreamSession` and `HttpUpstreamSession` SSE streams under `Deny`, `RelayToClient`, and `ApprovalGate` (with PII redaction and `maxTokens` caps).
- Forwarded streaming upstream `notifications/progress` frames onto `EventBus` (`event: mcp.progress`).
- Wired downstream `notifications/cancelled` (`requestId`) to propagate cancellation upstream and immediately unblock waiting caller channels.

### 5. Hybrid Okapi BM25 + Dense Vector RRF Discovery & Canonical Semantic Cache (`src/data_plane/semantic_index.rs`)
- Zero-external-API 256-dim L2-normalized local embedding engine (subword 3/4-grams + domain synonym-concept expansion) + optional remote HTTP embedding provider (`OMNI_EMBEDDING_URL`), fused with Okapi BM25 via Reciprocal Rank Fusion (RRF) across `discover_server`, `recommend_tools`, and `speculative_pre_inject`.
- Upgraded `SemanticResponseCache` with recursive canonical JSON key normalization, per-tool `cache_ttl_sec` overrides, vector-similarity fallback, and Prometheus hit/miss metrics.

### 6. `ChaCha20-Poly1305` AEAD Token Vault, Auto-Refresh OAuth, Argument ABAC & Tenant Quotas
- Upgraded `UpstreamOAuthTokenStore` to `ring::aead::CHACHA20_POLY1305` (`version: 2`, 96-bit OS CSPRNG nonces, server-name AAD) with transparent read-fallback for `v6.0.0` envelopes.
- Added automatic `grant_type=refresh_token` rotation on token expiry or HTTP `401`, plus browser PKCE endpoints `GET /api/v1/oauth/authorize/:server` and `GET /api/v1/oauth/callback`.
- Extended `CharterPolicy` with declarative JSON-pointer `ArgumentPolicyRule` guardrails (`-32001 UNAUTHORIZED`) and per-tenant sliding-window `TenantQuotaTracker` hourly budgets (`-32004 RATE_LIMITED` with `Retry-After`).

### 7. `src/server/` Modular Decomposition & Live Batching OTLP Worker
- Decomposed the 4,292-line `src/server.rs` monolith into `src/server/{mod,auth,http_codec,rest_api,mcp_router,sse,listener}.rs` while preserving 100% public symbol re-exports.
- Added `OtlpBatchWorker` (`src/telemetry/otlp.rs`) with bounded queue, periodic + threshold flushing (`OMNI_OTLP_FLUSH_INTERVAL_MS` / 128 spans), $p_{50}/p_{95}/p_{99}$ latency quantiles, error counters, and exponential retry backoff.

### 8. Governance & Observability Studio v3 (`ui/src/`)
- Added `ApprovalInboxModal.tsx` (live SSE-driven HITL Continuation & Upstream Callback Inbox with highlighted mutation lines, scope/arg previews, SHA-256 digest, separation-of-duties warnings, and 1-click Approve & Resume).
- Added `ObservabilityStudio.tsx` (W3C Trace Waterfall + OTLP quantiles + interactive Merkle Audit Explorer with live cryptographic chain verification).
- Added `ImportConfigModal.tsx` (browser `mcp.json` paste/upload wizard with `{env:...}` secret conversion preview) and 1-click OAuth PKCE Connect on `ExternalServerInspector.tsx`.
