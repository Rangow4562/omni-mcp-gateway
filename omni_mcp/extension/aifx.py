"""
aifx CLI - Uber's CLI for agentic operations and Code Mode tool execution.
Commands:
- aifx mcp list
- aifx mcp search <query>
- aifx mcp call <server> <tool> [--args '<json>'] [--project '<fields>'] [--output <path>]
"""
from __future__ import annotations
import argparse
import json
import sys
from typing import Optional, List, Dict, Any
from omni_mcp.models.entities import ToolStatus
from omni_mcp.models.rpc import RPCRequest
from omni_mcp.control_plane.registry import MCPRegistry
from omni_mcp.data_plane.cache import InMemoryConfigCache
from omni_mcp.data_plane.gateway import MCPProxyGateway


class AifxCLI:
    """Implements Code Mode for coding agents operating in shell environments."""

    def __init__(self, registry: Optional[MCPRegistry] = None, gateway_url: str = "http://localhost:8080") -> None:
        self.gateway_url = gateway_url
        if registry is not None:
            self.registry = registry
        else:
            from omni_mcp.server import get_gateway_context
            ctx = get_gateway_context()
            self.registry = ctx.registry

        self.cache = InMemoryConfigCache(self.registry)
        self.gateway = MCPProxyGateway(self.cache)

    def list_servers(self, format_type: str = "table") -> str:
        self.cache.refresh_now()
        servers = self.cache.list_servers()
        if format_type == "json":
            out = []
            for s in servers:
                out.append({
                    "name": s.name,
                    "displayName": s.display_name,
                    "protocol": s.protocol.value,
                    "status": s.status.value,
                    "enabledTools": sum(1 for t in s.tools.values() if t.status == ToolStatus.ENABLED),
                    "totalTools": len(s.tools),
                })
            return json.dumps(out, indent=2)

        # Table format
        lines = [
            f"{'SERVER NAME':<25} {'PROTOCOL':<12} {'STATUS':<10} {'ENABLED/TOTAL':<15} {'DESCRIPTION':<35}",
            "-" * 100,
        ]
        for s in servers:
            enabled = sum(1 for t in s.tools.values() if t.status == ToolStatus.ENABLED)
            lines.append(
                f"{s.name:<25} {s.protocol.value:<12} {s.status.value:<10} {f'{enabled}/{len(s.tools)}':<15} {s.description[:33]:<35}"
            )
        return "\n".join(lines)

    def search_tools(self, query: str, format_type: str = "table") -> str:
        self.cache.refresh_now()
        servers = self.cache.list_servers()
        results: List[Dict[str, Any]] = []
        tokens = query.lower().split()

        for s in servers:
            for t in s.tools.values():
                score = 0
                search_text = f"{s.name} {t.name} {t.description}".lower()
                for tok in tokens:
                    if tok in search_text:
                        score += 1
                if score > 0 or not tokens:
                    results.append({
                        "server": s.name,
                        "tool": t.name,
                        "status": t.status.value,
                        "protocol": t.protocol.value,
                        "description": t.description,
                    })

        if format_type == "json":
            return json.dumps(results, indent=2)

        lines = [
            f"{'SERVER':<22} {'TOOL':<28} {'STATUS':<10} {'PROTOCOL':<10} {'DESCRIPTION':<40}",
            "-" * 115,
        ]
        for r in results:
            lines.append(
                f"{r['server']:<22} {r['tool']:<28} {r['status']:<10} {r['protocol']:<10} {r['description'][:38]:<40}"
            )
        return "\n".join(lines)

    def call_tool(
        self,
        server_name: str,
        tool_name: str,
        arguments_json: str = "{}",
        projection: Optional[str] = None,
        output_file: Optional[str] = None,
        actor: str = "agent",
        token: Optional[str] = None,
    ) -> str:
        self.cache.refresh_now()
        try:
            args = json.loads(arguments_json) if arguments_json else {}
        except json.JSONDecodeError as ex:
            return json.dumps({"error": f"Invalid JSON in --args: {str(ex)}"}, indent=2)

        if projection:
            paths = [p.strip() for p in projection.split(",") if p.strip()]
            args["_projection"] = paths

        req = RPCRequest(
            id=1,
            method="tools/call",
            params={"name": tool_name, "arguments": args},
        )

        resp = self.gateway.handle_request(server_name, req, caller_actor=actor, user_token=token)
        out_str = json.dumps(resp.to_dict(), indent=2)

        if output_file:
            with open(output_file, "w", encoding="utf-8") as f:
                f.write(out_str)
            return f"Written response ({len(out_str)} bytes) to {output_file}"

        return out_str


def main(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        prog="aifx",
        description="Uber's CLI for agentic operations and Code Mode MCP execution",
    )
    subparsers = parser.add_subparsers(dest="subcommand", help="Subcommand")

    # aifx mcp ...
    mcp_parser = subparsers.add_parser("mcp", help="MCP Gateway commands")
    mcp_sub = mcp_parser.add_subparsers(dest="mcp_command", help="MCP action")

    # aifx mcp list
    list_p = mcp_sub.add_parser("list", help="List available MCP servers")
    list_p.add_argument("--format", choices=["table", "json"], default="table")

    # aifx mcp search
    search_p = mcp_sub.add_parser("search", help="Search for tools across all MCP servers")
    search_p.add_argument("query", nargs="?", default="", help="Search query")
    search_p.add_argument("--format", choices=["table", "json"], default="table")

    # aifx mcp call
    call_p = mcp_sub.add_parser("call", help="Invoke an MCP tool through MCP Gateway")
    call_p.add_argument("server", help="Server name")
    call_p.add_argument("tool", help="Tool name")
    call_p.add_argument("--args", default="{}", help="JSON arguments string")
    call_p.add_argument("--project", default="", help="Comma-separated field projection paths")
    call_p.add_argument("--output", "-o", default="", help="File path to write output directly to")
    call_p.add_argument("--actor", default="agent", help="Actor type (human, service, agent)")
    call_p.add_argument("--token", default="uber_token_default", help="User/service token")

    args = parser.parse_args(argv)
    cli = AifxCLI()

    if args.subcommand == "mcp":
        if args.mcp_command == "list":
            print(cli.list_servers(format_type=args.format))
            return 0
        elif args.mcp_command == "search":
            print(cli.search_tools(query=args.query, format_type=args.format))
            return 0
        elif args.mcp_command == "call":
            output = cli.call_tool(
                server_name=args.server,
                tool_name=args.tool,
                arguments_json=args.args,
                projection=args.project or None,
                output_file=args.output or None,
                actor=args.actor,
                token=args.token,
            )
            print(output)
            return 0
        else:
            mcp_parser.print_help()
            return 1
    else:
        parser.print_help()
        return 0


if __name__ == "__main__":
    sys.exit(main())
