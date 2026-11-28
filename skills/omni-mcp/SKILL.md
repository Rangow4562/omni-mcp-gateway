---
name: omni-mcp
description: Discover, inspect, and invoke enterprise microservices and MCP tools through the OmniMCP Gateway v6.8.5 with response projection, batch execution, SEP-2640 Skills, Tasks GA, Elicitation, and Streaming File Uploads.
---

# OmniMCP Gateway Skill (`v6.8.5`)

Use this skill whenever you interact with an OmniMCP Gateway over stdio (`omni-gateway --stdio`) or Streamable HTTP (`/omni/mcp`, `/groups/<slug>/mcp`, `/<service>/mcp`).

## 1. Core Progressive-Discovery Workflow

Always follow progressive discovery to keep context windows lean:

1. **Discover services**: Call `discover_server` to list registered virtual services and external MCP servers.
2. **Search tools**: Call `discover_tools` with a natural-language `query` (Okapi BM25 ranked) or `server_name` filter.
3. **Inspect schema**: Call `get_tool_schema` for the exact `server_name` + `tool_name` before invoking.
4. **Invoke with projection**: Call `invoke_tool` with `_projection` (comma-separated dot-paths such as `"id,status,user.email"`) to prune large payloads by up to 98% before they enter the LLM context.
5. **Batch or orchestrate**: Use `batch_invoke_tools` for independent parallel calls, or switch to the `omni-codemode` skill (`codemode` + `get_codemode_types`) for multi-step workflows, loops, joins, and Saga rollbacks.

## 2. All 11 Built-In Gateway MCP Tools

| Tool | Purpose |
| :--- | :--- |
| `discover_server` | List active microservices & external MCP servers (`query`, `status`). |
| `discover_tools` | BM25 semantic search over tools (`query`, `server_name`, `limit`). |
| `get_tool_schema` | Fetch JSON Schema (`inputSchema`), description, and safety metadata for a tool. |
| `invoke_tool` | Execute a tool with optional `_projection`, `_meta`, and `task` options. |
| `batch_invoke_tools` | Execute multiple independent tool invocations in a single round-trip. |
| `recommend_tools` | Plan a multi-step tool pipeline for a high-level intent. |
| `explain_error` | Diagnose `-320xx` gateway errors, circuit breaker state, and remediation steps. |
| `codemode` | Run sandboxed TypeScript/JavaScript orchestration over `codemode.<service>.<tool>(...)`. |
| `get_codemode_types` | Generate strict TypeScript `.d.ts` declarations (supports 0-turn BM25 `intent`). |
| `resume_codemode` | Resume a HITL-suspended Code Mode execution after Approver sign-off. |
| `simulate_codemode` | Deterministic Simulation Testing (DST) with seeded fault/chaos injection. |

See `references/mcp-tools.md` for full parameter schemas and examples.

## 3. MCP Resources, Prompts, Skills, Tasks & Elicitation

- **Resources (`resources/list`, `resources/read`, `resources/templates/list`, `resources/directory/read`)**:
  - `omni://catalog/servers` — live service catalog
  - `omni://audit/merkle` — SHA-256 Merkle audit head & verification metadata
  - `skill://<slug>/SKILL.md` & `skill://<slug>/<path>` — SEP-2640 skill bundles
  - `ext://<server>/<uri>` — proxied external MCP resources (strictly scoped under `/groups/<slug>/mcp`)
- **Prompts (`prompts/list`, `prompts/get`)**: Federated external prompts exposed as `<server>:<prompt_name>`.
- **Skills (`skills/list`, `skills/get`)**: Curated skills and auto-synthesized Tool Group playbooks with deterministic `manifest_sha256`.
- **Tasks GA (`tasks/get`, `tasks/update`, `tasks/cancel`)**: Passing `task: { ttl: 60000 }` on `tools/call` promotes calls exceeding `OMNI_TASK_SYNC_GRACE_MS` (default `250ms`) to durable background tasks returning `resultType: "task"` and `taskId`.
- **Elicitation (`form` / `url`)**: Upstream or governance-triggered `elicitation/create` requests route to the Approval & Elicitation Inbox and resolve via `tasks/update` or REST `/api/v1/callbacks/<id>`.
- **Streaming File Uploads (`x-mcp-file`)**: Binary payloads upload via `PUT/POST /api/files/upload/<id>` and pass opaque file handles to tools without base64 bloat.

## 4. Error Handling & Remediation

When any call returns a JSON-RPC error in the `-320xx` range (e.g., `-32001` Not Found, `-32004` Circuit Open, `-32005` Rate Limited, `-32008` Approval Required), call `explain_error` with the error code and context. See `references/error-codes.md` for the full error code matrix.
