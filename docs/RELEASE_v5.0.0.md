# Omni MCP v5.0.0 Release Notes — "Trust Boundary Reset & MCP 2026-07-28 Conformance"

**Release Date:** November 2026  
**License:** Apache-2.0  
**Runtime:** Pure Rust 2021 Standard Library (`std`-only, zero runtime dependencies, `#![forbid(unsafe_code)]`)

---

## Overview

Omni MCP v5.0.0 is a major milestone release delivering the **Trust Boundary Reset** and **Full MCP `2026-07-28` Specification Conformance**. Every trust claim has been hardened with verified cryptographic and architectural guarantees, while maintaining our zero-dependency pure standard library footprint.

---

## Key Highlights & Capabilities

### 1. 🤝 Complete MCP `2026-07-28` Protocol Conformance (B1–B10)

- **Standardized Result Types (`resultType`) (B1)**:
  - All JSON-RPC responses under `2026-07-28` emit the mandatory `"resultType": "complete"` envelope.
  - Multi Round-Trip Requests (MRTR): Human-in-the-loop approval requests emit `"resultType": "input_required"` with signed, tamper-evident `requestState` handles.
- **Deterministic Ordering & Caching Directives (B2)**:
  - Tool and resource lists (`tools/list`, `resources/list`, `prompts/list`, `resources/templates/list`) enforce deterministic lexicographical sorting.
  - Response envelopes carry authoritative caching metadata: `ttlMs` (300,000 ms for catalogs, 60,000 ms for resource reads) and `cacheScope: "workspace"`.
- **Streamable HTTP Framing & Header Mismatch Contract (B3)**:
  - Streamable HTTP POST endpoints (`/omni/mcp` and `/<service>/mcp`) require and validate incoming `Mcp-Method` and `Mcp-Name` headers.
  - Header mismatches are rejected immediately with HTTP 400 Bad Request and spec-reserved error code `-32020 HEADER_MISMATCH`. Valid headers are reflected on response envelopes.
- **Specification Hygiene & Deprecated Feature Pruning (B4)**:
  - Under `2026-07-28`, legacy endpoints `ping`, `logging/setLevel`, and `roots/list_changed` are cleanly removed.
  - Client capabilities are strictly validated on `initialize`, rejecting unsupported capabilities with spec-reserved `-32021 MISSING_REQUIRED_CLIENT_CAPABILITY`.
- **Spec-Reserved Error-Code Allocation (B5)**:
  - Aligned strictly with the MCP error-code specification: range `-32020..-32099` is strictly reserved for specification definitions:
    - `-32020`: `HEADER_MISMATCH`
    - `-32021`: `MISSING_REQUIRED_CLIENT_CAPABILITY`
    - `-32022`: `UNSUPPORTED_PROTOCOL_VERSION`
  - Gateway implementation-defined error codes renumbered into `-32000..-32019`:
    - `-32004`: `RATE_LIMITED`
    - `-32005`: `CIRCUIT_OPEN`
    - `-32006`: `APPROVAL_REQUIRED`
    - `-32007`: `FUEL_EXHAUSTED`
    - `-32008`: `OUTBOUND_BLOCKED`
    - `-32009`: `SAGA_COMPENSATED`
    - `-32010`: `SCHEMA_PREFLIGHT_FAILED`
    - `-32011`: `MEMORY_EXHAUSTED`
    - `-32012`: `TIMEOUT`
    - `-32013`: `APPROVAL_INVALID`
  - Backward compatibility: Gateway errors automatically include `"legacy_code"` in response `data` mapping to legacy `-32029..-32038` codes.
- **RFC 9728 Protected-Resource Metadata & HTTP Authorization (B6)**:
  - Exposes RFC 9728 resource metadata at `GET /.well-known/oauth-protected-resource`.
  - HTTP 401 Unauthorized responses supply `WWW-Authenticate: Bearer resource_metadata="/.well-known/oauth-protected-resource"`.
  - HTTP 403 Forbidden responses supply `WWW-Authenticate: Bearer error="insufficient_scope"`.
- **Tasks Extension (`io.modelcontextprotocol/tasks`) (B7)**:
  - Advertised during `initialize` under capabilities extensions.
  - Implements `tasks/get` and `tasks/update` endpoints, exposing long-running and suspended execution frames as resumable and cancellable tasks.
- **SSE Deprecation & Feature-Gated Legacy SSE (B8)**:
  - `/sse` returns `410 Gone` by default. Legacy SSE transport is gated behind the off-by-default `legacy-sse` Cargo feature.
- **Protocol Version Negotiation & Latest Revision (B9)**:
  - `LATEST_MCP_PROTOCOL_VERSION` set to `"2026-07-28"`, with negotiation fallback defaulting to the latest session-based revision `"2025-11-25"`.
- **Structural Response Validation (B10)**:
  - Built-in `validate_mcp_response` schema validation verifying structure and mandatory `resultType` presence across protocol revisions.

---

### 2. 🛡️ Trust Boundary Reset & Security Hardening (A1–A14)

- **Universal Route Authorization & RBAC**: Deny-by-default middleware gating all `/api/*` and `/api/v1/*` routes with role-based access control (`Agent`, `Operator`, `Approver`, `Admin`).
- **Atomic Compare-and-Swap Frame Resumption**: `consume_frame` resolves race conditions under write lock and persists to WAL prior to mutation dispatch, preventing concurrent double-resumes.
- **Cryptographic Separation of Duties**: Mutation requesters cannot self-approve; approver identity and expected SHA-256 argument digests are verified.
- **Dynamic Length-Prefixed HMAC-SHA256**: Continuation frames are sealed with length-prefixed canonical strings over all 12 frame fields using secrets derived from `OMNI_AUTH_SECRET`.
- **Strict HTTP Framing & Smuggling Defense**: Persistent single `BufReader` across keep-alive loops, strict digit-only `Content-Length`, immediate termination on body limit overflow, and 8,192-byte header line caps.
- **DoS & Worker Pool Resilience**: JSON recursion depth cap (`MAX_JSON_DEPTH = 64`), bounded worker queue with `503 Service Unavailable` load shedding, and connection-level `catch_unwind`.
- **Durability & Fsync Guarantees**: `sync_all()` on WAL mutations, snapshot atomic renames, and Merkle audit log appends.

---

### 3. ⚡ Architecture, Clean-up & CLI Subcommands (C1–C7, D1–D8)

- **Direct Subcommand Execution (C1)**: Unified binary entrypoint dispatches commands with or without prefixes (`omni-gateway list`, `omni search`, `aifx bench`, `omni-gateway migrate`).
- **O(1) Trace Eviction (D7)**: Trace span buffer converted to `VecDeque<TraceSpan>` ring buffer, eliminating $O(n)$ lock contention.
- **Route Versioning & OpenAPI 3.1 (D8)**: All REST endpoints aliased under `/api/v1/*`, alongside interactive OpenAPI 3.1 documentation at `GET /openapi.json` and `GET /api/v1/openapi.json`.
- **State Migration Utility (D8)**: `omni migrate` CLI migrates existing WAL and state stores between minor revisions.
- **Verified NPM Supply Chain (D5)**: `bin/omni-mcp.js` downloads precompiled binaries to temporary paths, verifies SHA-256 against `SHA256SUMS`, and performs atomic renames.

---

## Breaking Changes in v5.0.0

1. **Error Codes**: Gateway implementation-defined error codes moved from `-32029..-32038` to `-32004..-32013`. Clients should inspect `error.code` or fallback to `error.data.legacy_code`. Spec-reserved error `-32022` replaces `-32602` for unsupported protocol versions.
2. **Universal Authentication**: All `/api/*` and `/api/v1/*` routes now require Bearer authentication. Unauthenticated requests are rejected with 401/403.
3. **Loopback Default**: `OMNI_HOST` defaults to `127.0.0.1`. Binding to non-loopback interfaces requires `OMNI_AUTH_SECRET` to be explicitly configured.
4. **SSE Deprecation**: `/sse` returns HTTP 410 Gone unless compiled with `--features legacy-sse`. Use Streamable HTTP (`/omni/mcp`) instead.
5. **Human Approvals**: Free-text `approver` string field is replaced with authenticated approver principal validation. Self-approval is rejected.

---

## Upgrade Guide

### Upgrading from v4.x to v5.0.0

1. **Update Dependencies / NPX**:
   ```bash
   npx -y @rangow2260/omni-mcp@latest --version
   ```
2. **Run State Migration**:
   ```bash
   omni-gateway migrate --data-dir ./data
   ```
3. **Configure Authentication**:
   Ensure `OMNI_AUTH_SECRET` is defined in production environments before exposing the gateway over non-loopback network interfaces.
