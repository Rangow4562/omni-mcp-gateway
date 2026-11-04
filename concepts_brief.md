# External MCP Server Registration for omni-mcp

* **Author:** Rangow4562 (EGM Goldfish review, 6 lenses: design, codebase-details, zoom-out, comprehension, implementation-readiness, structural)
* **Date:** 2026-11-01

## Overview

Port MCPJungle's register-and-manage-MCP-servers capability into omni-mcp's existing UI so external MCP servers (stdio and Streamable HTTP) can be added "in one place" — and their tools become first-class omni-mcp tools: findable by BM25 search, callable through the gateway and Code Mode, with generated type declarations, tool enablement, RBAC, and audit covering them the same way as built-in tools.

MCPJungle (`D:\Personal\Memory\MCPJungle`, Go) is a read-only reference for the registration surface. The implementation target is omni-mcp (`D:\Personal\Memory\omni-mcp`, Rust, pinned at `83b75ab7`) — a zero-dependency, `#![forbid(unsafe_code)]` gateway with a control plane (registry + WAL, ConfigDiff approval, AutoCrawler) and a data plane (gateway dispatch, translators, Code Mode, BM25, RBAC, Merkle audit).

## Analysis

### What survived stress-testing

These claims were checked against code and hold:

- **One registry.** External servers extend the existing `ServerRecord` with a transport config; no parallel registry. Because BM25, Code Mode type declarations, RBAC, and dispatch all derive from `ServerRecord`/`ToolRecord` at query time, external tools inherit every surface for free. The "no parallel bookkeeping" claim is verified true for placement — as long as cache refresh is not forgotten (see below).
- **New tools register DISABLED.** Matches the existing pattern (crawler and `native_monitor` both land discoveries Disabled; the gateway filters non-Enabled tools out of exposure).
- **Manual rediscover only, no background sync.** Matches how `POST /api/crawler/trigger` works today. But see the wording correction below.
- **Refs-only credentials** (`{env:NAME}` / `{file:PATH}`, never raw tokens, never persisted resolved). Correct, and stricter than the reference (MCPJungle accepts raw `bearer_token`, `headers`, `env` values). The divergence must be taught in the form UI.
- **Out-of-V1 list is right:** OAuth, background sync, prompts, resources, tool groups stay out.
- **Opt-in demo seed** (`OMNI_MCP_DEMO_SEED`) is a real fix — the current seed block re-fires every boot and toggles statuses.
- **Type injection is free.** `json_schema_to_ts_type` (`src/data_plane/codemode.rs:725`) already turns any tool's JSON Schema into a TypeScript declaration. Note it is shallow (flat primitives; `$ref`/`oneOf`/nested objects degrade to `any`), which is acceptable for V1.

### The three decisions that block code

Every lens that touched these three flagged them as blockers. They change V1's shape, so they need an explicit call before drafting ends.

**1. TLS does not exist in this codebase, and the HTTP fleet is HTTPS.**
The crate has zero dependencies and hand-rolls HTTP/1.1 over raw `TcpStream`; `parse_url` (`src/data_plane/downstream_http.rs:43-51`) rejects everything but `http://`. The four named HTTP upstreams are hosted SaaS — context7 alone is `https://mcp.context7.com/mcp` — so the flagship "Streamable HTTP" transport cannot reach the flagship fleet as designed. Streamable HTTP also implies SSE response framing and `Mcp-Session-Id` lifecycle, which the current client cannot parse. Options: (a) adopt TLS (rustls, or the `rmcp` SDK which supplies both transport clients and session handling) as a recorded dependency-policy change; (b) ship HTTP transport `http://`-only with hard errors naming the ceiling, and wrap the four remote servers in a local proxy (mcp-proxy/supergateway) so they register via stdio; (c) ship V1 stdio-only (13 of the 17 fleet servers) and put HTTP in V1.5. Hand-rolling TLS is not an option.

**2. Registration endpoints under `/api/servers/` inherit the Agent role, and stdio registration is arbitrary code execution.**
The role match at `src/server.rs:499` — `_ if path.starts_with("/api/servers/") => AuthRole::Agent` — sits before the specific Admin arm for `/api/servers/toggle_status` (line 510), so that Admin arm is unreachable today (a real bug, verified). New register/rediscover routes placed at the natural REST paths land under the same prefix: an Agent-token holder could register `npx anything` and make the gateway host run it. Fix: Admin (or a dedicated role) arms ordered before the prefix arm, a role-per-action matrix in the draft, and the stdio command allowlist owned by config file (e.g. `OMNI_MCP_STDIO_ALLOWLIST`), never API-editable, validated at registration and again at spawn.

**3. Auto schema sync corrupts the version machinery it promises to preserve.**
`register_server` (`src/control_plane/registry.rs:67-101`) is a merge: it overwrites `description`/`input_schema` (clobbering human-approved edits), never removes tools, and pushes `version_history` only on first insert. `version_history` is in-memory and rebuilds as one snapshot after restart, so "rollback versions preserved" needs durable before/after rows — which today only `ConfigDiff.before_state` provides. `approve_diff` has no check that the tool still sits at `diff.previous_version`, so a stale approval can clobber a fresh sync. The converged fix (both a design and a structural reviewer landed here independently): a dedicated `apply_external_sync()` registry method that writes **auto-approved `ConfigDiff` rows tagged `origin: external_sync`** (before/after populated) — reusing the persisted diff ledger, `rollback_tool`'s `before_state` fallback, and the diffs timeline for free — plus a version guard in `approve_diff` (mismatch → 409) and per-field provenance (`schema_source: Upstream | Human`) so rediscover auto-applies only fields a human has not overridden, surfacing divergence as a pending diff instead of silently winning.

### Windows supervision is the operational landmine

13 of the 17 fleet servers are stdio (node/npx). On this win32 machine:

- `Command::new("npx")` does not resolve `npx.cmd` (CreateProcess needs an explicit extension or a shell, and `cmd /C` brings quoting-injection problems). Resolve and allowlist canonicalized absolute paths at registration.
- `Child::kill()` is `TerminateProcess` and there is no kill-on-drop; without Job Objects (`windows-sys`, which collides with `forbid(unsafe_code)` and the zero-dependency identity) a gateway crash orphans node grandchildren holding ports. Either accept and document orphan-on-crash (kill the tree on graceful shutdown only), or make the dependency/`unsafe` call explicitly.
- A captured stderr pipe with no drain thread deadlocks the child at the ~64KB buffer mark. One drain thread per child into a ring buffer.
- Restart policy: none in V1. A crashed child is marked unreachable until the operator hits rediscover (V1 is manual anyway). Lazy restart on next call. This deletes the backoff/flapping problem entirely.
- Secrets must reach children via declared env refs only — children must not inherit the gateway environment (or `OMNI_AUTH_SECRET` leaks to third-party tools). Secrets never in argv (process listings).

### The UI is broken against its own backend today

`ui/src/App.tsx` hardcodes `http://localhost:8080/api/...` and sends no Authorization header on any of its 15+ fetch sites, while every `/api/*` route 401s without a Bearer — the dashboard shows nothing against the current gateway, in every mode. No token-provisioning mechanism exists (OAuth is out of V1). This is prerequisite zero: a centralized `api()` client (relative URLs work — the gateway serves the UI same-origin) plus a named dev-token story (paste/localStorage; in dev mode roles derive from the token, so the draft must say what the UI sends) come before any panel work. The UI has no test harness at all, so "works under enterprise RBAC" cannot be automated in V1 — restate it as manual verification or add a Playwright smoke later.

### Wording and semantics corrections

- **"Mirrored automatically" is wrong.** Sync is operator-triggered (rediscover button/endpoint). Within a triggered sync, changes apply without an approval step. Say exactly that; the word "automatically" invites a sync daemon that does not exist.
- **"Tools appear in BM25/Code Mode/type declarations" is false while tools are DISABLED.** Both search and typegen filter Enabled-only. Rewrite the criterion: "after an operator enables the server and its tools…". Decide the enablement choreography (enabling a server vs per-tool enablement — both toggles exist) and the landing state: `PendingReview` (surfaced in the existing diff queue) may be better UX than inert `Disabled`.
- **"Unavailable" is a state that does not exist.** `ToolStatus` is exactly {Disabled, PendingReview, Enabled}, and `register_server` cannot express removal at all. Retired tools need either a new `Unavailable`/`Retired` variant or `Disabled` + a `retired_at`/`last_seen_in_sync` field, plus a revival policy (what happens when a retired tool reappears).
- **`authenticate()` description corrected.** Verified against code: empty/anonymous tokens are rejected with 401 unconditionally (`server.rs:551`); there is no loopback passthrough. The real dev-mode hole: with no `auth_secret` set, any non-empty token is accepted and the role comes from a substring check (`token.contains("admin")` → Admin, line 600). Dev mode must switch to an explicitly provisioned dev token.
- **`security_mode` as one flag conflates two policies** — inbound auth and upstream network/SSRF policy — and "enterprise: localhost denied by default" collides with the live fleet (13 stdio are localhost by nature; the localhost e2e acceptance would be unsatisfiable unless loopback is allowlistable). Recommendation: drop the single flag; keep `auth_secret` semantics (enterprise startup already fails without it via `validate_bind`), add one SSRF guard at the new upstream-connect site (metadata-IP deny like 169.254.169.254, pin resolved IP pre-connect against DNS rebinding, refuse 3xx redirects — the raw client never follows them anyway), and write the 4-cell policy table (mode × inbound auth × outbound network class) in the draft. Egress checks apply to the HTTP transport; a stdio child makes its own connections, so the allowlist is the boundary for those.
- **"Single source of truth" means single storage, not single mutation path.** AutoCrawler already calls `register_server` diff-less today. What needs protecting is field ownership (see decision 3), not a nonexistent invariant.

### Remaining majors, carried as requirements into the draft

- **Transport config is a tagged enum**, not an HTTP-shaped tuple: `Http { url, secret refs, session_mode }` | `Stdio { command, args, env_refs }`. Adopt MCPJungle's `session_mode: stateless | stateful` vocabulary (the port dropped a dimension the reference explicitly has). Session lifecycle needs an object, not three flat methods: `Mcp-Session-Id` round-trip, re-initialize on session-expired 404, `notifications/initialized`, tools/list pagination cursors (chrome-devtools-class servers paginate), a declared rejection policy for upstream sampling/elicitation (else servers hang), and `notifications/cancelled` or abandon-id cancellation. V1 ceiling suggestion: POST-only JSON responses, no GET/SSE streams.
- **Dispatch routing contract.** `translators.rs` dispatches on `tool.protocol`, not the server's — so `ToolRecord` needs a stated convention for external tools (pseudo-endpoint like the existing `muttley://` style, or a new arm routing to the owning server's transport). The match at `translators.rs:42` is exhaustive over 5 variants; add the 6th arm explicitly, no wildcard.
- **Result mapping rule.** MCP `tools/call` returns content blocks + `structuredContent` + `isError`; the pipeline expects a flat `JsonValue` through projection, PII redaction, cache. Rule: `structuredContent` if present, else the single text block, else an error.
- **Call timeout and cancellation.** `HttpDownstreamClient` hardcodes 5s read / 3s connect — real calls (playwright, rea) run 30s+. One config key (`OMNI_MCP_UPSTREAM_CALL_TIMEOUT`, default ~120s), deadline fires → abandon the JSON-RPC id and audit it; never kill the child mid-flight.
- **Namespace collisions at three levels.** `sanitize_ident` collapses `pay.ments`/`pay-ments`/`pay_ments`, and the same collision exists for tool names within a server (`web-search` vs `web_search` both emit `function search_web`) and schema property names. Check sanitized-identifier uniqueness over servers AND tools (and top-level properties) at registration AND at sync time; 409 body names both raw inputs. Registry key `external-mcp:<slug>` needs `encodeURIComponent` in UI routes; collision scope must span crawler-seeded names too.
- **Concurrency.** Rediscover must fetch upstream outside the registry `RwLock`, validate-then-apply inside with the version CAS; single-flight per server (concurrent rediscover → 409 or coalesce). Every mutation handler must call `cache.refresh_now()` like existing handlers do, or search/typegen serve stale schemas.
- **`ProtocolType::from_str` catch-all `_ => Http`** (entities.rs:244) silently reclassifies an unknown protocol as Http — an old binary reading a new WAL would dispatch external tools to garbage endpoints. Add the `ExternalMcp` arm now, write the downgrade rule ("disable external servers before downgrading"), and round-trip test both directions.
- **Audit ledger choice.** The Merkle audit store is invocation-shaped and written from the data plane; control-plane mutations today leave `HistoryEntry` strings + `ConfigDiff` rows. Auto-approved ConfigDiff rows (decision 3) answer which ledger ExternalSync writes. Reconciliation counts should also leave a record.
- **Upgrade preservation.** `register_server`'s merge already preserves enablement (`registry.rs:71-74`) — extract that into one helper shared by registration and sync, or the "rediscover preserves enablement" criterion fails on the sync path.

## Goals vs. Non-Goals

* **In-Scope Goals:**
  1. Register/deregister external MCP servers (stdio + Streamable HTTP) from the omni-mcp UI, with a register form ported from MCPJungle's dashboard minus raw secrets.
  2. External tools become first-class `ToolRecord`s: BM25 discovery, Code Mode, generated type declarations, tool enablement, gateway execution, RBAC, audit — same paths as built-in tools.
  3. Operator-triggered rediscover with deterministic reconciliation (added/updated/retired/unchanged counts), auto-applied without approval inside the sync, tagged `origin: external_sync`, versioned with before/after rows.
  4. Refs-only credentials everywhere (transport auth, headers, env maps), resolved at connect time, never persisted, never shown in UI.
  5. SSRF guard at upstream connect; strict allowlist for stdio commands (config-owned); explicit dev-token provisioning for the UI.
  6. Panel verbs: register, deregister, rediscover, enable/disable (server + tool), inspect (health, last initialize, tool list), with 401/403/409 consistent with existing error helpers.
  7. Demo seed behind `OMNI_MCP_DEMO_SEED`; WAL backward-compat tests old-snapshot-load and new-snapshot round-trip.

* **Out-of-Scope Non-Goals (explicitly deferred, named upgrade paths):**
  - OAuth flows and any UI display of tokens/OAuth secrets/raw upstream config (MCPJungle's own dashboard rule).
  - Background/automatic sync; prompts and resources surfaces; tool groups.
  - TLS/HTTPS upstream support unless decision 1 chooses the dependency path; SSE server-push streams; legacy SSE transport.
  - Restart backoff/policy machinery (V1: mark unreachable, lazy restart on next call); process-tree containment beyond graceful-shutdown kill unless decision 2 chooses the dependency path.
  - Full `security_mode` policy engine; per-tool call-timeout overrides; batch WAL ops (accept per-tool entries until the log measurably bloats).

## Alternatives Weighed

- **Separate `ExternalMcpServer` entity + `McpUpstreamManager` registry (original proposal).** Rejected: parallel bookkeeping duplicates every consumer (BM25, RBAC, audit) and the headline benefit — inheriting existing surfaces — disappears.
- **Port MCPJungle's dashboard as-is onto omni-mcp.** Rejected: omni-mcp has no upstream MCP client at all; a ported panel would list servers without integrating their tools into search/Code Mode — the stated goal.
- **HTTP-only V1 (Streamable HTTP first).** Rejected after fleet evidence: 13 of 17 real servers are stdio; HTTP is the minority case AND the expensive one (TLS, SSE, sessions). A stdio-first client mirrors code that already exists (`run_stdio_server`, `rpc.rs` handshake semantics).
- **Full both-transport V1 as worded (TLS-capable HTTP + full supervision + security overhaul + versioned sync store).** Rejected as four subprojects in one ship. A scoped both-transport V1 is viable only with written ceilings (http://-only or proxied HTTP, lazy-restart-only, no security matrix).
- **Adopt `rmcp` (official SDK) as the single new dependency.** Open alternative for decision 1: supplies stdio + streamable-HTTP clients and session handling; keeps `McpUpstreamTransport` as a thin test seam. Cost: first dependency in a zero-dep project.
- **Externalize supervision via mcp-proxy (wrap stdio servers as localhost HTTP, zero process code).** Rejected for V1 as the primary story (ops friction, per-server sidecars), kept as the fallback for the four HTTPS servers under a no-dependency decision.
- **Second versioning/sync store for ExternalSync.** Rejected: `version_history`, `rollback_tool`, WAL snapshot compaction already exist. Auto-approved `ConfigDiff` rows with `origin` + one `apply_external_sync()` funnel + approve-time CAS is the smaller correct design.
- **Single `security_mode` flag driving auth + network policy.** Rejected: the two axes diverge (strict Bearer + LAN upstreams is a real deployment shape), the matrix would rewire auth that is 70% already correct, and "localhost denied by default" contradicts the local fleet. Keep `auth_secret`, add the SSRF guard function, provision dev tokens explicitly.
- **Background sync daemon.** Out of scope from the start; the word "automatically" was the only trace of it, and that wording is corrected above.

## Key Decisions & Staging for Next Steps

### Carried forward (review consensus)

1. One registry: `ProtocolType::ExternalMcp` + tagged transport-config enum on `ServerRecord`; runtime state (pid, handles, session id, health) lives in a separate `UpstreamManager` wired in `server.rs`, never in WAL records. The transport trait goes in a neutral module (e.g. `src/upstream/`) so control plane and data plane both depend downward.
2. External sync = `apply_external_sync()` writing auto-approved `ConfigDiff` rows (`origin: external_sync`, before/after populated) + per-field provenance (`schema_source: Upstream | Human`) + `approve_diff` version guard (mismatch → 409). `ChangeOrigin` is net-new; existing records default to human.
3. Retired tools: `ToolStatus::Unavailable` variant (or `Disabled` + `retired_at`), with revival policy: a retired tool that reappears keeps prior enablement only if its schema is unchanged; otherwise lands `PendingReview`.
4. Roles: register/deregister/rediscover/enable/disable/allowlist = Admin; propose = Operator; approve = Approver; read/test = Agent. Specific arms before the `/api/servers/` prefix arm. Stdio allowlist is config-owned (`OMNI_MCP_STDIO_ALLOWLIST`, canonicalized absolute argv[0], validated at registration and spawn); registration failure is a clean 4xx naming the key — no half-success rows.
5. Credentials: `{env:NAME}`/`{file:PATH}` refs in every credential position (bearer, headers, env maps); `{file:}` canonicalized and prefix-checked against a secrets dir (`OMNI_MCP_SECRETS_DIR`); resolved values never in WAL, diffs, logs, audit, or UI; children get a scrubbed env of declared refs only.
6. Cancellation/timeout: `OMNI_MCP_UPSTREAM_CALL_TIMEOUT` (default 120s); deadline → abandon id + audit; never kill a child mid-call. Stdio concurrency: one Mutex per server in V1.
7. UI: centralized `api()` client with Bearer + explicit dev-token provisioning first (prerequisite zero), then the panel; RBAC acceptance is manual verification in V1; URLs encode ids (`encodeURIComponent`).
8. Naming: registry id `external-mcp:<slug>`, sanitized-identifier collision checks (servers + tools) at registration and sync → 409 naming both parties; demo seed gated at the whole-block level; `ProtocolType::from_str` gets the `ExternalMcp` arm + downgrade note + compat tests.
9. Acceptance tests run against a real fake MCP server (e.g. `npx @modelcontextprotocol/server-everything`) asserting a nonce echo — never the muttley mock, which synthesizes responses and can green-pass with zero upstream traffic. Add: gateway shutdown leaves no child processes; stderr flood does not block tool calls.

### Open decisions (need a call before /egm-draft finishes)

1. **TLS strategy** — dependency (rustls behind `transport-http`, or `rmcp`), or http://-only with the four HTTPS servers wrapped in local proxies / deferred to V1.5. This decides whether the HTTP transport ships at all in V1.
2. **Windows process containment** — accept documented orphan-on-crash (graceful-shutdown tree-kill only), or allow a `windows-sys`-style dependency + `unsafe` carve-out for Job Objects.
3. **RBAC acceptance scope** — manual verification (V1), or add a Playwright smoke while building the panel.

### Staging (order-of-work)

0. UI auth client + dev-token story (fixes existing breakage; unblocks all UI acceptance).
1. Role-matrix fix (Admin before prefix) before any registration route exists.
2. Decision 1 + timeout/cancel policy → gate the HTTP transport; decision 2 + allowlist/spawn rules → gate the stdio transport.
3. `apply_external_sync` + origin-tagged ConfigDiffs + CAS guard (blocks rediscover and the approval-UI filter).
4. Data layer: transport enum on `ServerRecord`, WAL compat tests.
5. STDIO minimal client (absolute-path allowlist, scrubbed env, stderr drain, CRLF-tolerant framing, lazy restart) — covers 13/17 fleet servers.
6. HTTP minimal client per decision 1 (POST-only JSON, `Mcp-Session-Id` round-trip, session-expiry re-init, hard actionable errors naming ceilings).
7. Panel UI (MCPJungle register surface minus OAuth/tool-groups/raw secrets, plus inspect + reconciliation counts).
8. Acceptance suite per the rewritten criteria: end-to-end register+invoke with nonce echo (one stdio + one HTTP), collision 409s at registration and sync, restart persists registrations with zero secret bytes in `registry.db`/`-wal`, rediscover preserves enablement and returns counts, Enabled external tools appear in BM25/Code Mode/declarations, UI manually verified under Bearer auth, shutdown leaves no orphans, stderr flood does not block calls.

Rewritten acceptance wording (fixes the untestable originals): "after an operator enables the server and its tools, external tools appear in BM25 search, Code Mode, and generated type declarations"; "schema sync is operator-triggered; within a triggered sync, changes apply atomically without an approval step, tagged `origin: external_sync`, with before/after rows preserved for rollback"; "register + invoke end-to-end against a named local fake MCP server in development mode, asserting echoed nonce output from the real upstream path".
