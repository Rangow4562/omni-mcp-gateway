# Omni MCP v5.0.0 Roadmap — "Trust Boundary Reset"

<p align="center">
  <img src="https://raw.githubusercontent.com/Rangow4562/omni-mcp-gateway/main/assets/omni-mcp-eink-banner.png" alt="Omni MCP Banner" width="100%" />
</p>

## 0. Evidence legend and scope

This roadmap comes from auditing commit `dd1d818` (`main`, 2026-10-30) against the v4.7.0 README/release notes and the MCP `2026-07-28` specification changelog.

| Tag | Meaning |
| :--- | :--- |
| **[EXEC]** | Reproduced by running a local release build (`127.0.0.1`, throwaway data dir). |
| **[CODE]** | Confirmed by reading the source (file/line given). Not executed. |
| **[SPEC]** | Checked against the MCP `2026-07-28` changelog on modelcontextprotocol.io. |

**Limits of the audit.** Shallow clone (no history). Not exercised: the `transport-http` live downstream path, the React UI, the gRPC/TChannel translators, fuzzing, and the GitHub Actions runs. Release-workflow findings are from reading the YAML. The npm registry could not be queried from the audit sandbox, so anything about what is actually published on npm is unverified.

Baseline health: `cargo build --release` OK (zero dependencies, ~45 s); `cargo test` 24 unit + 33 integration tests pass, also with `--features transport-http`; `cargo fmt --check` clean; **`cargo clippy -D warnings` fails** (see D4).

---

## 1. Theme

**v4.7 added many features, but the trust boundary underneath them does not hold.** Authentication accepts any string, the approval gate can be passed by the caller it is meant to constrain, and several security claims in the README/release notes are not true of the code. v5.0 is a breaking release whose job is to make the trust claims real, make the `2026-07-28` support real, and make the documentation match the code.

v5.0 pillars:

1. **Real authn/authz, secure by default.**
2. **A hardened HTTP front door** (no remote crash, no starvation, strict framing).
3. **True MCP `2026-07-28` conformance**, including the new error-code allocation policy.
4. **Claims = evidence.** Every README claim links to a test or benchmark.
5. **Verifiable releases** (checksums, signatures, provenance).

---

## 2. Step 0 — ship a 4.7.1 hotfix before any 5.0 work

Several findings are exploitable on the published 4.7.0 defaults (bind `0.0.0.0`, no real auth). Do not wait for 5.0:

- Make `is_authorized` validate against a configured key (A1) and gate every `/api/*` route except `/healthz`, `/readyz` (A2).
- Use `OMNI_AUTH_SECRET` for frame HMACs and reject empty `frame_hmac` (A4).
- Add a JSON nesting limit (A7).
- Parse `Origin` properly and reject `null` (A10).
- Default `OMNI_HOST` to `127.0.0.1` (A11).
- Publish a GitHub security advisory per `SECURITY.md`, and deprecate 4.7.0 on npm with a pointer to 4.7.1.

---

## 3. Part A — P0 security and trust-boundary fixes (release blockers)

### A1. Authentication is a no-op
- **Finding:** Any non-empty Bearer token other than the literal `anonymous` is accepted. `OMNI_AUTH_SECRET` is never used to validate anything.
- **Evidence:** `server.rs:388-399` **[CODE]**. `/api/audit` returned 401 with no token, 200 with `Bearer x` **[EXEC]**.
- **Fix:** Pluggable authenticator: (a) hashed static API keys with constant-time compare for local/dev mode; (b) JWT/OAuth 2.1 resource-server validation (signature via JWKS, `iss`, `aud`, `exp`) for network mode. Refuse to start on a non-loopback bind without one configured.

### A2. Control-plane and Code Mode endpoints are unauthenticated
- **Finding:** Only 3 routes check auth (`/api/audit`, `/api/audit/verify`, `/api/execute`). Everything else is open, including `toggle_status`, `propose_update`, `diffs/approve`, `tools/rollback`, `crawler/trigger`, `codemode/eval|resume|simulate`, `/api/traces`, and `/api/codemode/suspended`.
- **Evidence:** `server.rs` routes at ~505-1090 **[CODE]**. An unauthenticated POST enabled the disabled `3p_jira` server **[EXEC]**. `/api/traces`, `/api/codemode/suspended`, `/api/crawler/trigger` returned 200 with no credentials **[EXEC]**.
- **Fix:** Central deny-by-default middleware with a route→required-role table (`agent`, `operator`, `approver`, `admin`). Never rely on per-handler checks. Add an auth-matrix test: every route × {no token, bad token, wrong role, right role}.

### A3. The human-approval gate can be passed by the requester
- **Finding:** The same unauthenticated caller can trigger a mutation, list the pending frame, and approve it. `approver` is a free-text string; `/api/codemode/resume` defaults it to the `X-Caller-Actor` header or `"agent"`, and only `""`/`"anonymous"` are rejected.
- **Evidence:** `server.rs:1051-1068`, `entities.rs:575-585` **[CODE]**. Full chain reproduced with no credentials: eval with `require_mutation_approval:true` → `APPROVAL_REQUIRED` → `GET /api/codemode/suspended` → `POST /api/codemode/resume` → `payments_charge` executed **[EXEC]**.
- **Fix:** Approver must be an authenticated principal with an `approve` permission, and must differ from the requester. Bind the frame to the requester's principal. `/api/codemode/suspended` must require a role and redact `script_source`, `pending_arguments`, `serialized_scope`, and `frame_hmac`. Consider implementing approvals via MRTR `input_required` (B1).

### A4. Continuation HMAC uses a hard-coded public key, and verification can be skipped
- **Finding:** Frames are signed and verified with the literal `"omni-secret-key"`; `config.auth_secret` is never read. `verify_approval` skips the HMAC check entirely when `frame_hmac` is empty. The HMAC covers only 7 fields, not `script_source`, `serialized_scope`, `pending_arguments`, or `caller_actor`. Comparison is not constant-time; the canonical string is `:`-joined without length prefixes. The config default secret is the public string `omni_enterprise_secret_key`.
- **Evidence:** `codemode.rs:2829`, `gateway.rs:1196-1197`, `entities.rs:559-571, 591`, `config.rs:75-76` **[CODE]**.
- **Fix:** Key from required config (no default; fail startup if unset in network mode), with key id + rotation. HMAC over a length-prefixed canonical encoding of the **entire** frame. Reject empty/missing signatures. Constant-time compare. Re-hash `pending_arguments` and `script_source` against their stored hashes on resume.

### A5. "Changed arguments are rejected" is vacuous, and single-use is racy
- **Finding:** `resume_codemode` passes the frame's own stored arguments as `resume_args`, so it compares the frame to itself. No caller-supplied arguments are ever checked. Consumption is get → verify → … → save with no lock held between them.
- **Evidence:** `gateway.rs:1196-1200`, `registry.rs:483-503` **[CODE]**. Sequential replay is correctly rejected with `-32038` **[EXEC]**; the concurrent case was not run.
- **Fix:** Atomic `consume_frame(execution_id)` (compare-and-swap under the write lock, persisted and fsynced *before* executing the mutation). Pass an idempotency key to the downstream call. Add a concurrent double-resume test. Resume should accept an optional approved-arguments digest from the approver.

### A6. "OS CSPRNG" is not a CSPRNG
- **Finding:** `gather_entropy` hashes wall-clock nanos, PID, thread-id string, a stack address, and a counter with SHA-256. All of these are guessable or low-entropy, so execution IDs (the approval handles) are predictable. The v4.7 release notes and roadmap state OS CSPRNG.
- **Evidence:** `entities.rs:52-76`, `docs/RELEASE_v4.7.0.md` §4 **[CODE]**.
- **Fix:** Use the OS CSPRNG (`getrandom` syscall / `BCryptGenRandom` / `/dev/urandom`). With `#![forbid(unsafe_code)]` there is no std-only way to do this portably, so this is the strongest argument for allowing a tiny audited dependency (see Decision D1). Use a separate ≥128-bit random approval nonce rather than the ULID.

### A7. One unauthenticated request can crash the whole gateway
- **Finding:** The JSON parser is recursive with no depth limit. A ~195 KiB body of `[` overflows the worker stack and aborts the process; `catch_unwind` cannot catch a stack overflow. The body is parsed before auth on most routes.
- **Evidence:** `json.rs:326-495` (no depth tracking) **[CODE]**. `POST /omni/mcp` with 200,000 `[` → process exit 134, `fatal runtime error: stack overflow` **[EXEC]**.
- **Fix:** Depth limit (default 64, configurable) in the JSON parser and in the Code Mode parser/evaluator; explicit worker thread stack size; cap total parsed nodes. Add parser fuzz targets (D3).

### A8. Worker-pool starvation (slowloris protection is incomplete)
- **Finding:** The 10 s timeout applies per `read` call, not per request, so a client sending a byte every few seconds holds a worker indefinitely. The pool is small (default `max(8, 2×cores)`) and the accept queue is an unbounded `mpsc` channel.
- **Evidence:** `server.rs:1181-1204, 1300` **[CODE]**. With 8 workers, 12 trickling connections made `/healthz` time out **[EXEC]**.
- **Fix:** Total deadlines (headers, body, idle keep-alive), max connections per IP/total, bounded accept queue with `503` load shedding, `catch_unwind` per connection with worker respawn. Or move to an async server (D1).

### A9. HTTP request-framing bugs
- **Finding:** (1) `Transfer-Encoding: chunked` without `Content-Length` is accepted and the body is ignored. (2) Invalid `Content-Length` (`abc`, `+N`) silently becomes `0`. When the body arrives in a later packet, both cause the body to be parsed as a *new request*. (3) A fresh `BufReader` is created per request, so read-ahead bytes are discarded and pipelined requests are dropped. (4) `read_line` has no per-line cap, so a single huge header line is buffered before the 64 KB check. (5) When `Content-Length` exceeds the body cap, only `max+1` bytes are read and the connection is not closed, so leftover bytes are parsed as the next request. (6) The status-text map lacks 401 and 403, so they are sent as `Internal Server Error`.
- **Evidence:** `server.rs:1299-1396` **[CODE]**. (1)(2) produced 2 responses on one connection when the body was sent in a second packet; (3) two pipelined GETs in one segment produced 1 response; (6) `HTTP/1.1 401 Internal Server Error` **[EXEC]**.
- **Fix:** Strict parser: reject any `Transfer-Encoding` (501) or implement chunked decoding; `Content-Length` digits only; reject duplicates; one buffered reader per connection; bounded line reads; close the connection on every error path. Add raw-socket smuggling/pipelining regression tests (roadmap item H1 claimed these but none exist).

### A10. Origin / CORS validation is bypassable
- **Finding:** `starts_with("http://localhost")` also matches `http://localhost.evil.com`; `Origin: null` is always allowed; `Access-Control-Allow-Origin: *` is hard-coded on one path and the raw config string (possibly a comma list, which is invalid) is emitted on another; no `Host` allowlist.
- **Evidence:** `server.rs:73-101, 630, 1400-1408` **[CODE]**. With `Host: localhost`, `Origin: http://localhost.evil.com` → 200 and `Origin: null` → 200 (both should be 403) **[EXEC]**. The response writer also appends `Access-Control-Allow-Origin: <raw config string>` to every response, including 403s **[EXEC]**.
- **Fix:** Parse the origin URL; exact scheme/host/port match against an allowlist; reject `null`; reflect a single matched origin with `Vary: Origin`; validate `Host` against an allowlist (real DNS-rebinding protection).

### A11. Insecure defaults and mock behavior in the production path
- **Finding:** Defaults are `OMNI_HOST=0.0.0.0`, `OMNI_CORS_ORIGINS=*`, `OMNI_ENV=production`, and a public default secret. A fresh registry is seeded with demo servers, including **ENABLED** `payments_charge`/`payments_refund`/refund tools, contradicting "disabled by default". `exchange_token` returns a fabricated `oauth_3p_<provider>_<millis>` token for any bearer starting with `uber_token_` or `emp_`.
- **Evidence:** `config.rs:44-80`, `third_party.rs:43-50` **[CODE]**. `mcp list` on an empty data dir shows `paymentsservice ENABLED 2/2` **[EXEC]**.
- **Fix:** Loopback bind by default; refuse non-loopback without auth configured; no seed data unless `--demo`; demo/mock token exchange only under a `testing` feature; implement real RFC 8693 token exchange or drop the 3P exchanger.

### A12. Rate limiting and ABAC trust client-supplied identity
- **Finding:** The actor class (`human`/`service`/`agent`) comes from the `X-Caller-Actor` header. The rate-limit key is `token:actor`, both attacker-controlled, in an unbounded map with no eviction, so rotating tokens gives fresh buckets and grows memory.
- **Evidence:** `server.rs:781-800`, `rate_limiter.rs:29-91` **[CODE]**.
- **Fix:** Derive actor and rate key from the authenticated principal (plus IP); TTL/LRU eviction; retire `X-Caller-Actor`.

### A13. Durability claims ("crash-safe WAL / Saga ledger") are not met
- **Finding:** There are zero `sync_all`/`sync_data` calls in the codebase. The WAL uses `File::flush()` (a no-op for an unbuffered `File`); snapshot compaction does write-tmp + `rename` with no fsync of file or directory; write errors are discarded with `let _ =`. The audit log is appended the same way. The Merkle chain is checked only against what is on local disk, so truncating the tail is undetectable.
- **Evidence:** `wal_store.rs:191-232`, `audit_log.rs:331-333`, `grep sync_all\|sync_data src` = 0 **[CODE]**.
- **Fix:** Configurable fsync policy (`always` | `batch` | `off`, default `always` for the Saga ledger and approval frames); fsync tmp file + rename + parent dir; propagate write errors (mark `/readyz` unhealthy); periodic signed checkpoints or an external anchor for the audit chain; log rotation. Add a kill -9 crash-recovery test.

### A14. Panic and lock-poisoning resilience
- **Finding:** `catch_unwind` exists only around the Code Mode evaluator. A panic in any other handler kills that worker thread permanently (no respawn), and 79 `lock().unwrap()`/`read().unwrap()`/`write().unwrap()` sites turn one poisoned lock into cascading panics.
- **Evidence:** `server.rs:1189-1198`, `grep` count **[CODE]**.
- **Fix:** Per-connection `catch_unwind` + worker respawn; poison-tolerant locking helper; `/readyz` reports live worker count.

---

## 4. Part B — MCP `2026-07-28` conformance

v4.7 marks `2026-07-28` as `implemented: true` in `PROTOCOL_CAPABILITY_TABLE`. Against the `2026-07-28` changelog **[SPEC]**, a search of `src/` finds none of the following **[CODE]**. Until B1-B5 land, flip that row to `implemented: false` so the gateway does not advertise a revision it does not meet.

| ID | Requirement (per changelog) | Repo status |
| :--- | :--- | :--- |
| **B1** | Every result carries a required `resultType` (`"complete"` / `"input_required"`). Multi Round-Trip Requests (`InputRequiredResult`, `inputRequests`/`inputResponses`, `requestState`) replace server-initiated requests. | Absent. **Design option:** implement approval gates as `input_required` with a signed `requestState` instead of custom `-32031` + `/api/codemode/resume`. |
| **B2** | `ttlMs` + `cacheScope` required on `tools/list`, `prompts/list`, `resources/list`, `resources/read`, `resources/templates/list`; deterministic tool ordering recommended. | Absent. Tools are held in `BTreeMap`s, so ordering is probably already deterministic (confirm with a test). |
| **B3** | `Mcp-Method` and `Mcp-Name` headers required on Streamable HTTP POST; `x-mcp-header` support; `HeaderMismatchError`. | Absent. |
| **B4** | `_meta` keys: `serverInfo` in results, `clientInfo`, per-request `logLevel`; remove `ping`, `logging/setLevel`, `roots/list_changed`; no `Mcp-Session-Id`, no `Last-Event-ID` resumability. | Partial. `_meta` `protocolVersion` is checked, but only that an unknown revision is rejected (`omni_mcp.rs:309-326`). `clientCapabilities` is never read (v4.7 release notes say it is validated), a stateless request with no `_meta.protocolVersion` is accepted, and `serverInfo`/`clientInfo`/`logLevel` are absent. |
| **B5** | Error-code policy: `-32000..-32019` implementation-defined; **`-32020..-32099` reserved for the spec**. Assigned: `-32020` HeaderMismatch, `-32021` MissingRequiredClientCapability, `-32022` UnsupportedProtocolVersion. | **Conflict.** All gateway codes `-32029..-32038` sit in the reserved range, and an unsupported `_meta` protocol version is returned as `-32602` (`INVALID_PARAMS`, `omni_mcp.rs:318-325`), not `-32022`. See breaking change #1. |
| **B6** | Authorization for HTTP transports: RFC 9728 protected-resource metadata, `401` with `WWW-Authenticate: Bearer resource_metadata=…`, `403 insufficient_scope`, token audience validation (RFC 8707), **no token passthrough**, Client ID Metadata Documents preferred (DCR deprecated). | Absent (see A1). |
| **B7** | Tasks moved to extension `io.modelcontextprotocol/tasks` (`tasks/get`, `tasks/update`). | Absent. **Design option:** expose long-running/suspended Code Mode executions as tasks. |
| **B8** | Deprecated: Roots, Sampling, Logging; HTTP+SSE transport. | `/sse` still served with `Sunset: 2026-12-31` (two months out). Remove it in 5.0 (or behind an off-by-default `legacy-sse` feature) or extend the date. |

Also:
- **B9.** `LATEST_MCP_PROTOCOL_VERSION` is still `"2025-03-26"` and is the negotiation fallback (`rpc.rs:34-35, 118-130`). The v4.7 roadmap itself says that is no longer a safe default. Fall back to the newest session-based revision (`2025-11-25`).
- **B10.** CI: validate gateway responses against the published `schema.json` for each supported revision.

---

## 5. Part C — Honesty and product-surface fixes

- **C1. Documented CLI commands don't run.** `main.rs` only dispatches the CLI when the first argument is `mcp`. `npx @rangow2260/omni-mcp aifx mcp types|resume|simulate …` (three README examples) silently starts the HTTP server instead; `--version` does too **[EXEC]**. `release.yml` builds only `omni-gateway`, never `omni` or `aifx`. Fix: real subcommand parsing (`--version`, `--help`, `aifx …`), ship all three binaries, add wrapper `bin` entries.
- **C2. Performance claims lack evidence.** README claims ~100 µs execution and <50 µs boot; the sandbox timing test asserts only `boot + eval < 100_000 µs`, i.e. 100 ms (`integration_tests.rs:1204`; the latest commit relaxed these thresholds for CI jitter), and the built-in `mcp bench` measures only data-plane routing (16 µs avg) and never the sandbox **[CODE/EXEC]**. Publish a benchmark harness, methodology, hardware, and p50/p95/p99; state claims accordingly.
- **C3. "Rust/Rhai scripts."** The interpreter is a custom JS-like parser that accepts `|x| expr` closures and `service::tool()` calls. It is not Rhai, and the README should not imply Rhai compatibility **[CODE]**.
- **C4. Rewrite the "REA Claim Audit" table.** It labels as "Verified" several capabilities this audit found not to hold (authenticated approvals, OS CSPRNG, crash-safe ledger, slowloris/smuggling defense, 2026-07-28 parity). Add an *Evidence* column linking each row to a test; drop rows without one.
- **C5. Simulated wire.** gRPC, TChannel, and Muttley are in-memory simulators. Either implement them (gRPC needs HTTP/2) or label them experimental/mock in the README, `description` metadata, and Cargo/npm keywords.
- **C6. Branding and seed-data hygiene.** "Rust 2.0 Engine" appears 4× (startup banner, Dockerfile, `.env.example`, docs); Uber-specific names (Uber Rides Service, "Routed over HTTP via Uber MCP Gateway", `uber_token_`, Muttley/TChannel/MCPFx) appear in 9 files / 28 hits. Replace with a neutral, opt-in demo dataset, and check third-party-name usage.
- **C7. `CHANGELOG.md`** still reads "[Unreleased] — v4.7.0 work in progress" although 4.7.0 shipped.

---

## 6. Part D — Engineering, CI, supply chain, architecture

- **D1. Decide the dependency stance** (see §7). This single decision drives A6, A8, A9, B6 and the server rewrite.
- **D2. Break up the monoliths.** `codemode.rs` is 4,613 lines, `gateway.rs` 1,683, `omni_mcp.rs` 1,571, `server.rs` 1,463. Suggested split: `omni-codemode` (lexer/parser/AST/VM/columnar/saga/continuations), `omni-core` (models, registry, storage), `omni-gateway` (HTTP + MCP surfaces), `omni-cli`.
- **D3. Test depth.** 57 tests for ~17.4k lines. Add: auth matrix (A2); raw-socket HTTP smuggling/pipelining/slowloris suite (A8/A9); concurrent double-resume (A5); kill -9 WAL recovery (A13); fuzz targets for JSON parser, Code Mode parser, HTTP parser, WAL replay (dev-only; no `fuzz/` dir exists today); differential test of columnar `PhysicalExpr` vs row evaluation (v4.7 item H2, not done); coverage gate.
- **D4. CI and toolchain.**
  - Clippy currently fails on Rust 1.97: `#[allow(clippy::chunks_exact_to_as_chunks)]` at `audit_log.rs:11` is an unknown lint **[EXEC]**. CI uses floating `stable` with `-D warnings`, so this will keep breaking. Pin via `rust-toolchain.toml` and run a separate "latest stable" non-blocking job.
  - `rust-version = "1.75.0"` is never tested and the Dockerfile uses `rust:1.85`. Add an MSRV job.
  - Add `cargo audit`/`cargo deny`, Dependabot, CodeQL for the JS wrapper, and a container scan.
- **D5. Release and supply chain** (v4.7 item H3 was not delivered).
  - `bin/omni-mcp.js` downloads an executable over HTTPS with **no checksum or signature**, and writes straight to the final cache path, so an interrupted download leaves a truncated file that the next run executes **[CODE]**. Download to a temp file, verify SHA-256, then rename.
  - Publish `SHA256SUMS`, sigstore/GitHub artifact attestations, and npm `--provenance`.
  - `release.yml` hard-codes `body_path: docs/RELEASE_v4.7.0.md`, so a `v5.*` tag would publish v4.7.0's notes.
  - The npm step is gated by `if: env.NPM_TOKEN != ''` where `NPM_TOKEN` is only defined in that step's own `env`; step-level `env` is generally not visible to that step's `if`, so this may never run. **Unverified. Check the Actions logs.**
  - `publish-github-packages.yml` triggers on `release: published`, but releases created with `GITHUB_TOKEN` don't trigger other workflows, and `release.yml` publishes to GitHub Packages itself, so there are two overlapping paths. Consolidate.
  - Linux builds are glibc-only; add `musl` static builds.
- **D6. Docker.** Runs as root, no `HEALTHCHECK`, no `--locked`, base images not pinned by digest, and `Cargo.lock` is git-ignored although this is a binary crate (the drift guard even references it). Commit `Cargo.lock`; add non-root `USER`; use a minimal runtime image.
- **D7. Observability.** The trace buffer is capped at 5,000 but evicts with `Vec::remove(0)` under a write lock for every recorded span (O(n), `gateway.rs:617-621`); the audit `Vec` grows without rotation. Use a ring buffer, an OTLP exporter, and rotating/exportable audit segments.
- **D8. Config and API stability.** Validate all `OMNI_*` settings at startup (fail fast); version REST routes under `/api/v1`; publish an OpenAPI document; add a v4→v5 state migration tool and WAL format versioning.

---

## 7. Decisions needed from you

1. **Dependency stance (D1).** Keep "zero runtime dependencies", or move to a *small audited set*? Keeping zero-dep means hand-maintaining crypto/RNG/HTTP/JSON and writing the fuzzers yourself, and the OS CSPRNG (A6) and TLS remain unsolved under `forbid(unsafe_code)`. A minimal set would be roughly `getrandom`, `subtle`, `sha2`+`hmac`, `serde_json` (built-in recursion limit), and `httparse` or `hyper`. A larger step would build the protocol layer on the official Rust SDK `rmcp` (v3.8.0, released 2026-10-26, targets spec `2026-07-28`), at the cost of a tokio-based dependency tree. **Recommendation:** adopt the minimal set now; evaluate `rmcp` for the protocol layer during Phase 3.
2. **Deployment target.** Local single-user tool (stdio/loopback) vs. a multi-user network gateway. This sets how much of A1/A8/A12 is mandatory and whether TLS/OAuth ship in 5.0.
3. **Breaking error codes (B5).** OK to renumber `-32029..-32038` into `-32000..-32019` in 5.0, with a one-release compatibility map?
4. **Keep or cut** gRPC/TChannel/Muttley (C5) and `/sse` (B8).

---

## 8. Phased plan and exit criteria

| Phase | Scope | Release | Exit criteria |
| :--- | :--- | :--- | :--- |
| **0** | §2 hotfix | `4.7.1` | A1/A2/A4/A7/A10/A11 patched; advisory published; 4.7.0 deprecated on npm. |
| **1. Trust boundary** | A1-A6, A11, A12, C6 (seed data) | `5.0.0-alpha.1` | Auth-matrix test green for every route; resume cannot be self-approved; CSPRNG in use; no default secret. |
| **2. Front door + durability** | A7-A10, A13, A14, D7 | `alpha.2` | Parser/HTTP fuzzers run ≥1 h clean; smuggling/slowloris/pipelining suite green; kill -9 recovery test green. |
| **3. Protocol conformance** | B1-B10 | `beta.1` | Responses validate against `schema.json` for all supported revisions; error codes renumbered; `/sse` removed or flagged. |
| **4. CLI, honesty, supply chain** | C1-C5, C7, D4-D6 | `beta.2` | README claims each linked to a test/benchmark; `aifx` works via npx; checksums + attestations on release; clippy/MSRV CI green. |
| **5. Structure and evidence** | D2, D3, C2 | `rc.1` | Crates split; benchmark methodology published; coverage gate met. |
| **6. Migration and release** | D8, migration guide | `5.0.0` | Upgrade guide + `omni migrate`; `CHANGELOG`, `RELEASE_v5.0.0.md`, `release.yml` body updated. |

---

## 9. What is already solid (keep)

- `#![forbid(unsafe_code)]` on the library and every binary.
- Declarative `PROTOCOL_CAPABILITY_TABLE` and the `FeatureStatus`/error-contract design.
- In-tree SHA-256/HMAC (HMAC test-vector coverage is thin: one RFC 4231 case, so add the rest).
- Merkle audit chain design, WAL + snapshot structure, version drift guard, 3-OS CI matrix, `SECURITY.md` scope notes (no in-process TLS; sandbox is not a hardened jail).
- Sequential single-use replay rejection works (`-32038` on second resume) **[EXEC]**.

---

## 10. Breaking changes to document in the v5.0 upgrade guide

1. Gateway error codes move out of the spec-reserved `-32020..-32099` range; UnsupportedProtocolVersion becomes `-32022`.
2. All `/api/*` routes require authentication; `X-Caller-Actor` no longer selects the actor class.
3. `OMNI_AUTH_SECRET` is required in network mode; no default; existing continuation frames signed with the old key are rejected.
4. Default bind is `127.0.0.1`; demo data requires `--demo`.
5. `/sse` removed (or behind `legacy-sse`); `Mcp-Session-Id`/`Last-Event-ID` unsupported for `2026-07-28`.
6. Approvals move to an authenticated approver (and MRTR if B1 is adopted); the free-text `approver` field is removed.

---

## 11. Sources

- MCP `2026-07-28` changelog: https://modelcontextprotocol.io/specification/2026-07-28/changelog
- MCP 2026-07-28 authorization requirements (summary): https://ssojet.com/blog/mcp-authentication-oauth-tokens-security-ai-connections.md
- Official Rust SDK `rmcp`: https://docs.rs/crate/rmcp
