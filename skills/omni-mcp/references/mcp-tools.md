# OmniMCP Gateway Tool Reference (`v6.8.5`)

## Discovery & Inspection

### `discover_server`
- **Arguments**: `{ "query"?: string, "status"?: "active" | "inactive" }`
- **Returns**: Array of registered services with protocol (`grpc`, `http`, `thrift`, `external-mcp`, `wasm`), status, and tool counts.

### `discover_tools`
- **Arguments**: `{ "query"?: string, "server_name"?: string, "limit"?: number }`
- **Returns**: BM25-ranked tools with `server_name`, `tool_name`, `description`, and relevance score.

### `get_tool_schema`
- **Arguments**: `{ "server_name": string, "tool_name": string }`
- **Returns**: Full JSON Schema (`inputSchema`), mutation/idempotency classification, and rate-limit metadata.

### `recommend_tools`
- **Arguments**: `{ "intent": string, "max_steps"?: number }`
- **Returns**: Ordered tool pipeline recommendation with rationale.

## Invocation

### `invoke_tool`
- **Arguments**:
  ```json
  {
    "server_name": "driver_dispatch",
    "tool_name": "get_driver_location",
    "arguments": { "driver_id": "drv_101" },
    "_projection": "driver_id,status,latitude,longitude"
  }
  ```
- **Tip**: Always supply `_projection` on list/search endpoints to reduce context token usage.

### `batch_invoke_tools`
- **Arguments**:
  ```json
  {
    "calls": [
      { "server_name": "driver_dispatch", "tool_name": "get_driver_location", "arguments": { "driver_id": "drv_1" } },
      { "server_name": "accounting", "tool_name": "get_balance", "arguments": { "account_id": "acc_1" } }
    ]
  }
  ```

### `explain_error`
- **Arguments**: `{ "code": number, "message"?: string, "server_name"?: string, "tool_name"?: string }`
- **Returns**: Root cause analysis, circuit breaker status, and actionable next steps.

## Code Mode Orchestration

### `get_codemode_types`
- **Arguments**: `{ "server_name"?: string, "query"?: string, "intent"?: string }`
- **Returns**: Strict TypeScript `.d.ts` ambient declarations for `codemode.<service>.<tool>(args)`.

### `codemode`
- **Arguments**: `{ "code": string, "require_mutation_approval"?: boolean, "max_operations"?: number }`
- **Returns**: Execution output or `{ "status": "APPROVAL_REQUIRED", "execution_id": "..." }`.

### `resume_codemode`
- **Arguments**: `{ "execution_id": string, "approved": boolean, "approver"?: string, "reason"?: string }`
- **Returns**: Resumed execution output (Separation-of-Duties enforced: approver must differ from initiator).

### `simulate_codemode`
- **Arguments**: `{ "code": string, "seed"?: number, "runs"?: number, "fault_rate"?: number }`
- **Returns**: Deterministic simulation report across seeded runs with fault injection and Saga rollback verification.
