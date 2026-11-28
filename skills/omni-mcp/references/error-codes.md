# OmniMCP JSON-RPC Error Codes (`v6.8.5`)

| Code | Name | Typical Cause | Remediation |
| :--- | :--- | :--- | :--- |
| `-32700` | `PARSE_ERROR` | Malformed JSON body | Validate JSON syntax and UTF-8 encoding. |
| `-32600` | `INVALID_REQUEST` | Missing `jsonrpc: "2.0"` or `method` | Include valid JSON-RPC 2.0 envelope. |
| `-32601` | `METHOD_NOT_FOUND` | Unknown RPC method or tool outside Tool Group scope | Check `tools/list` or verify `--group <slug>` / `/groups/<slug>/mcp` bindings. |
| `-32602` | `INVALID_PARAMS` | Missing or invalid arguments against `inputSchema` | Call `get_tool_schema` and fix argument types. |
| `-32001` | `RESOURCE_NOT_FOUND` | Unknown service, tool, skill, or Tool Group | Run `discover_server` or `skills/list`. |
| `-32002` | `UPSTREAM_ERROR` | Upstream MCP/gRPC/HTTP backend returned an error | Call `explain_error` or inspect W3C trace spans. |
| `-32003` | `HEADER_MISMATCH` | `Mcp-Method` or `Mcp-Name` header does not match payload | Align HTTP headers with JSON-RPC `method` and `params.name`. |
| `-32004` | `CIRCUIT_OPEN` | Upstream circuit breaker tripped after consecutive failures | Wait `OMNI_CB_RESET_SEC` or inspect upstream health. |
| `-32005` | `RATE_LIMITED` | Caller exceeded `OMNI_DEFAULT_RATE_LIMIT_RPM` | Back off per `Retry-After` header or batch calls with `codemode`. |
| `-32008` | `APPROVAL_REQUIRED` | Mutating tool requires HITL Approver sign-off | Wait for Approver via `resume_codemode` or Approval Inbox. |
| `-32020` | `MISSING_REQUIRED_CLIENT_CAPABILITY` | Required client capability unsupported | Check `server/discover` and `initialize` capabilities. |
| `HTTP 401` | `UNAUTHORIZED` | Missing/invalid Bearer or OIDC JWT (`OMNI_REQUIRE_MCP_AUTH=true`) | Authenticate with a valid `Agent`+ token; inspect `/.well-known/oauth-protected-resource`. |
| `HTTP 403` | `FORBIDDEN` | Untrusted `Origin` (DNS rebinding) or insufficient RBAC role | Use allowed origin and a principal with required `AuthRole`. |
