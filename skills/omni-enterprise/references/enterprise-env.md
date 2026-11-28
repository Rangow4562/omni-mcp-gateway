# OmniMCP Enterprise Environment Variables (`v6.8.5`)

## Network & Authentication
| Variable | Default | Description |
| :--- | :--- | :--- |
| `OMNI_HOST` | `127.0.0.1` | Bind host (`0.0.0.0` requires auth configured). |
| `OMNI_PORT` | `8080` | Bind port. |
| `OMNI_PUBLIC_BASE_URL` | `http://<host>:<port>` | External HTTPS URL advertised in OAuth metadata & manifests. |
| `OMNI_REQUIRE_MCP_AUTH` | `true` (non-loopback), `false` (loopback) | Enforces HTTP 401 + RFC 9728 on `/omni/mcp`, `/groups/<slug>/mcp`, `/<service>/mcp`. |
| `OMNI_AUTH_SECRET` | _(empty)_ | Comma-separated `role:secret[:id]` static bearer credentials. |
| `OMNI_JWT_SECRET` | _(empty)_ | HS256 JWT signing secret. |
| `OMNI_OIDC_JWKS_URI` | _(empty)_ | OIDC JWKS endpoint for RS256/ES256/EdDSA JWT validation. |
| `OMNI_OIDC_ISSUER` | _(empty)_ | Expected `iss` claim for OIDC tokens. |
| `OMNI_OIDC_AUDIENCE` | _(empty)_ | Expected `aud` claim (RFC 8707 resource indicator). |
| `OMNI_CORS_ORIGINS` | _(empty)_ | Comma-separated allowed browser origins. |

## Skills, Tasks & Extensions
| Variable | Default | Description |
| :--- | :--- | :--- |
| `OMNI_ENABLE_SKILLS_EXTENSION` | `true` | Enable SEP-2640 `io.modelcontextprotocol/skills`. |
| `OMNI_SEED_BUILTIN_SKILLS` | `false` | Opt-in auto-seeding of the 4 built-in usage skills on startup. |
| `OMNI_ENABLE_TASKS_EXTENSION` | `true` | Enable MCP Tasks GA (`tasks/get`, `tasks/update`, `tasks/cancel`). |
| `OMNI_TASK_SYNC_GRACE_MS` | `250` | Duration before async promotion to durable task. |
| `OMNI_MAX_CONCURRENT_TASKS` | `64` | Maximum concurrent background tasks per pod. |
| `OMNI_ENABLE_FILE_UPLOADS_EXTENSION` | `false` | Enable SEP-2356 streaming file uploads (`x-mcp-file`). |
| `OMNI_FILE_STORAGE_BACKEND` | `local` | `local`, `s3`, `gcs`, `azure`, `postgres`, or `http`. |

## Storage & Observability
| Variable | Default | Description |
| :--- | :--- | :--- |
| `OMNI_STORAGE_BACKEND` | `wal` | `wal`, `sqlite`, or `postgres`. |
| `OMNI_DATABASE_URL` | _(empty)_ | PostgreSQL DSN or SQLite file path. |
| `OMNI_PG_POOL_SIZE` | `8` | `PgWirePool` connection pool size (1–128). |
| `OMNI_OTLP_ENDPOINT` | _(empty)_ | OpenTelemetry OTLP/HTTP trace export endpoint. |
| `OMNI_ENABLE_PII_REDACTION` | `true` | Automatic PII redaction on logs and responses. |
