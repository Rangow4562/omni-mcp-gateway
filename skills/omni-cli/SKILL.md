---
name: omni-cli
description: Operate OmniMCP from any terminal or CLI-first coding agent using omni, aifx, omni-gateway, or npx @rangow2260/omni-mcp for tool search, projected calls, Code Mode, skills installation, enterprise plugin packaging, Wasm registration, and migrations.
---

# OmniMCP CLI Skill (`v6.8.5`)

Use this skill when driving OmniMCP from a shell or CLI-first coding agent. All commands work interchangeably via `omni`, `aifx`, `omni-gateway`, or `npx -y @rangow2260/omni-mcp`.

## 1. Quick Discovery & Tool Invocation

```bash
# List registered services (table or JSON)
omni list --format json

# BM25 search across all tools
omni search "driver location" --format json

# Call a tool with response field projection
omni call driver_dispatch get_driver_location \
  --args '{"driver_id":"drv_1"}' \
  --project 'driver_id,status,latitude,longitude'
```

## 2. Code Mode, Types, Simulation & Resume

```bash
# Generate 0-turn BM25 TypeScript declarations for an intent
omni types --intent "refund a disputed ride"

# Execute a sandboxed Code Mode workflow
omni codemode --code 'async () => { return await codemode.driver_dispatch.get_driver_location({ driver_id: "drv_1" }); }'

# Require HITL approval for mutations & resume as an Approver
omni codemode --code 'async () => { return await codemode.accounting.refund({ ride_id: "r_1" }); }' --require-approval
omni resume <execution_id> --approver security_lead

# Deterministic Simulation Testing (DST) with chaos faults
omni simulate --code 'async () => { ... }' --seed 42 --runs 10
```

## 3. Stdio MCP Server & Tool Group Isolation

```bash
# Full gateway over stdio
omni-gateway --stdio

# Strictly isolated single service over stdio
omni-gateway --stdio --server driver_dispatch

# Strictly isolated Tool Group over stdio (enforces tools/resources/prompts/skills isolation)
omni-gateway --stdio --group billing
```

Note: `--server` and `--group` are mutually exclusive and cannot be combined.

## 4. Universal Agent Skills & Enterprise Plugin CLI

```bash
# Offline install of the 4 built-in skills into any coding agent
omni skills install --agent claude --scope project
omni skills install --agent all --scope project --dir ./my-repo --dry-run

# Seed built-in usage skills into the live MCP skills/list registry
omni skills bootstrap
omni skills list
omni skills publish ./path/to/skill-dir --group billing
omni skills verify omni-mcp

# Generate Standard (local stdio) or Enterprise (remote HTTPS + OIDC + Tool Group) plugins
omni plugin agents
omni plugin init --mode standard --agent claude --out ./omni-plugin
omni plugin init --mode enterprise --agent all \
  --gateway-url https://mcp.corp.example.com \
  --group billing \
  --auth-env OMNI_AUTH_TOKEN \
  --out ./dist/enterprise-plugin

# Export air-gapped MDM bundle & cryptographically verify
omni plugin export --enterprise --gateway-url https://mcp.corp.example.com --include-registry-skills --out ./dist/mdm-bundle
omni plugin verify ./dist/mdm-bundle
```

## 5. Config Import, Wasm Tools, Migration & Benchmarks

```bash
omni import ./mcp.json
omni wasm register custom_srv calc ./calc.wasm --description "Sandboxed calculator"
omni migrate --to-v6.8.5 --target postgres --database-url "$OMNI_DATABASE_URL"
omni bench --iterations 1000
```

See `references/cli-cheatsheet.md` for all flags and offline vs. online command guarantees.
