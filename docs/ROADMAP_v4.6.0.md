# Omni MCP v4.6.0 Feature Roadmap — "Honesty & Hardening"

<p align="center">
  <img src="https://raw.githubusercontent.com/Rangow4562/omni-mcp-gateway/main/assets/omni-mcp-eink-banner.png" alt="Omni MCP Banner" width="100%" />
</p>

## 1. Theme & Context

Omni MCP sits at **v4.5.0** after delivering resumable AST continuations, columnar predicate pushdown, Static Single Assignment (SSA) parallel waves, declarative Saga rollbacks, and Deterministic Simulation Testing (`dst`). A comprehensive gap analysis of the ~14.7k-line zero-dependency Rust codebase confirms that while the execution engine is architecturally sound, v4.6 must address three structural gaps:

1. **Honesty Debt**: Documentation and comments in several modules outrun the in-memory/simulated reality or label additive substring matching as BM25.
2. **Interpreter Unit Test Floor**: The 3,892-line Code Mode interpreter (`src/data_plane/codemode.rs`) and core subsystems currently rely exclusively on `tests/integration_tests.rs` (32 end-to-end tests) with zero in-module unit tests (`#[cfg(test)]`).
3. **Machine-Readable Error Contract**: Runtime feature states (`"APPROVAL_REQUIRED"`, `"FuelExhausted"`, `"GlobalOutboundBlocked"`, `"SAGA_ROLLED_BACK"`) ride as untyped strings rather than authoritative JSON-RPC `-320xx` error codes.

**v4.6 Theme**: *Every published claim becomes true, the interpreter gets a dedicated unit test floor, and the error surface becomes a compiler-checked, machine-readable contract — all while strictly preserving the zero-third-party-dependency (`std`-only) guarantee.*

---

## 2. Marquee Features ("What's New in v4.6")

### M1. 🛡️ Code Mode Test Fortress

- **What**: Dedicated `#[cfg(test)] mod tests` unit test suites inside the core engine modules covering parser error diagnostics, AST evaluator semantics, fuel exhaustion limits, `globalOutbound: null` refusal, vectorized `PhysicalExpr` evaluation over `ColumnarRecordBatch`, `SuspendedExecutionFrame` serialize-to-resume identity, WAL FNV-1a checksum round-trips, and declarative Saga LIFO rollback ordering.
- **Why**: Gives the 3,892-line polyglot AST interpreter and durable storage layer fast, isolated, deterministic unit test coverage using plain `std` `#[test]` before refactoring ranking and error contracts.
- **Files Touched**:
  - `src/data_plane/codemode.rs` (parser, evaluator, fuel, outbound guard, `PhysicalExpr`, `SuspendedExecutionFrame`)
  - `src/json.rs` (RFC 8259 parser/serializer edge cases, escapes, trailing-byte rejection)
  - `src/storage/wal_store.rs` (WAL append, snapshot compaction, FNV-1a corruption detection, continuation frame persistence)
  - `src/data_plane/gateway.rs` (Saga LIFO unwinding order and version single-sourcing drift guard)
- **Effort**: **M–L**

### M2. 🎯 Real Okapi BM25 Ranking

- **What**: Replace the additive substring and token overlap scorer in `src/data_plane/codemode.rs` (`431–448`) and `src/extension/omni_mcp.rs` with true **Okapi BM25** ranking:
  $$\text{BM25}(D, Q) = \sum_{q \in Q} \text{IDF}(q) \cdot \frac{f(q, D) \cdot (k_1 + 1)}{f(q, D) + k_1 \cdot \left(1 - b + b \cdot \frac{|D|}{\text{avgdl}}\right)}$$
  using inverse document frequency ($\text{IDF}$) across the registered tool corpus, term frequency saturation ($k_1 = 1.2$), document length normalization ($b = 0.75$), camelCase/snake_case identifier splitting, suffix stemming, and field boosts across `name`, `description`, `tags`, and `schema` parameter keys.
- **Why**: Makes every published "BM25 ranking" claim across the documentation and architecture diagrams genuinely true while significantly improving 0-turn `{{types}}` pre-injection precision on multi-term natural-language prompts.
- **Files Touched**:
  - `src/data_plane/codemode.rs` (`generate_speculative_types` and corpus BM25 scorer)
  - `src/extension/omni_mcp.rs` (`call_discover_server` and `call_recommend_tools`)
  - `tests/integration_tests.rs` (BM25 IDF, length normalization, and threshold tests)
- **Effort**: **S**

### M3. 📐 Authoritative Error-Code Contract (`-320xx` Table)

- **What**: Expand `src/models/rpc.rs` (`8–19`) with a complete, compiler-checked `-320xx` JSON-RPC 2.0 server error table and a strongly-typed `FeatureStatus` enum:
  - `-32030 CIRCUIT_OPEN` — Downstream microservice circuit breaker is open
  - `-32031 APPROVAL_REQUIRED` — Mutating tool invocation paused at human-in-the-loop gate
  - `-32032 FUEL_EXHAUSTED` — Code Mode script exceeded deterministic instruction fuel budget
  - `-32033 OUTBOUND_BLOCKED` — Unauthorized network/filesystem call blocked by `globalOutbound: null`
  - `-32034 SAGA_COMPENSATED` — Multi-step script failed and prior mutations were unwound in LIFO order
  - `-32035 SCHEMA_PREFLIGHT_FAILED` — Reserved for pre-execution schema validation failures
- **Why**: Replaces ad-hoc string matching (`"APPROVAL_REQUIRED"`, `"FuelExhausted"`, `"GlobalOutboundBlocked"`) with a wire-compatible `FeatureStatus` enum (`as_str()` + `error_code()`) and structured `RPCError { code, message, data: Some({ status, detail }) }` payloads.
- **Files Touched**:
  - `src/models/rpc.rs` & `src/models/mod.rs`
  - `src/data_plane/codemode.rs`
  - `src/data_plane/gateway.rs`
  - `src/data_plane/dst.rs`
  - `src/extension/omni_mcp.rs`
  - `README.md`
- **Effort**: **M**

### M4. 🤝 MCP Protocol Version Negotiation

- **What**: Replace hardcoded `"protocolVersion": "2024-11-05"` responses in `src/extension/omni_mcp.rs:292` and `src/data_plane/gateway.rs:191` with spec-compliant lifecycle version negotiation:
  - If the client sends a `protocolVersion` in the supported set `{"2024-11-05", "2025-03-26"}`, echo that exact version back.
  - Otherwise (missing or unknown future version), respond with the gateway's latest supported version `"2025-03-26"`.
  - Include additive `2025-03-26` tool capability annotations (`annotations: { readOnlyHint, destructiveHint, idempotentHint }`) derived from `ToolRecord` metadata.
- **Why**: Ensures seamless interoperability with both legacy `2024-11-05` MCP clients and modern `2025-03-26` agent harnesses without breaking backwards compatibility.
- **Files Touched**:
  - `src/models/rpc.rs`
  - `src/models/entities.rs`
  - `src/extension/omni_mcp.rs`
  - `src/data_plane/gateway.rs`
  - `tests/integration_tests.rs`
- **Effort**: **S**

---

## 3. Hygiene & Structural Hardening Table

| # | Item | Target Files | Action |
| :--- | :--- | :--- | :--- |
| **H1** | **Kill Misnomer Alias** | `src/storage/wal_store.rs:12`, `src/storage/mod.rs:5` | Delete `pub type SQLiteRegistryStore = WalRegistryStore;` and its module re-export so the WAL engine is never misidentified as SQLite. |
| **H2** | **Doc Honesty Sweep** | `README.md`, `Cargo.toml:6`, `src/lib.rs`, `src/data_plane/muttley.rs:23`, `src/data_plane/translators.rs:101,115`, `src/control_plane/crawler.rs:1` | Accurately describe in-process / simulated wire envelopes and mock mesh adapters; drop stale `🚧` icon on shipped Human-in-the-Loop approval in `README.md:47`. |
| **H3** | **Version Single-Sourcing** | `Cargo.toml:3`, `package.json:3-4`, `ui/package.json:4`, `src/extension/omni_mcp.rs:301`, `src/storage/wal_store.rs:185`, `src/extension/aifx.rs:664`, `src/data_plane/codemode.rs:72`, `README.md` | Replace hardcoded Rust version literals with `env!("CARGO_PKG_VERSION")`, align `ui/package.json` (`1.0.0` -> `4.6.0`), and add a unit test guarding against version drift across manifests and `README.md`. |
| **H4** | **Artifact Cleanup** | `tests/__pycache__/`, `.gitignore` | Delete stale Python bytecode cache directory `tests/__pycache__/` and verify `__pycache__/` is ignored in `.gitignore`. |
| **H5** | **Residual Audit Status Constants** | `src/data_plane/gateway.rs:829,850,910` | Define explicit audit-log-only constants (`AUDIT_STATUS_SAGA_COMPENSATED`, `AUDIT_STATUS_SAGA_UNCOMPENSATED`, `AUDIT_STATUS_CONTINUATION_RESUMED`) with doc comments distinguishing Merkle audit status tags from RPC error codes. |
| **H6** | **Release Notes (`RELEASE_v4.6.0.md`)** | `RELEASE_v4.6.0.md` | Author full v4.6.0 release documentation in the project's headline style, including a forward-looking **"What's Next (v4.7)"** roadmap section. |

---

## 4. Out of Scope for v4.6

- **Real gRPC/HTTP-2 or Thrift Binary Network Sockets**: Implementing a hand-rolled HTTP/2 HPACK parser in zero-dependency Rust is an unnecessary security surface; v4.7 will introduce an optional, feature-gated `transport-http` live HTTP/1.1 JSON downstream client first while keeping gRPC/TChannel envelope framing clearly labeled as in-memory wire translators.
- **External SQLite C-Binding Dependency**: `WalRegistryStore` remains the sole zero-dependency durable storage engine.
- **Distributed Multi-Node Consensus / SSE Tool Streaming / Full MCP `2025-06-18` Surface**: Deferred to maintain minimal surface complexity (YAGNI).
- **Frontend UI Test Harness or Further Columnar/SSA Micro-Optimizations**: Current sub-millisecond performance already exceeds targets.

---

## 5. Execution Sequencing Phases

1. **Phase 1 — Contract & Truth**:
   - Implement M3 (`-320xx` error code constants, `FeatureStatus` enum, and `RPCError` structured data helpers).
   - Single-source version strings to `env!("CARGO_PKG_VERSION")` and bump manifests to `4.6.0`.
   - Delete `SQLiteRegistryStore` alias, remove `tests/__pycache__/`, and execute the non-BM25 documentation honesty sweep.
2. **Phase 2 — Test Fortress**:
   - Land M1 unit test modules across `src/data_plane/codemode.rs`, `src/json.rs`, `src/storage/wal_store.rs`, and `src/data_plane/gateway.rs`.
   - Lock baseline ranking and continuation invariants before swapping the scoring engine.
3. **Phase 3 — Behavior Upgrades**:
   - Land M2 (true Okapi BM25 ranking in `codemode.rs` and `omni_mcp.rs`) + BM25 unit tests (IDF weighting, document length normalization, top-$K$ confidence threshold).
   - Land M4 (MCP protocol version negotiation for `2024-11-05` and `2025-03-26` + tool annotations) and handshake tests.
   - Update `README.md` BM25 and `-320xx` contract documentation.
4. **Phase 4 — Release Readiness**:
   - Bump `ui/package.json` to `4.6.0`, write `RELEASE_v4.6.0.md`, and pass `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test` with zero warnings.

---

## 6. Verification Checklist

- [x] Every file path and line reference in `ROADMAP_v4.6.0.md` verified against the repository tree.
- [x] Clean Markdown formatting (proper blank lines around headings, lists, and code blocks).
- [x] **M1 Gate**: Unit test suites in `src/data_plane/codemode.rs`, `src/json.rs`, `src/storage/wal_store.rs`, and `src/data_plane/gateway.rs` pass under `cargo test --lib`.
- [x] **M2 Gate**: Okapi BM25 unit tests verify rare-term IDF preference, document length normalization (`b = 0.75`), and speculative 0-turn confidence scoring.
- [x] **M3 Gate**: All `-32030`..`-32035` error codes and `FeatureStatus` variants verified across unit and integration tests and documented in `README.md`.
- [x] **M4 Gate**: `initialize` handshake tests verify `"2024-11-05"` echo, `"2025-03-26"` negotiation, and fallback to `"2025-03-26"` on unknown client versions.
- [x] **Release Gate**: Drift-guard test confirms version `4.6.0` matches across `Cargo.toml`, `package.json`, `ui/package.json`, and `README.md`; `cargo fmt --check && cargo clippy -- -D warnings && cargo test` exits cleanly.
