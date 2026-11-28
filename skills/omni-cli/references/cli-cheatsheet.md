# OmniMCP CLI Cheatsheet (`v6.8.5`)

## Pure-Offline Commands (Zero `~/.omni/data` or Postgres Side Effects)

These commands execute hermetically without initializing gateway storage:
- `omni skills install --agent <target> [--scope user|project] [--dir <path>] [--only <slug,...>] [--dry-run] [--force]`
- `omni plugin agents`
- `omni plugin init --mode standard|enterprise --agent <target> [--gateway-url <url>] [--group <slug>] [--auth-env <VAR>] [--transport stdio|http|mcp-remote] [--out <dir>] [--allow-insecure-http] [--dry-run]`
- `omni plugin export --enterprise [--gateway-url <url>] [--group <slug>] [--auth-env <VAR>] [--out <dir>]` (offline unless `--include-registry-skills` is passed)
- `omni plugin verify <dir>`
- `omni --version` / `omni --help`

## Supported `--agent` Targets

`claude`, `antigravity`, `gemini`, `codex`, `cursor`, `copilot`, `vscode`, `windsurf`, `cline`, `opencode`, `universal`, `all`.

## Online / Registry Commands

- `omni list [--format table|json]`
- `omni search [query] [--format table|json]`
- `omni call <server> <tool> [--args '<json>'] [--project '<fields>'] [--output <path>] [--actor <actor>]`
- `omni codemode --code '<script>' [--require-approval]`
- `omni resume <execution_id> [--approver <name>]`
- `omni simulate --code '<script>' [--seed <u64>] [--runs <N>]`
- `omni types [--server <name>] [--query <q>] [--intent '<prompt>']`
- `omni skills list [--group <slug>]`
- `omni skills publish <dir> [--group <slug>] [--slug <slug>]`
- `omni skills verify <slug>`
- `omni skills bootstrap [--group <slug>]`
- `omni import <path/to/mcp.json>`
- `omni wasm register <server> <tool> <path.wasm> [--description <text>]`
- `omni migrate [--to-v6 | --to-v6.5 | --to-v6.8 | --to-v6.8.5 --target sqlite|postgres --database-url <dsn>]`
- `omni bench [--iterations <N>]` / `omni bench-codemode [--iterations <N>]`
