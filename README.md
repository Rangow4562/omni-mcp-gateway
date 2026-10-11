# Omni MCP (`omni-mcp`)

A production-grade, enterprise Model Context Protocol (MCP) management platform and runtime gateway reconstructing the architecture documented in [`D:\Personal\Memory\docv\www-uber-com.md`](file:///D:/Personal/Memory/docv/www-uber-com.md) (*"Designing MCP Gateway: Uber's MCP Management Platform"*, October 1, 2026).

Omni MCP unifies hundreds of microservices and thousands of tools behind a standardized, token-optimized gateway with built-in governance, protocol translation, incremental discovery, and security.

<p align="center">
  <img src="assets/omni-mcp-banner.png" alt="Omni MCP Gateway Banner" width="100%" />
</p>

---

## Key Features & Production Architecture

- **Control Plane**:
  - **AutoCrawler**: Cadence-style distributed discovery engine scanning IDL repositories and service signals.
  - **IDL Parsers**: Extracts Protobuf (`.proto`) and Thrift (`.thrift`) service definitions and translates them into JSON-RPC 2.0 schemas.
  - **LLM Description Generator**: Synthesizes agent-friendly tool documentation.
  - **Native MCPFx Monitor**: Automatic heartbeat listener discovering native MCP servers and their exposed tools.
  - **3P Token Exchange**: Secure authentication bridge for Jira and Google integrations.
  - **Governance & Approvals**: Strict *disabled-by-default* exposure gate, config change diff approvals, and one-click rollback.
  - **Durable Persistence**: SQLite WAL storage engine (`SQLiteRegistryStore`) ensuring servers, tools, diffs, and audit logs persist across restarts.
- **Data Plane**:
  - **Dynamic Virtual Servers**: Routes `/<service-name>/mcp` dynamically without restarts or redeployments.
  - **In-Memory Config Cache**: Sub-millisecond routing table refreshed on a fixed background cadence.
  - **Wire Protocol Translation**: Serializes JSON-RPC to HTTP REST, gRPC (Protobuf), and TChannel (Thrift).
  - **Muttley Service Mesh Adapter**: Downstream sidecar proxying with trace IDs and latency metrics.
  - **ABAC & Charter Policies**: Tool and server-level access rules distinguishing `agent`, `human`, and `service` actors.
  - **Out-of-the-Box PII Redaction**: Automatic regex scrubbing for emails, phone numbers, SSNs, credit cards, and bearer tokens.
  - **Production Rate Limiting**: Token-bucket rate limiter per caller token and actor.
- **Scale & Token Optimization**:
  - **Omni MCP Proxy**: Single endpoint (`/omni/mcp`) supporting 4 incremental discovery tools (`discover_server`, `discover_tools`, `get_tool_schema`, `invoke_tool`) to prevent context bloat.
  - **Response Projection**: GraphQL-like nested path selection (`_projection`) that trims response payloads at runtime.
  - **Code Mode (`aifx` / `omni` CLI)**: Shell tool (`mcp list`, `mcp search`, `mcp call`) allowing agents to pipe outputs directly to disk for zero-context shell grep operations.
- **Production Observability & Operations**:
  - **Liveness & Readiness**: `/healthz` and `/readyz` endpoints.
  - **Prometheus Metrics**: `/metrics` emitting request counts, error counts, rate-limit hits, server gauges, and tool gauges.
  - **Structured Logging**: JSON log events with timestamps, trace IDs, actors, and latency.
  - **Container Ready**: Production `Dockerfile` and `docker-compose.yml`.
- **Interactive React UI (Figs 3 & 4)**:
  - React 18 + Vite frontend built directly into `ui/dist` and statically served by the backend.

---

## REA Claim Audit

| Document Claim | Document Statement | Omni MCP Implementation | Status |
| :--- | :--- | :--- | :--- |
| **800+ Servers / 5000+ Tools** | Gateway scales to enterprise volume without context exhaustion | High-throughput in-memory routing table backed by SQLite | **Verified Documented Baseline** |
| **Control vs Data Plane** | Centralized Registry (Control) vs Proxy Gateway (Data) | Decoupled `omni_mcp.control_plane` & `omni_mcp.data_plane` | **Verified Architecture** |
| **AutoCrawler** | Cadence workflow scanning IDL registry and native heartbeats | `AutoCrawler` workflow engine with Protobuf/Thrift AST parsers | **Verified Architecture** |
| **Disabled by Default** | Discovery doesn't imply exposure; tools require review & approval | Strict `DISABLED` state machine; requires explicit owner review | **Verified Principle** |
| **Diffs & Rollback** | Tool edits trigger diffs requiring approval; supports rollback | Versioned config diff engine with approval gates and rollback | **Verified Feature** |
| **Dynamic Routing** | Materializes virtual servers at `/<service-name>/mcp` without redeploy | Dynamic HTTP dispatch table refreshed at fixed cadence | **Verified Data Plane** |
| **Wire Translation** | Translates JSON-RPC to HTTP, gRPC, TChannel wire formats | `ProtocolTranslator` for HTTP, gRPC (Protobuf), TChannel (Thrift) | **Verified Data Plane** |
| **Muttley Delegation** | Downstream calls routed via Muttley service mesh sidecar | `MuttleyMeshClient` sidecar adapter with trace metadata | **Verified Integration** |
| **Security & ABAC** | Server/tool charter policies for humans, services, agents; PII redaction | ABAC charter policy engine + automated PII regex redactor | **Verified Security Model** |
| **Omni MCP** | Proxy enabling incremental discovery via 4 standard tools | Standalone Omni MCP server with semantic discovery | **Verified Context Feature** |
| **Response Projection** | GraphQL-like nested path array trimming response in runtime | `ResponseProjectionEngine` with nested key & array indexing | **Verified Token Optimization** |
| **Code Mode (`aifx`)** | CLI for coding agents: `mcp list`, `mcp search`, `mcp call` | Console script CLI supporting table/json output and file redirect | **Verified CLI Interface** |
| **Registry & Tool UI** | Web interfaces for reviewing configs, diffs, and testing (Figs 3 & 4) | High-fidelity React application served statically at `/` | **Verified Documented UI** |

---

## Architecture Diagram

<p align="center">
  <img src="assets/architecture.svg" alt="Omni MCP Architecture" width="100%" />
</p>

```
                    AI Agents / LLMs / Shell Coding Agents
                        │                         │
                        ▼                         ▼
                 Omni MCP Proxy              omni / aifx CLI
             (Incremental Discovery)          (Code Mode)
                        │                         │
                        └──────────┬──────────────┘
                                   │
                                   ▼
             ┌───────────────────────────────────────────────┐
             │       DATA PLANE (Proxy Gateway Runtime)      │
             │   Dynamic Virtual Servers: /<service-name>/mcp│
             ├───────────────────────────────────────────────┤
             │  • In-Memory Config Cache (Fixed Cadence Sync)│
             │  • Token-Bucket Rate Limiter (Per Actor/Key)  │
             │  • ABAC Charter Policy Engine (Actor Access)  │
             │  • Automatic PII / Sensitive Data Redaction   │
             │  • Response Projection Engine (Field Trimmer) │
             │  • Protocol Translators (JSON <-> Wire Bytes) │
             │  • Muttley Service Mesh Sidecar Client        │
             └───────────────────────┬───────────────────────┘
                                     │
                 ┌───────────────────┴───────────────────┐
                 ▼                                       ▼
        Downstream Services                     Native MCP Servers
     (HTTP / gRPC / TChannel)                     (via MCPFx)

                                   ▲
                                   │ (Syncs Config)
                                   │
             ┌───────────────────────────────────────────────┐
             │         CONTROL PLANE (MCP Registry)          │
             ├───────────────────────────────────────────────┤
             │  • SQLite Persistent Storage (WAL Durability) │
             │  • Disabled-by-Default Policy Enforcer        │
             │  • Config Diff Review, Approval & Rollback    │
             │  • AutoCrawler Cadence Engine                 │
             │  • IDL Parser (Protobuf & Thrift AST)         │
             │  • LLM Description Enrichment Generator       │
             │  • MCPFx Heartbeat Listener                   │
             │  • 3P OAuth Token Exchanger (Jira / Google)   │
             └───────────────────────────────────────────────┘
                                   ▲
                                   │
                    Service Owners / Developers
                   (React UI Dashboard: Figs 3 & 4)
```

---

## Codebase Structure

```
omni-mcp/
├── pyproject.toml              # Production packaging with console scripts (omni, omni-gateway, aifx)
├── Dockerfile                  # Multi-stage production container build
├── docker-compose.yml          # Containerized deployment with volume persistence
├── .env.example                # Configuration template
├── omni_mcp/
│   ├── __init__.py
│   ├── config.py               # Environment configuration with sensible defaults
│   ├── logging.py              # Structured JSON logging
│   ├── server.py               # Threaded HTTP & JSON-RPC gateway server, healthz, readyz, metrics
│   ├── models/
│   │   ├── entities.py         # ServerRecord, ToolRecord, ToolStatus, ProtocolType, CharterPolicy, ConfigDiff
│   │   └── rpc.py              # JSON-RPC 2.0 Request, Response, Error dataclasses & MCP constants
│   ├── storage/
│   │   └── sqlite_store.py     # SQLite persistent registry storage (WAL mode)
│   ├── control_plane/
│   │   ├── registry.py         # Versioned store, diff approvals, rollback engine
│   │   ├── crawler.py          # AutoCrawler Cadence workflow engine
│   │   ├── idl_parser.py       # Protobuf and Thrift IDL parser -> JSON Schema
│   │   ├── description_gen.py  # LLM description enrichment generator
│   │   ├── native_monitor.py   # MCPFx heartbeat monitor and listTools discovery
│   │   └── third_party.py      # 3P token exchange adapter (Jira, Google)
│   ├── data_plane/
│   │   ├── gateway.py          # Dynamic virtual server router (/<service-name>/mcp)
│   │   ├── cache.py            # In-memory config refresh worker
│   │   ├── rate_limiter.py     # Token-bucket rate limiting
│   │   ├── security.py         # ABAC charter policies & PII redactor
│   │   ├── projection.py       # Response Projection engine (GraphQL-like path trimming)
│   │   ├── translators.py      # HTTP, gRPC, and TChannel wire protocol translators
│   │   └── muttley.py          # Muttley service mesh sidecar client
│   └── extension/
│       ├── omni_mcp.py         # Omni MCP proxy server (4 incremental discovery tools)
│       └── aifx.py             # Code Mode CLI (omni / aifx mcp list / search / call)
├── ui/                         # React 18 + Vite dashboard (Figures 3 & 4)
│   └── dist/                   # Production built assets
└── tests/
    └── test_omni_mcp.py        # 18 comprehensive unit & integration tests
```

---

## Quick Start & Usage

### 1. Run the Gateway Server

```bash
# Starts the threaded server on port 8080
python -m omni_mcp.server 8080
```

Available Endpoints:
- **Interactive UI Dashboard**: `http://localhost:8080/`
- **Liveness Probe**: `http://localhost:8080/healthz`
- **Readiness Probe**: `http://localhost:8080/readyz`
- **Prometheus Metrics**: `http://localhost:8080/metrics`
- **Omni MCP Proxy**: `http://localhost:8080/omni/mcp`
- **Virtual Endpoints**: `http://localhost:8080/<service-name>/mcp`
- **REST Catalog API**: `http://localhost:8080/api/servers`, `http://localhost:8080/api/stats`

---

### 2. Code Mode CLI (`omni` / `aifx`)

```bash
# List all registered servers
python -m omni_mcp.extension.aifx mcp list

# Search for tools across all servers
python -m omni_mcp.extension.aifx mcp search driver

# Invoke tool through gateway with Response Projection
python -m omni_mcp.extension.aifx mcp call driverdispatchservice GetDriverLocation \
  --args '{"driver_id": "drv_102"}' \
  --project "data.driver.name,data.driver.vehicle.license_plate"

# Save tool response directly to file for zero-context shell grep
python -m omni_mcp.extension.aifx mcp call driverdispatchservice GetDriverLocation \
  --args '{"driver_id": "drv_102"}' \
  --output ./driver_info.json
```

---

### 3. Omni MCP Incremental Discovery (For LLM Agents)

AI agents connect to a single endpoint: `http://localhost:8080/omni/mcp`.
Agents discover and invoke tools in 4 incremental steps:

1. **`discover_server(query="find driver coordinates")`** -> Returns matching servers (`driverdispatchservice`).
2. **`discover_tools(server_name="driverdispatchservice")`** -> Returns enabled tools (`GetDriverLocation`).
3. **`get_tool_schema(server_name="driverdispatchservice", tool_name="GetDriverLocation")`** -> Returns parameter schema.
4. **`invoke_tool(server_name="driverdispatchservice", tool_name="GetDriverLocation", arguments={...}, _projection=["data.driver.name"])`** -> Returns trimmed, redacted result.

---

### 4. Running the Tests

```bash
python -m unittest discover -s tests
```

Output:
```
..................
----------------------------------------------------------------------
Ran 18 tests in 1.841s

OK
```
