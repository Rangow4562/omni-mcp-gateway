# Omni MCP v4.7.1 Release Notes — "Trust Boundary Reset & Security Hardening"

**Release Date:** October 2026  
**License:** Apache-2.0  
**Runtime:** Pure Rust 2021 Standard Library (`std`-only, zero runtime dependencies, `#![forbid(unsafe_code)]`)

---

## Overview

Omni MCP v4.7.1 is a critical security hardening and trust-boundary reset release addressing findings identified in the v5.0 roadmap security audit. It enforces universal route-level authorization, closes race conditions in approval continuations, hardens HTTP socket framing and worker-pool resilience, eliminates stack-overflow DoS vectors, and adds fsync durability guarantees.

---

## Key Security Fixes & Improvements

### 1. 🛡️ Universal Route Authorization & RBAC
- **Deny-by-Default Middleware**: All 20 `/api/*` endpoints now enforce Bearer token authentication and role-based access control (RBAC):
  - **`Agent` role**: `/api/execute`, `/api/stats`, `/api/servers`, `/api/codemode/eval`, `/api/codemode/types`, `/api/codemode/speculative_types`, `/api/codemode/simulate`
  - **`Operator` role**: `/api/traces`, `/api/audit`, `/api/audit/verify`, `/api/diffs`, `/api/tools/propose_update`
  - **`Approver` role**: `/api/codemode/suspended`, `/api/codemode/resume`, `/api/diffs/approve`
  - **`Admin` role**: `/api/tools/toggle_status`, `/api/servers/toggle_status`, `/api/tools/rollback`, `/api/crawler/trigger`
- **Constant-Time Verification**: API keys are validated using constant-time byte comparisons (`constant_time_eq`) against `OMNI_AUTH_SECRET`.
- **Fail-Fast Bind Validation**: The gateway defaults to `127.0.0.1` and strictly refuses to start on non-loopback network addresses if `OMNI_AUTH_SECRET` is unset.
- **Sensitive Field Redaction**: `GET /api/codemode/suspended` strictly redacts `script_source`, `serialized_scope`, `pending_arguments`, and `frame_hmac`.

### 2. 🔐 Atomic CAS Resumption & Continuation Hardening
- **Atomic Compare-and-Swap (`consume_frame`)**: Resolves race conditions in human-in-the-loop approvals by checking `!frame.is_consumed`, marking it consumed, appending to WAL, and calling `sync_all()` under a write lock *prior* to executing the downstream mutation. Racing concurrent resumes are rejected with `-32038 APPROVAL_INVALID`.
- **Separation of Duties**: Requesters are cryptographically prevented from approving their own mutations (`approver == caller_actor` is rejected).
- **Approver Argument Digest Verification**: Approvers can provide an expected SHA-256 argument digest upon resumption, verified case-insensitively before execution.
- **Dynamic Continuation HMAC**: Continuation frames are canonically signed with length-prefixed encoding across all 12 frame fields using keys derived from `OMNI_AUTH_SECRET` or per-process random keys (removing static public keys).

### 3. 🌐 Strict HTTP Request Framing & DoS Containment
- **Persistent Connection Framing**: Preserves a single `BufReader` across HTTP/1.1 keep-alive loops, preventing dropped pipelined requests.
- **Strict Protocol Validation**:
  - Rejects non-digit `Content-Length` headers with `400 Bad Request`.
  - Rejects `Transfer-Encoding: chunked` without `Content-Length` with `501 Not Implemented`.
  - Rejects conflicting framing headers with `400 Bad Request`.
  - Immediately terminates connections when `Content-Length` exceeds the configured payload limit (`413 Payload Too Large`).
  - Caps request lines and individual header lines at 8,192 bytes.
- **JSON Recursion Depth Limit**: `MAX_JSON_DEPTH = 64` enforced in the recursive-descent JSON parser, preventing worker thread stack overflow crashes.
- **Worker-Pool Resilience**: Bounded worker channel shedding saturated requests with `503 Service Unavailable`, and connection-level `std::panic::catch_unwind` preventing thread death.

### 4. 💾 Storage Durability & Fsync Guarantees
- **WAL & Snapshot Fsync**: Added `f.sync_all()` after every WAL append, temporary snapshot commit before atomic rename, snapshot truncation, and Merkle audit log append.

### 5. 🤝 Protocol Negotiation Honesty
- **Capability Matrix Honesty**: Flagged `2026-07-28` as `implemented: false` in `PROTOCOL_CAPABILITY_TABLE` and updated negotiation fallback to `2025-11-25`, ensuring the gateway only advertises fully implemented protocol revisions.

### 6. 📦 Supply Chain & Tooling
- **Verified Package Downloads**: Updated `bin/omni-mcp.js` to download precompiled binaries to a temporary file, compute SHA-256, and verify against `SHA256SUMS` before caching.
- **CLI Subcommand Support**: Added native `--version`, `-V`, `version`, `aifx ...`, and `mcp ...` subcommand execution to the binary entrypoints.
- **Container Hardening**: Dockerfile updated with non-root user (`USER omni:omni`) and a `/healthz` container `HEALTHCHECK`.

