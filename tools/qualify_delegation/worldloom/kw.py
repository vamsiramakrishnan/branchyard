"""The evaluator side of a campaign: open graded runs, score answers, end runs.

Talks to a served Worldloom case set (WL_URL). Agents never get these calls: the campaign's
branchyard.toml blocks the eval_* tools for every harness.

  kw.py begin QID...            -> prints {qid: run_id}
  kw.py score RUN_ID ANSWER_FILE -> appends the eval_score document to $QUALIFY_SCORES and prints a summary
  kw.py end RUN_ID
  kw.py query QID               -> prints the task text
"""

import asyncio
import json
import os
import sys

from mcp import ClientSession
from mcp.client.streamable_http import streamable_http_client

URL = os.environ.get("WL_URL", "http://127.0.0.1:8765/mcp")


async def call(tool, args):
    async with streamable_http_client(URL) as (r, w), ClientSession(r, w) as s:
        await s.initialize()
        res = await s.call_tool(tool, args)
        text = res.content[0].text if res.content else "{}"
        try:
            return json.loads(text)
        except ValueError:
            return {"text": text, "is_error": res.is_error}


def main():
    cmd, *rest = sys.argv[1:]
    if cmd == "begin":
        out = {}
        for qid in rest:
            out[qid] = asyncio.run(call("eval_begin", {"query_id": qid}))["run_id"]
        print(json.dumps(out))
    elif cmd == "query":
        doc = asyncio.run(call("eval_list", {"limit": 50}))
        for q in doc["queries"]:
            if q["query_id"].startswith(rest[0]):
                print(json.dumps(q, indent=1))
    elif cmd == "score":
        run_id, answer_file = rest
        with open(answer_file) as f:
            answer = f.read()
        doc = asyncio.run(call("eval_score", {"run_id": run_id, "answer": answer}))
        with open(os.environ.get("QUALIFY_SCORES", "scores.jsonl"), "a") as f:
            f.write(json.dumps(doc) + "\n")
        score = doc.get("score", {})
        print(
            json.dumps(
                {
                    "case": doc.get("case_id", "")[:8],
                    "score": score.get("score"),
                    "passed": score.get("passed"),
                    "calls": doc.get("calls"),
                }
            )
        )
    elif cmd == "end":
        print(json.dumps(asyncio.run(call("eval_end", {"run_id": rest[0]}))))


main()
