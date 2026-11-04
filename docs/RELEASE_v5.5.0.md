# Omni MCP v5.5.0 Release Notes — "Unified External MCP Fleet"

**Release Date:** November 2026  
**License:** Apache-2.0  
**Runtime:** Pure Rust 2021 Standard Library (`std`-only, zero runtime dependencies, `#![forbid(unsafe_code)]`)

---

## Overview

Omni MCP v5.5.0 introduces the **Unified External MCP Fleet** engine, enabling enterprise gateway deployments to register, supervise, reconcile, and execute arbitrary external Model Context Protocol servers across both local subprocesses (`stdio`) and remote endpoints (`Streamable HTTP`).

All external servers automatically inherit Omni MCP's first-class governance envelope — including Okapi BM25 tool discovery, Code Mode TypeScript `.d.ts` declaration generation, declarative Saga compensation, circuit breakers, semantic caching, and Merkle-tree chained audit logging — while adhering to our zero-dependency, `#![forbid(unsafe_code)]` pure Rust standard library architecture.

---

## Key Highlights & Capabilities

### 1. 🚀 Stdio Subprocess Supervision & Windows/POSIX Isolation
- **Allowlist Canonicalization**: Candidate binaries are verified against `OMNI_MCP_STDIO_ALLOWLIST`. On Windows, executable paths without extensions probe `.exe` and `.cmd`, while `cmd.exe /C` wrapper injection is strictly forbidden.
- **Detached Non-Blocking `stderr` Drain**: Child process `stderr` is drained into an in-memory 256-line ring buffer (capped at 4 KB per line), eliminating pipe deadlock risks even during massive log floods (>128 KB).
- **Graceful Lifecycle Management**: Lazy re-spawning of exited subprocesses and clean termination (`kill()` + `wait()`) on gateway shutdown ensure zero orphan processes.

### 2. 🌐 Streamable HTTP Transport & Zero-Dependency SSRF Guard
- **Raw Socket HTTP/1.1 Engine**: Pure `std::net::TcpStream` implementation supporting stateful session tracking via `Mcp-Session-Id` and automatic 404 session-expiry recovery.
- **SSRF & DNS Pinning Guard**: Rejects cloud metadata IPs (`169.254.169.254`), IPv4/IPv6 link-local addresses, and loopback connections (configurable via `OMNI_MCP_ALLOW_LOOPBACK_UPSTREAM`). DNS resolution is pinned to prevent DNS rebinding attacks.
- **TLS Egress Guidance**: Explicitly reports `-32014 UPSTREAM_TLS_UNSUPPORTED` on `https://` endpoints, guiding users to local stdio bridges (`npx -y supergateway`) or local TLS proxies.

### 3. 🔐 Refs-Only Secret Resolver & Zero-Persistence Guarantee
- **Strict Symbolic References**: Secrets must follow `{env:NAME}` or `{file:PATH}` syntax; raw credentials are completely rejected at registration.
- **Path Traversal Protection**: File-based secrets are canonicalized and strictly verified to reside within `OMNI_MCP_SECRETS_DIR`.
- **Zero Raw Secrets on Disk**: Secrets are resolved only at subprocess spawn or HTTP connect time in memory, ensuring raw credentials never touch `registry.db`, WAL logs, or audit trails.

### 4. ⚖️ Provenance-Aware External Sync & CAS Approval
- **Deterministic Reconciliation**: `POST /api/v1/servers/external/:name/rediscover` synchronizes upstream catalogs, calculating exact `{ added, updated, retired, unchanged, pending_human_conflicts }` metrics.
- **Human Governance Preservation**: Tools with `schema_source: Human` are never overwritten by upstream updates; instead, unapproved diffs are queued for operator review.
- **Compare-And-Swap (CAS) Protection**: `approve_diff` validates that tool versions match proposed diffs, returning `409 Conflict` on race conditions.

### 5. 🖥️ Modernized React Dashboard
- **Authenticated API Client (`ui/src/api.ts`)**: Built-in dev token switcher for testing `Admin`, `Approver`, `Operator`, and `Agent` RBAC roles.
- **External Server Registration Modal**: Intuitive modal for configuring both `stdio` and `streamable_http` transports with client-side secret ref validation.
- **External Server Inspector**: Comprehensive diagnostics panel featuring live connection health, tool metrics, live `stderr` tail inspection, and one-click rediscovery/deregistration.
