"""
Example MCP Client & Harness Agent using the official `mcp` Python SDK
to connect to the Omni MCP Gateway over stdio.
"""

import asyncio
from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client

async def run_harness_agent():
    # Option 1: Use portable npx distribution (zero installation required)
    server_params = StdioServerParameters(
        command="npx",
        args=["-y", "@rangow2260/omni-mcp", "--stdio"]
    )

    # Option 2: Use direct compiled binary
    # server_params = StdioServerParameters(
    #     command=r"D:\Personal\Memory\omni-mcp\target\release\omni-gateway.exe",
    #     args=["--stdio"],
    #     env={"OMNI_DATA_DIR": r"D:\Personal\Memory\omni-mcp\target\data"}
    # )

    async with stdio_client(server_params) as (read_stream, write_stream):
        async with ClientSession(read_stream, write_stream) as session:
            # Initialize session
            init_res = await session.initialize()
            print(f"[+] Initialized MCP session with {init_res.serverInfo.name} v{init_res.serverInfo.version}")

            # List available Omni discovery tools
            tools = await session.list_tools()
            print(f"[+] Available tools ({len(tools.tools)}): {[t.name for t in tools.tools]}")

            # 1. Discover microservices matching "driver"
            discover_res = await session.call_tool(
                "discover_server",
                {"query": "driver", "limit": 3}
            )
            print("[+] Discovered servers:\n", discover_res.content[0].text)

            # 2. Discover tools under driverdispatchservice
            tools_res = await session.call_tool(
                "discover_tools",
                {"server_name": "driverdispatchservice"}
            )
            print("[+] Tools under driverdispatchservice:\n", tools_res.content[0].text)

            # 3. Invoke tool with field projection
            call_res = await session.call_tool(
                "invoke_tool",
                {
                    "server_name": "driverdispatchservice",
                    "tool_name": "GetDriverLocation",
                    "arguments": {"driver_id": "drv_102"},
                    "_projection": ["latitude", "longitude", "status"]
                }
            )
            print("[+] Invoked GetDriverLocation with projection:\n", call_res.content[0].text)

if __name__ == "__main__":
    asyncio.run(run_harness_agent())
