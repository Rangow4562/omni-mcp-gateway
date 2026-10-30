# Omni MCP v4.7.0 Release Notes — "Protocol Parity & Real Wire"

**Release Date:** October 2026  
**License:** Apache-2.0  
**Runtime:** Pure Rust 2021 Standard Library (`std`-only, zero runtime dependencies)

---

## Overview

Omni MCP v4.7.0 delivers **Protocol Parity & Real Wire**, extending our enterprise Model Context Protocol gateway to negotiate all five historical and current MCP revisions (`2024-11-05`, `2025-03-26`, `2025-06-18`, `2025-11-25`, `2026-07-28`), introducing our first feature-gated live downstream transport (`transport-http`), hardening human-in-the-loop continuations with HMAC-SHA256 signatures, persisting in-flight Saga compensations across crashes, and enforcing sandbox memory, wall-clock timeout, and DNS rebinding boundaries.

---

## Key Features & Improvements

### 1. 🤝 Full MCP Protocol Parity (`2024-11-05` through `2026-07-28`)
- **First-Class Capabilities Matrix**: Protocol behavior driven by a declarative capability table across all 5 official revisions.
- **MCP `2026-07-28` Stateless Support**:
  - Implements the required `server/discover` RPC surfacing supported versions, tools, resources, prompts, and extensions.
  - Implements `subscriptions/listen` replacing session notification streams.
  - Validates `io.modelcontextprotocol/protocolVersion` and `io.modelcontextprotocol/clientCapabilities` in request `_meta`, returning `-32602` (`UnsupportedProtocolVersionError`) on mismatch.
  - Gates JSON-RPC array batching and `batch_invoke_tools` to legacy revisions only; rejects with `-32600` on `2026-07-28`.
- **Streamable HTTP Transport**:
  - Primary single-endpoint HTTP POST/GET support on `/omni/mcp` and `/<service-name>/mcp`.
  - `/sse` marked as explicitly deprecated with `Deprecation` and `Sunset` response headers.
- **Trace Context Bridging**: W3C `traceparent` extraction and propagation in request/response `_meta`.

### 2. 🌐 Live HTTP/1.1 Downstream Transport (`transport-http`)
- **Pure `std` Downstream Client**:
  - Thread-safe connection pooling with HTTP/1.1 keep-alive.
  - Bounded connect, read, and write timeouts with jittered exponential retries.
  - Chunked and Content-Length transfer decoding.
  - Feeds per-service circuit breaker (`-32030`) and Saga unwinder (`-32034`).

### 3. 📋 Schema Preflight Validator (`-32035 SCHEMA_PREFLIGHT_FAILED`)
- **Single-Pass AST Schema Validation**:
  - Validates tool invocation arguments against registered `ToolRecord.input_schema` prior to execution or approval.
  - Gathers all schema violations (missing required fields, type mismatches) into `RPCError.data.violations` for single-turn LLM recovery.
  - Integrated into `explain_error` for automated diagnostic guidance.

### 4. 🔐 Approval & Continuation Hardening + Persistent Saga Ledger
- **HMAC-SHA256 Continuation Signatures**:
  - Suspended continuation frames signed via HMAC-SHA256 over execution ID, script hash, instruction pointer, pending tool, and argument digest.
  - Approvals bind to single-use semantics, non-empty approver identity, argument integrity, and expiration timestamps (`-32038 APPROVAL_INVALID`).
- **Crash-Safe Saga Ledger**:
  - WAL and snapshot persistence for in-flight `SagaCompensationRecord` entries with idempotency keys.
  - Recovery across node restarts guaranteeing eventual compensation or operator alerting (`AUDIT_STATUS_SAGA_UNCOMPENSATED`).
- **OS CSPRNG Entropy**:
  - Cryptographic ULID generation mixing system time nanos, thread/process IDs, ASLR stack address, and atomic counters through SHA-256.

### 5. 🧱 Sandbox Resource Limits
- **Memory Allocation Caps**: Limits string, array, and object allocation growth (`-32036 MEMORY_EXHAUSTED`).
- **Wall-Clock Execution Timeouts**: Bounds script runtime (`-32037 TIMEOUT`).
- **Panic Containment**: Isolates internal script panics via `catch_unwind` without aborting the gateway process.

### 6. 🛡️ HTTP Surface Hardening
- **DNS Rebinding Protection**: Validates HTTP `Origin` header on requests targeted at localhost hosts.
- **Enforced Authentication**: Requires Bearer authorization tokens on `/api/execute` and `/api/audit*`.
- **Request Smuggling & Slowloris Defense**: Header line count (100) and byte size (64KB) caps, read timeouts, and conflicting header rejection.

---

## Authoritative Error Codes (`-320xx`)

| Code | Constant | Description |
| :--- | :--- | :--- |
| `-32030` | `CIRCUIT_OPEN` | Downstream service circuit breaker is open. |
| `-32031` | `APPROVAL_REQUIRED` | Script paused at human approval gate; resume via `execution_id`. |
| `-32032` | `FUEL_EXHAUSTED` | Code Mode script exceeded instruction fuel budget. |
| `-32033` | `OUTBOUND_BLOCKED` | Raw network/filesystem access denied by `globalOutbound: null`. |
| `-32034` | `SAGA_COMPENSATED` | Multi-step mutations rolled back in reverse LIFO order. |
| `-32035` | `SCHEMA_PREFLIGHT_FAILED` | Tool arguments failed schema preflight validation. |
| `-32036` | `MEMORY_EXHAUSTED` | Code Mode script exceeded memory allocation budget. |
| `-32037` | `TIMEOUT` | Code Mode script exceeded wall-clock execution time limit. |
| `-32038` | `APPROVAL_INVALID` | Continuation token verification failed, expired, replayed, or altered. |
