# Changelog

All notable changes to Omni MCP. Per-release narrative notes live in [`docs/`](docs/).

## [Unreleased] — v4.7.0 work in progress

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
