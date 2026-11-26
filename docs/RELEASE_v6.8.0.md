# OmniMCP `v6.8.0` Release Notes

**Release Date:** 2026-11-26  
**Theme:** *Skills over MCP, Postgres HA Tasks, Governance-Driven Elicitation, Streaming File Uploads & Wasm Guest ABI v2*

## Highlights

### 1. Unified `2026-07-28` Capability Negotiation, `server/discover` Parity & `PgWirePool`
- Unified `build_server_capabilities()` and `server/discover` across `/omni/mcp`, `/groups/<slug>/mcp`, and `/<service>/mcp`.
- `ClientCapabilitiesMeta::from_params()` enforces backward-compatible `elicitation: {}` (`form`-only) vs explicit `elicitation: { url: {} }`.
- Strict `group_slug` / `server_scope` authorization enforced on `tasks/get`, `tasks/update`, `tasks/cancel`, `skills/list`, `skills/get`, `resources/read`, and `resources/directory/read`.
- Bounded `PgWirePool` (`OMNI_PG_POOL_SIZE=8`) eliminates single-connection starvation, and channel-dispatched `LISTEN/NOTIFY` (`omni_registry_changed` vs `omni_task_changed`) prevents task status updates from triggering server catalog reload storms.

### 2. Skills over MCP (`io.modelcontextprotocol/skills`, SEP-2640)
- Enabled by default (`OMNI_ENABLE_SKILLS_EXTENSION=true`).
- Strict `SkillRecord::validate()` enforcing 512 files / 16 MiB total, root `SKILL.md` YAML frontmatter (`name` + `description`), POSIX path-traversal rejection, and deterministic `manifest_sha256` digests.
- Fleet-aware `SkillEngine` (`src/control_plane/skills.rs`) merging curated persisted skills with deterministically auto-synthesized Tool Group and Virtual Service Code Mode playbooks (`SKILL.md`, `references/types.d.ts`, `references/examples.ts`).
- Full MCP (`skills/list`, `skills/get`, `resources/read` `skill://`, `resources/directory/read`), REST (`/api/v1/skills*`), and CLI (`omni skills list|publish|verify`) support.

### 3. Postgres HA-Backed MCP Tasks (`io.modelcontextprotocol/tasks`, GA)
- Enabled by default (`OMNI_ENABLE_TASKS_EXTENSION=true`).
- Durable `omni_mcp_tasks` persistence across Postgres, SQLite, and WAL with atomic CAS transitions and 60-second TTL reaper.
- `TaskManager` (`src/data_plane/task_runner.rs`) with bounded concurrency (`OMNI_MAX_CONCURRENT_TASKS=64`) and `OMNI_TASK_SYNC_GRACE_MS=250` grace-window promotion returning `resultType: "task"`.
- Cooperative cancellation (`cancel_flag` in AST VM `HostCallbacks` and Wasm `WasmHostState`) raising `CodeModeError::Cancelled` and unwinding completed mutations via LIFO Saga compensation.
- GA `tasks/get`, `tasks/update` (`inputResponses` -> `{}` ack, preserving legacy `action: "resume"|"cancel"` fallback), `tasks/cancel` (`{}` ack), and Prometheus metrics (`omni_mcp_tasks_active`, `omni_mcp_tasks_total`).

### 4. Governance-Driven Elicitation (`form` & `url`) + Multi-Replica HA OAuth
- `ElicitationPolicyRule` on `CharterPolicy` with a strict Secret-Safety & Primitive-Schema validator rejecting secret-like field names (`KEY`, `TOKEN`, `SECRET`, `PASSWORD`, `PASS`, `AUTH`, `CREDENTIAL`) and non-primitive JSON schema types.
- `form`-mode MRTR `resultType: "input_required"` with `inputRequests: [elicitation/create]`, Merkle audit logging on `accept`/`decline`, and LIFO Saga rollback on `decline`.
- Multi-replica HA `omni_oauth_flows`, `omni_oauth_tokens` (`ChaCha20-Poly1305` AEAD), and `omni_upstream_callbacks` across Postgres/SQLite/WAL using `OMNI_PUBLIC_BASE_URL`.

### 5. Feature-Gated File Uploads (`io.modelcontextprotocol/files`, SEP-2356), Wasm Guest ABI v2 & Studio UI v4
- Opt-in (`OMNI_ENABLE_FILE_UPLOADS_EXTENSION=false` by default) and isolated in `src/extension/file_uploads.rs`.
- Dual async/sync streaming upload bypass (`POST`/`PUT /api/v1/files/upload/:upload_id`, up to `OMNI_FILE_UPLOAD_MAX_BYTES=64 MiB`, `Transfer-Encoding: chunked`, `PUT` CORS) while keeping JSON-RPC strictly capped at 10 MiB.
- Universal multi-cloud `FileBlobBackend` (`src/storage/file_store.rs`) in pure `#![forbid(unsafe_code)]` Rust with zero vendor SDK crates: `local`, `postgres`, `s3` (AWS SigV4), `gcs`, `azure` (SharedKey HMAC-SHA256 / SAS / Bearer), and `http`/WebDAV.
- Wasm Guest ABI v2 host imports: `omni::file_stat` and `omni::file_read_chunk` (max 256 KiB/call, fuel-metered per KiB, bounds-checked, zero ambient FS access).
- `HttpDownstreamClient::post_stream_file()` (64 KiB chunked streaming) and `GrpcDownstreamClient::invoke_grpc_with_file_metadata()`.
- Governance Studio UI v4: **Skills Studio**, **Tasks Monitor**, and unified **Approval & Elicitation Inbox**.
- State migration utility: `omni migrate --to-v6.8`.
