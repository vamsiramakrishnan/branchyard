"""Call the company's connectors from Python (code mode).

    from wlsdk import call, tools, find
    find("jira")                      # names of tools whose name contains "jira"
    tools("servicenow_get")           # [{name, description, input_schema}] for matching tools
    rows = call("jira_search_issues", run_id=RUN, jql="...")

Every call goes to the same served company as the MCP tools, so state is shared.
`call` returns parsed JSON (or {"text": ...}) and raises ToolError when the tool reports an error.
"""

import asyncio
import json
import os

from mcp import ClientSession
from mcp.client.streamable_http import streamable_http_client

URL = os.environ.get("WORLDLOOM_MCP_URL", "http://127.0.0.1:8765/mcp")
_HIDDEN = {"eval_grade", "eval_trace", "eval_begin", "eval_end", "eval_score", "eval_list"}


class ToolError(Exception):
    pass


async def _with_session(fn):
    async with streamable_http_client(URL) as (r, w), ClientSession(r, w) as s:
        await s.initialize()
        return await fn(s)


def _list():
    async def go(s):
        out, cursor = [], None
        while True:
            page = await (s.list_tools(cursor=cursor) if cursor else s.list_tools())
            out.extend(page.tools)
            cursor = page.next_cursor
            if not cursor:
                return out

    return [t for t in asyncio.run(_with_session(go)) if t.name not in _HIDDEN]


def find(fragment=""):
    return [t.name for t in _list() if fragment in t.name]


def tools(fragment=""):
    return [
        {"name": t.name, "description": t.description, "input_schema": t.input_schema}
        for t in _list()
        if fragment in t.name
    ]


def call(tool, **arguments):
    if tool in _HIDDEN:
        raise ToolError(f"{tool} is reserved for the evaluator")

    async def go(s):
        res = await s.call_tool(tool, arguments)
        text = "".join(getattr(c, "text", "") for c in (res.content or []))
        try:
            value = json.loads(text) if text else None
        except ValueError:
            value = {"text": text}
        if res.is_error:
            raise ToolError(f"{tool}: {text[:2000]}")
        return value

    return asyncio.run(_with_session(go))
