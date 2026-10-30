# Contributing

1. Keep the runtime **std-only**: no new runtime dependencies. Dev-only tooling must never be linked into release binaries.
2. Before opening a PR run:
   ```
   cargo fmt --all -- --check
   cargo clippy --all-targets -- -D warnings
   cargo test --locked
   ```
3. Do not weaken or delete tests to make a change pass.
4. Only claim a feature works if a test proves it; label simulated behaviour as simulated.
5. Versions are single-sourced: bump `Cargo.toml`, `Cargo.lock`, `package.json`, `ui/package.json`, the `Dockerfile` label, README download links and add `docs/RELEASE_vX.Y.Z.md` together (a drift-guard test enforces this).
