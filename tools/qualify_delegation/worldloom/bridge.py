"""stdio MCP server that forwards tools/list and tools/call to a streamable-HTTP MCP server.

Usage: bridge.py URL. One upstream session per bridge process, opened lazily.
"""

import sys
from contextlib import asynccontextmanager

import anyio
from mcp import ClientSession, types
from mcp.client.streamable_http import streamable_http_client
from mcp.server.lowlevel import Server
from mcp.server.stdio import stdio_server

URL = sys.argv[1]


@asynccontextmanager
async def lifespan(_server):
    async with streamable_http_client(URL) as (r, w), ClientSession(r, w) as upstream:
        await upstream.initialize()
        yield {"upstream": upstream}


async def list_tools(ctx, params):
    upstream = ctx.lifespan_context["upstream"]
    tools, cursor = [], None
    while True:
        page = await upstream.list_tools(cursor=cursor) if cursor else await upstream.list_tools()
        tools.extend(page.tools)
        cursor = page.next_cursor
        if not cursor:
            break
    return types.ListToolsResult(tools=tools)


async def call_tool(ctx, params):
    upstream = ctx.lifespan_context["upstream"]
    return await upstream.call_tool(params.name, params.arguments or {})


server = Server("worldloom-bridge", lifespan=lifespan, on_list_tools=list_tools, on_call_tool=call_tool)


async def main():
    async with stdio_server() as (read, write):
        await server.run(read, write, server.create_initialization_options())


anyio.run(main)
