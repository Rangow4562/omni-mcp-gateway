# Changelog

All notable changes to Omni MCP. Per-release narrative notes live in [`docs/`](docs/).

## [5.0.0-alpha.2] - 2026-10-31

### Added
- Phase 1 & 2 Trust Boundary Reset:
  - Atomic compare-and-swap (CAS) frame consumption under write lock to eliminate concurrent double-resume vulnerability (A5).
  - Approver argument digest verification preventing argument manipulation between suspension and resumption (A5).
  - Strict HTTP request framing: persistent `BufReader` across keep-alive loops, digit-only Content-Length validation, rejection of chunked transfer-encoding without content-length with 501, and connection termination on body size limit violations (A9).
  - Worker pool resilience: `std::panic::catch_unwind` boundary preventing worker thread death on panic, and bounded queue with 503 Service Unavailable when saturated (A8, A14).
  - Hardened authentication principal extraction: rate-limiting keys and actor identities derived strictly from authenticated claims, discarding client-supplied `X-Caller-Actor` (A12).
  - Bounded rate limiter state with TTL-based and LRU eviction limiting memory consumption (A12).
  - Fsync durability: added `flush()` and `sync_all()` on WAL mutations, snapshot renames, and audit trail appends (A13).
  - Protocol negotiation honesty: `2026-07-28` set to `implemented: false` until Part B protocol changes are fully implemented, falling back cleanly to `2025-11-25`.

### Changed
- CLI enhancement: Subcommand dispatch for `--version`, `-V`, `version`, and `aifx ...` commands (C1).
- Scrubbed inaccurate Rhai compatibility claims; documented as JS/TS syntax with closure support (C3).
- Neutral branding & demo catalog: Scrubbed Uber-specific naming references and "Rust 2.0" branding across codebase, adopting neutral fleet/rides service naming with backward-compatible aliases (C6).

## [4.7.1] - 2026-10-31

### Fixed
- Hotfix for Step 0 & Phase 1 trust boundary items (A1, A2, A3, A4, A7, A10, A11 default bind & secret validation).

## [4.7.0] - 2026-10-30

### Added
- Per-revision MCP protocol capability table and `negotiate_protocol_version()` (only revisions flagged `implemented` are offered).
- Error codes `-32036` MEMORY_EXHAUSTED, `-32037` TIMEOUT, `-32038` APPROVAL_INVALID (reserved/defined).
- Cargo package metadata (license, repository, homepage, keywords, categories).
- Version drift guard v2 (README links, Dockerfile label, npm wrapper, Cargo.lock, `docs/` release notes).

### Changed
- Release/roadmap documents moved to `docs/`.
- README: wire translation and Muttley delegation relabelled **Simulated** (in-memory); `/sse` marked deprecated.

## [4.6.0]
See [docs/RELEASE_v4.6.0.md](docs/RELEASE_v4.6.0.md).

## [4.5.0]
See [docs/RELEASE_v4.5.0.md](docs/RELEASE_v4.5.0.md).
