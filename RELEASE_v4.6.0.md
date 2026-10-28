# Omni MCP v4.6.0 — "Honesty & Hardening"

<p align="center">
  <img src="https://raw.githubusercontent.com/Rangow4562/omni-mcp-gateway/main/assets/omni-mcp-eink-banner.png" alt="Omni MCP Banner" width="100%" />
</p>

Omni MCP v4.6.0 (**"Honesty & Hardening"**) hardens the zero-dependency Rust autonomous runtime with dedicated in-module unit test fortresses, true Okapi BM25 corpus ranking, an authoritative `-320xx` JSON-RPC 2.0 error-code contract, spec-compliant MCP `2024-11-05` / `2025-03-26` protocol version negotiation, and native pre-built binaries across **all major operating systems and CPU architectures**.

---

### 🌟 Key Highlights in v4.6.0

#### 1. 🛡️ Code Mode Unit Test Fortress (M1)
- Adds dedicated `#[cfg(test)] mod tests` unit suites directly inside `src/data_plane/codemode.rs`, `src/json.rs`, `src/storage/wal_store.rs`, and `src/data_plane/gateway.rs` using pure `std` `#[test]`.
- Verifies parser syntax diagnostics, AST expression nesting caps, closure evaluation semantics, instruction fuel exhaustion (`-32032`), `globalOutbound: null` isolation (`-32033`), vectorized `PhysicalExpr` evaluation over `ColumnarRecordBatch`, `SuspendedExecutionFrame` serialize-to-resume identity, FNV-1a WAL corruption rejection, and declarative Saga LIFO rollback ordering.

#### 2. 🎯 True Okapi BM25 Ranking (M2)
- Replaces the legacy additive substring scorer with full **Okapi BM25** ($k_1 = 1.2$, $b = 0.75$) over stemmed multi-field documents (`tool.name`, `server.name`, `tags`, `description`, and `input_schema` parameter keys).
- Computes corpus-wide inverse document frequency ($\text{IDF}$), term-frequency saturation, document length normalization, and camelCase/snake_case identifier splitting across `speculative_pre_inject`, `discover_server`, and `recommend_tools`.

#### 3. 📐 Authoritative `-320xx` Error-Code Contract (M3)
- Introduces a compiler-checked `FeatureStatus` enum and expands `src/models/rpc.rs` with a complete `-320xx` error table:
  - `-32030 CIRCUIT_OPEN`
  - `-32031 APPROVAL_REQUIRED`
  - `-32032 FUEL_EXHAUSTED`
  - `-32033 OUTBOUND_BLOCKED`
  - `-32034 SAGA_COMPENSATED`
  - `-32035 SCHEMA_PREFLIGHT_FAILED` (reserved)
- Surfaces `feature_status` and `error_code` directly in `CodeOutput`, `RPCError::from_status`, and `explain_error` diagnostics.

#### 4. 🤝 MCP Protocol Version Negotiation & `2025-03-26` Annotations (M4)
- Replaces hardcoded `"2024-11-05"` handshakes in `OmniMCPServer` and `MCPProxyGateway` with dynamic lifecycle negotiation supporting `{"2024-11-05", "2025-03-26"}` (echoing supported client versions and defaulting to `"2025-03-26"`).
- Enriches `ToolRecord::to_mcp_spec` with additive MCP `2025-03-26` tool capability hints (`annotations: { readOnlyHint, destructiveHint, idempotentHint, openWorldHint }`).

#### 5. 🧹 Single-Sourced Versioning & Documentation Alignment
- Single-sources all Rust runtime version strings from `env!("CARGO_PKG_VERSION")` with an automated drift-guard test verifying alignment across `Cargo.toml`, `package.json`, `ui/package.json`, and `README.md`.
- Removes the legacy `SQLiteRegistryStore` type alias in favor of `WalRegistryStore` and labels all in-memory wire framing and Merkle audit status constants explicitly.

---

### 📦 Installation Across All Operating Systems

#### 1. Universal Zero-Install via NPX (Windows, macOS, Linux)
```bash
npx -y @rangow2260/omni-mcp --stdio
npx -y @rangow2260/omni-mcp aifx mcp --help
```

#### 2. Pre-Built Native Binaries Matrix

| Operating System | Architecture | Target Triple | Asset Name |
| :--- | :--- | :--- | :--- |
| **macOS** | Apple Silicon (M1/M2/M3/M4) | `aarch64-apple-darwin` | `omni-gateway-darwin-arm64` |
| **macOS** | Intel (x86_64) | `x86_64-apple-darwin` | `omni-gateway-darwin-x64` |
| **Linux** | x86_64 (glibc) | `x86_64-unknown-linux-gnu` | `omni-gateway-linux-x64` |
| **Linux** | ARM64 / AArch64 | `aarch64-unknown-linux-gnu` | `omni-gateway-linux-arm64` |
| **Windows** | x64 (Intel / AMD) | `x86_64-pc-windows-msvc` | `omni-gateway-win32-x64.exe` |
| **Windows** | ARM64 (Snapdragon / Surface) | `aarch64-pc-windows-msvc` | `omni-gateway-win32-arm64.exe` |

#### 🍎 macOS (Apple Silicon & Intel)
```bash
curl -fL https://github.com/Rangow4562/omni-mcp-gateway/releases/download/v4.6.0/omni-gateway-darwin-arm64 -o omni-gateway
chmod +x omni-gateway && ./omni-gateway --stdio
```

#### 🐧 Linux (x64 & ARM64)
```bash
curl -fL https://github.com/Rangow4562/omni-mcp-gateway/releases/download/v4.6.0/omni-gateway-linux-x64 -o omni-gateway
chmod +x omni-gateway && ./omni-gateway --stdio
```

#### 🪟 Windows (PowerShell — x64 & ARM64)
```powershell
Invoke-WebRequest -Uri "https://github.com/Rangow4562/omni-mcp-gateway/releases/download/v4.6.0/omni-gateway-win32-x64.exe" -OutFile "omni-gateway.exe"
.\omni-gateway.exe --stdio
```

#### 3. Verify Test Suites From Source
```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
```

---

### 🔭 What's Next (v4.7 Roadmap Preview)

- **Feature-Gated Live HTTP/1.1 Downstream Transport (`transport-http`)**: Optional zero-dependency TCP/HTTP/1.1 downstream client alongside the in-memory `MuttleyMeshClient` simulator.
- **Pre-Execution Schema Preflight Validator (`-32035 SCHEMA_PREFLIGHT_FAILED`)**: Static AST argument type-checking against registered `ToolRecord.input_schema` prior to script execution.
- **Persistent Saga Compensation Ledger**: Durable cross-restart recovery of in-flight Saga compensation chains in `WalRegistryStore`.
