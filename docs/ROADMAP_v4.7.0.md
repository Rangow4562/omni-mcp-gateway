# Omni MCP v4.7.0 Feature Roadmap — "Protocol Parity & Real Wire"

<p align="center">
  <img src="https://raw.githubusercontent.com/Rangow4562/omni-mcp-gateway/main/assets/omni-mcp-eink-banner.png" alt="Omni MCP Banner" width="100%" />
</p>

## 1. Theme & Context

Omni MCP sits at **v4.6.0** after delivering a Code Mode unit test floor, true Okapi BM25 ranking, the authoritative `-320xx` error contract, and MCP `2024-11-05` / `2025-03-26` version negotiation. Three structural gaps remain for v4.7:

1. **Protocol Debt**: The gateway negotiates only two MCP revisions. The current MCP specification revision is `2026-07-28` (replacing `2025-11-25`), which makes the protocol stateless, adds `server/discover`, and deprecates HTTP+SSE. The `2025-06-18` surface was deferred in v4.6 and is now three revisions behind.
2. **Simulated Wire**: gRPC/TChannel/HTTP downstream calls run through in-memory translators and the `MuttleyMeshClient` simulator. v4.7 adds the first real network transport.
3. **Trust Boundary Debt**: Suspended continuations are protected only by FNV-1a (corruption detection, not tamper resistance), approvals are free-text identities, and sandbox limits cover instruction fuel but not memory or wall-clock time.

**v4.7 Theme**: *Speak the current MCP protocol, make the first downstream call real, and make every approval, continuation, and release artifact verifiable — while preserving the zero-runtime-dependency (`std`-only) guarantee.*

> **Dependency policy clarification**: "Zero-dependency" means zero **runtime** dependencies. Dev-only tooling (fuzzers, benchmark harnesses) may be used but never linked into release binaries.

---

## 2. Marquee Features ("What's New in v4.7")

### M1. 🤝 MCP Protocol Parity (`2025-06-18`, `2025-11-25`, `2026-07-28`)

- **What**:
  - Extend the supported set to `{"2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25", "2026-07-28"}` with a per-version capability table driving behavior (not scattered `if` checks).
  - **Legacy (session-based) revisions**: keep the v4.6 `initialize` negotiation (echo supported client version, else latest supported).
  - **`2026-07-28` (stateless)**:
    - Read `io.modelcontextprotocol/protocolVersion` and `io.modelcontextprotocol/clientCapabilities` from each request's `_meta`.
    - Return `UnsupportedProtocolVersionError` on mismatch instead of silently falling back.
    - Implement the required `server/discover` RPC (supported versions, capabilities, server identity).
    - Replace the session notification stream with `subscriptions/listen`.
    - Advertise `extensions` in server capabilities.
  - **Transport**: implement Streamable HTTP (single MCP endpoint, POST/GET) on `/omni/mcp` and `/<service-name>/mcp`. Keep `/sse` as an explicitly **deprecated legacy** endpoint.
  - **Batching**: gate JSON-RPC array batching and `batch_invoke_tools` array envelopes to legacy negotiated versions only (batching removed in newer spec).
  - **Trace context**: read/write `traceparent`, `tracestate`, `baggage` in request/response `_meta` and bridge them to the existing W3C tracing collector.
  - **Error codes**: realign resource-not-found to `-32602` for `2026-07-28` clients and migrate `-32002 TOOL_DISABLED` into the gateway-owned `-320xx` block (alias retained for legacy clients for one release).
  - **Newer tool surface** (additive, version-gated): `title`, structured tool output (`outputSchema` / `structuredContent`), and resource links in tool results, where the negotiated revision supports them.
- **Why**: Modern agent harnesses increasingly speak `2025-11-25` and `2026-07-28`. Falling back to `2025-03-26` is no longer a safe default.

### M2. 🌐 Live HTTP/1.1 Downstream Transport (`transport-http`)

- **What**: Feature-gated, `std`-only HTTP/1.1 JSON downstream client alongside the in-memory `MuttleyMeshClient`.
  - Connect/read/write timeouts, bounded retries with jitter, and keep-alive connection pooling.
  - Failures feed the per-service circuit breaker (`-32030`) and the Saga unwinder (`-32034`).
  - Request/response size caps; no TLS in-process (documented: terminate TLS at a sidecar or reverse proxy).
  - gRPC and TChannel remain clearly labeled as in-memory envelope translators.
- **Why**: Turns the first downstream path from simulated into real while keeping the attack surface small (no hand-rolled HTTP/2).

### M3. 📋 Schema Preflight Validator (`-32035 SCHEMA_PREFLIGHT_FAILED`)

- **What**: Static AST argument type-checking against registered `ToolRecord.input_schema` before execution.
  - Collect **all** violations in one pass and return them in `RPCError.data` (path, expected type, actual type) so an LLM can self-correct in a single turn.
  - Wire the `explain_error` tool to surface preflight diagnostics.
  - Preflight runs before approval gates, so humans never approve calls that cannot succeed.
- **Why**: Promotes the reserved `-32035` code from placeholder to shipped behavior.

### M4. 🔐 Approval & Continuation Hardening + Persistent Saga Ledger

- **What**:
  - **Authenticated frames**: replace FNV-1a as the *integrity* mechanism for `SuspendedExecutionFrame` with HMAC-SHA256 (using the existing in-tree SHA-256) under a configured key. FNV-1a stays as the WAL corruption check.
  - **Approval binding**: approvals bind to an authenticated approver identity, a SHA-256 hash of the exact pending arguments, an expiry timestamp, and single-use semantics. Resuming with changed arguments is rejected.
  - **New code**: `-32038 APPROVAL_INVALID` for expired, replayed, mismatched, or unauthenticated approvals.
  - **Persistent Saga compensation ledger** in `WalRegistryStore`:
    - Idempotency keys on every compensation.
    - Startup recovery pass for in-flight chains.
    - Defined behavior when a compensation itself fails (`AUDIT_STATUS_SAGA_UNCOMPENSATED` plus operator-visible alert).
  - **Randomness audit**: execution IDs and tokens must come from the OS CSPRNG, never from the DST `Xoshiro256StarStar` generator.
- **Why**: Closes the gap where write access to the WAL, or a guessed approver string, could bypass a human-in-the-loop gate.

### M5. 🧱 Sandbox Resource Limits

- **What**: Extend fuel exhaustion with:
  - **Memory budget** for string/array/object growth: `-32036 MEMORY_EXHAUSTED`.
  - **Wall-clock timeout**: `-32037 TIMEOUT`.
  - **Recursion depth** and **`ParallelWave` thread** caps.
  - Panic containment at the sandbox boundary (keep unwinding panics; do not set `panic = "abort"`).
- **Why**: Instruction fuel alone does not bound allocation-heavy or blocking scripts.

---

## 3. Hygiene & Structural Hardening Table

| # | Item | Target Files | Action |
| :--- | :--- | :--- | :--- |
| **H1** | **HTTP Surface Hardening** | `src/server.rs`, `src/data_plane/gateway.rs` | Validate `Origin` (DNS-rebinding protection for localhost), require auth on `/api/execute` and `/api/audit*`, cap header/body sizes, enforce read timeouts (slowloris), add request-smuggling regression tests. |
| **H2** | **Fuzzing & Differential Tests** | `tests/` | Differential tests comparing columnar `PhysicalExpr` evaluation with row-by-row evaluation. Add `#![forbid(unsafe_code)]` if no `unsafe` exists. |
| **H3** | **Release Supply Chain** | `.github/workflows/`, `bin/`, `package.json` | Publish `SHA256SUMS`, sign artifacts, verify checksums in the npx wrapper. |
| **H4** | **Cargo Metadata** | `Cargo.toml` | Add `license = "Apache-2.0"`, `repository`, `keywords`, `categories`. |
| **H5** | **CI Platform Matrix** | `.github/workflows/rust.yml` | Run `cargo test` on Linux, macOS, and Windows. |
| **H6** | **Benchmark & Quality Evidence** | `docs/` | Document benchmark methodology and BM25 precision metrics. |
| **H7** | **Doc & Metadata Honesty Sweep** | `README.md` | Relabel "Verified" rows for wire translation and Muttley delegation as "simulated" until M2 lands. |
| **H8** | **Repository Layout Migration** | Top-level files | Archive `RELEASE_v*.md` into `docs/`. Add `CHANGELOG.md`, `SECURITY.md`, `CONTRIBUTING.md`. |
| **H9** | **Drift Guard Expansion** | `src/data_plane/gateway.rs` | Extend `test_version_single_sourcing_drift_guard()` to check `Dockerfile` and `bin/omni-mcp.js`. |
| **H10** | **Release Notes** | `docs/RELEASE_v4.7.0.md` | Author v4.7.0 release notes with upgrade guide and deprecation notice for `/sse`. |

