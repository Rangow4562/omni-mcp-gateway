# Omni MCP v4.5.0 — Autonomous Runtime & Distributed Mesh Gateway

<p align="center">
  <img src="https://raw.githubusercontent.com/Rangow4562/omni-mcp-gateway/main/assets/omni-mcp-eink-banner.png" alt="Omni MCP Banner" width="100%" />
</p>

Omni MCP v4.5.0 introduces the **Autonomous Runtime & Distributed Mesh Gateway**, expanding the zero-dependency pure Rust in-process Code Mode engine into a fully resilient, production-grade distributed execution runtime.

---

### 🌟 Key Highlights in v4.5.0

#### 1. 🔄 Resumable AST Continuations for Approval Gates (Stateless Suspend/Resume)
- When scripts hit human-in-the-loop approval gates (`is_mutation: true` requiring approval), execution no longer halts and discards state.
- Serializes instruction pointer, call stack, scope heap, and Merkle audit hashes into a durable Crockford Base32 ULID snapshot (`SuspendedExecutionFrame`) stored in the FNV-1a checksummed WAL.
- Once approved via the REST API or CLI (`aifx mcp resume <execution_id>`), any node in the cluster verifies the cryptographic state and seamlessly resumes execution at `instruction_ptr + 1`.

#### 2. ⚡ Zero-Copy Columnar Predicate Pushdown (`ColumnarRecordBatch` & `PhysicalExpr`)
- Compiles `.filter()` and `.map()` AST closures directly into vectorized `PhysicalExpr` pipelines over typed contiguous columnar batches (`ColumnarRecordBatch`).
- Evaluates predicates (`>=`, `<=`, `==`, `!=`, `<`, `>`) with zero per-row object allocations, processing 50,000+ records in microseconds.

#### 3. 🔀 Static AST SSA Auto-Parallelization (Compiler-Driven Parallel Waves)
- Analyzes variable read/write sets across AST statements via a Static Single Assignment (SSA) pass before script interpretation.
- Automatically lifts consecutive independent `codemode.*` invocations into concurrent `Stmt::ParallelWave` batches evaluated across threads via `std::thread::scope`, correcting suboptimal sequential LLM code without prompting.

#### 4. ↩️ Declarative Saga Compensation for Multi-Mutation Rollbacks
- Mutating tools declare compensating rollback tools via `MutationCompensationSpec`.
- Tracks executed state mutations in an ephemeral transaction log during script execution.
- If a subsequent step fails or trips a circuit breaker (`-32030 CIRCUIT_OPEN`), the gateway automatically unwinds previous mutations in reverse (`LIFO`) order and records cryptographic rollback receipts in the Merkle audit log.

#### 5. 🎯 Speculative 0-Turn Schema Pre-Injection
- Sub-millisecond BM25 + stem/tag intent routing pre-populates the prompt template `{{types}}` with the top-$K$ highest-confidence TypeScript declarations on Turn 1 based on natural-language user intent.
- Eliminates the discovery round-trip, collapsing a 2-turn discovery pattern into a single inference call.

#### 6. 🎲 Deterministic Simulation Testing (`dst`) Engine
- Pure Rust 64-bit `Xoshiro256StarStar` seeded PRNG chaos harness.
- Validates agent scripts against network drops, latency spikes, malformed JSON payloads, and tripped circuit breakers across $N$ deterministic simulation runs (`aifx mcp simulate`).

---

### 📦 Installation & Verification

#### Run instantly via npx:
```bash
npx -y @rangow2260/omni-mcp aifx mcp --help
```

#### CLI Operations:
```bash
# Resume suspended execution frame after approval
aifx mcp resume 01JC1234567890ABCDEFGHJKMN --approver "secops-lead"

# Run deterministic simulation testing with fault injection
aifx mcp simulate --code "const res = await codemode.orders.charge({ id: 'ord_1' });" --seed 42 --runs 10

# Speculative 0-turn type resolution
aifx mcp types --intent "Process customer refund and verify payment ledger"
```

#### Pre-built Platform Binaries:
- macOS Apple Silicon (`omni-gateway-darwin-arm64`)
- macOS Intel x64 (`omni-gateway-darwin-x64`)
- Linux x64 (`omni-gateway-linux-x64`)
- Linux ARM64 (`omni-gateway-linux-arm64`)
- Windows x64 (`omni-gateway-win32-x64.exe`)
