# Omni MCP v5.5.1 Release Notes — "Unified External MCP Fleet Security & Routing Hardening"

**Release Date:** November 2026  
**License:** Apache-2.0  
**Runtime:** Pure Rust 2021 Standard Library (`std`-only, zero runtime dependencies, `#![forbid(unsafe_code)]`)

---

## Overview

Omni MCP v5.5.1 is a critical security and stability patch release for the **Unified External MCP Fleet** engine. It resolves a route authorization privilege escalation on external server deletion, introduces pure standard library URL percent-decoding across all server endpoints, aligns the React UI registration modal schema contract, adds compiler lint hygiene, and expands regression test coverage.

---

## Key Fixes & Security Hardening

### 1. 🛡️ RBAC Authorization & Privilege Escalation Hardening
- **Scoped Route Protection**: Fixed an RBAC route ordering loophole in `src/server.rs` where `DELETE /api/servers/external/:name` (without `/deregister`) fell through to `AuthRole::Agent`.
- **Strict Method Exemption**: The `AuthRole::Agent` role exemption on `/inspect` is now strictly restricted to HTTP `GET`. Any non-GET methods or mutations across `/api/servers/external/*` and `/api/v1/servers/external/*` strictly require `AuthRole::Admin`.
- **Sub-Route Deletion Guard**: Path validation in `handle_delete` explicitly rejects non-delete endpoints (`/inspect`, `/rediscover`, `/register`), preventing accidental or unauthorized manipulation.

### 2. 🌐 Zero-Dependency URL Percent-Decoding
- **Pure `std` Percent Decoder**: Implemented zero-dependency `percent_decode` handling ASCII hex escapes, multi-byte UTF-8 sequences, and malformed sequence fallback.
- **Routing & Registry Lookups**: Applied percent-decoding to server identifiers across:
  - `GET /api/servers/:name` (with fallback to `external-mcp:<name>`)
  - `GET /api/v1/servers/external/:name/inspect`
  - `POST /api/v1/servers/external/:name/rediscover`
  - `POST /api/v1/servers/external/:name/deregister`
  - `POST /api/servers/toggle_status` & `POST /api/tools/toggle_status`
  - `POST /:service/mcp` & `GET /:service/mcp`
- Completely resolves 404 / `-32602` errors for UI operations on encoded names (`external-mcp%3A<slug>`).

### 3. 📋 React Dashboard Schema Alignment & Robust Deserialization
- **Frontend Registration Modal (`RegisterExternalServerModal.tsx`)**: Aligned submission payload to emit canonical `name`, `display_name`, `owner_team`, and structured `transport` objects.
- **Defensive Backend Deserialization**: `POST /api/v1/servers/external/register` supports canonical fields (`name`, `display_name`, `owner_team`), camelCase aliases (`displayName`, `owningTeam`), legacy nested wrappers (`{ transport: "stdio", stdio: { ... } }`), and flat transport schemas seamlessly.

### 4. 🧹 Compiler Lint & UI Asset Synchronization
- **Clippy Attribute**: Added `#[allow(unknown_lints)]` in `src/storage/audit_log.rs` to ensure warning-free compilation on all toolchains.
- **Fresh UI Bundle**: Rebuilt production assets via Vite v5.4.21 into `ui/dist/` (`dist/assets/index-BcKxxuKA.js`).

### 5. 🧪 Regression Test Suite
- Added comprehensive integration tests covering:
  - Privilege escalation verification (asserting 403 Forbidden for `Agent` tokens on `DELETE /api/servers/external/*`).
  - URL percent-decoding and fallback resolution.
  - Unit tests for percent-decoding edge cases (multi-byte UTF-8, malformed sequences, percent-encoded slashes).
