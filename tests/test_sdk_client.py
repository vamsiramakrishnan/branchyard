"""`sdk/python/branchyard_client.py` against a fake `by serve`: a stdlib
HTTP server scripted per route, so every claim the module makes about the
wire is checked without a real server. The real server is covered by
`crates/branchyard-cli/tests/sdk_client.rs`, which runs
`tests/sdk_client_e2e.py` against `by serve`.

Run: `python tests/test_sdk_client.py`.
"""

import base64
import contextlib
import inspect
import json
import os
import re
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
from collections import deque
from collections.abc import Callable
from functools import partial
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, ClassVar
from unittest import mock
from urllib.parse import parse_qs, urlsplit

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "sdk" / "python"))

import branchyard as bm  # noqa: E402
import branchyard_client as bc  # noqa: E402

TOKEN = "secret-token"
REPO = "/v1/repos/app"
# Routes the fake answers without the token, as the server does.
PUBLIC = ("/.well-known/branchyard",)
HANDOFF_MESSAGE: dict[str, Any] = {
    "id": 42,
    "from": "lead",
    "to": "consumer",
    "kind": "answer",
    "text": "Use these sources",
    "at_ms": 123,
    "delivered": False,
    "next_turn": True,
    "handoff": {
        "source": "source",
        "version": {"incarnation": 7, "turn": 3, "result_event": 17},
        "result": {"summary": "frozen evidence", "answer": None},
        "candidate": "candidate-commit",
        "artifacts": [
            {
                "id": "art7",
                "digest": "ab" * 32,
                "size": 8,
                "name": "evidence.md",
                "media_type": "text/markdown",
                "publisher_branch": "source",
                "turn": 3,
                "created_at": 120,
                "labels": {"kind": "evidence"},
            }
        ],
    },
}


def _error(code: str, message: str = "", detail: dict | None = None) -> dict:
    body: dict[str, Any] = {"code": code, "message": message or code}
    if detail is not None:
        body["detail"] = detail
    return {"error": body}


def _delta(seq: int, branch: str, text: str) -> dict:
    return {
        "seq": seq,
        "branch": branch,
        "at_ms": seq,
        "activity": {"harness": {"type": "message_delta", "turn": 1, "text": text}},
    }


def _prompt(seq: int, branch: str, text: str) -> dict:
    return {"seq": seq, "branch": branch, "at_ms": seq, "activity": {"prompt": text}}


def _ended(seq: int, branch: str) -> dict:
    return {
        "seq": seq,
        "branch": branch,
        "at_ms": seq,
        "activity": {"harness": {"type": "turn_ended", "turn": 1, "outcome": {"kind": "completed"}}},
    }


class Fake:
    """The scripted server's state. `routes[(method, path)]` is a deque of
    `(status, body, headers)` answers, or a callable taking the request
    body and returning one; a route without a script answers 404.
    `requests` records every request as `(method, path, headers, body)`.
    `feed` is the event stream's entries; `cut_after` closes a stream after
    that many entries (once), `hold` keeps a quiet stream open that long."""

    def __init__(self) -> None:
        self.routes: dict[tuple[str, str], Any] = {}
        self.requests: list[tuple[str, str, dict[str, str], Any]] = []
        self.feed: list[dict] = []
        self.cut_after: int | None = None
        self.hold: float = 0.0
        self.stream_opens: list[int | None] = []
        self.lock = threading.Lock()

    def answer(self, method: str, path: str, *answers: tuple[int, Any]) -> None:
        self.routes[(method, path)] = deque(answers)

    def always(self, method: str, path: str, status: int, body: Any) -> None:
        self.routes[(method, path)] = lambda _request: (status, body)

    def response(self, method: str, path: str, body: Any) -> tuple[int, Any]:
        script = self.routes.get((method, path))
        if script is None:
            return 404, _error("not_found", f"no route {method} {path}")
        if callable(script):
            return script(body)
        if not script:
            return 500, _error("internal", f"script for {method} {path} ran out")
        return script.popleft()


class Handler(BaseHTTPRequestHandler):
    fake: Fake
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args: Any) -> None:
        pass

    def _body(self) -> Any:
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        if self.headers.get("Content-Type", "").startswith("application/json") and raw:
            return json.loads(raw)
        return raw

    def _send(self, status: int, body: Any, headers: dict[str, str] | None = None) -> None:
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/octet-stream" if isinstance(body, bytes) else "application/json")
        self.send_header("Content-Length", str(len(data)))
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(data)

    def _handle(self, method: str) -> None:
        url = urlsplit(self.path)
        body = self._body()
        headers = {k.lower(): v for k, v in self.headers.items()}
        with self.fake.lock:
            self.fake.requests.append((method, self.path, headers, body))
        if url.path == "/healthz":
            self._send(200, b"ok")
            return
        if headers.get("authorization") != f"Bearer {TOKEN}" and url.path not in PUBLIC:
            self._send(401, _error("unauthorized", "no token"))
            return
        if url.path == f"{REPO}/events/stream":
            self._stream(url.query, headers)
            return
        status, answer = self.fake.response(method, url.path, body)
        extra: dict[str, str] = {}
        if isinstance(answer, tuple):
            answer, extra = answer
        self._send(status, answer, extra)

    def _stream(self, query: str, headers: dict[str, str]) -> None:
        cursor = headers.get("last-event-id")
        if cursor is None:
            values = parse_qs(query).get("cursor")
            cursor = values[0] if values else None
        start = int(cursor) if cursor is not None else (self.fake.feed[-1]["seq"] if self.fake.feed else 0)
        with self.fake.lock:
            self.fake.stream_opens.append(int(cursor) if cursor is not None else None)
            cut = self.fake.cut_after
            self.fake.cut_after = None
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        head = self.fake.feed[-1]["seq"] if self.fake.feed else 0
        self.wfile.write(f"event: open\nid: {start}\ndata: {json.dumps({'cursor': start, 'head': head})}\n\n".encode())
        self.wfile.flush()
        sent = 0
        for entry in self.fake.feed:
            if entry["seq"] <= start:
                continue
            if cut is not None and sent >= cut:
                break
            self.wfile.write(f"event: activity\nid: {entry['seq']}\ndata: {json.dumps(entry)}\n\n".encode())
            self.wfile.flush()
            sent += 1
        if cut is None and self.fake.hold:
            self.wfile.write(b": keep-alive\n\n")
            self.wfile.flush()
            time.sleep(self.fake.hold)
        self.close_connection = True

    def do_GET(self) -> None:
        self._handle("GET")

    def do_POST(self) -> None:
        self._handle("POST")

    def do_DELETE(self) -> None:
        self._handle("DELETE")


class ServerCase(unittest.TestCase):
    def setUp(self) -> None:
        self.fake = Fake()
        handler = type("H", (Handler,), {"fake": self.fake})
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        self.client = bc.Client(self.url, TOKEN, timeout=5.0)
        self.app = self.client.repo("app")
        self.stopped = False

    def stop(self) -> None:
        if not self.stopped:
            self.stopped = True
            self.server.shutdown()
            self.server.server_close()

    def tearDown(self) -> None:
        self.stop()

    def op(self, id: str, state: str, branch: str = "b", cursor: int = 0, end: int | None = None, **extra: Any) -> dict:
        op: dict[str, Any] = {
            "id": id,
            "repo": "app",
            "kind": "send",
            "state": state,
            "branches": [branch],
            "cursor": cursor,
            "created_at_ms": 1,
            "priority": 0,
        }
        if end is not None:
            op["end_cursor"] = end
        op.update(extra)
        return op

    def requests(self, method: str, path_prefix: str) -> list[tuple[str, str, dict[str, str], Any]]:
        return [r for r in self.fake.requests if r[0] == method and r[1].startswith(path_prefix)]


class Requests(ServerCase):
    def test_a_task_carries_the_token_a_key_and_the_grant_with_a_private_home(self):
        self.fake.answer("POST", f"{REPO}/tasks", (202, self.op("op1", "queued", "slack-c1", kind="task")))
        op = self.app.task(
            "hello",
            harness="claude-code",
            name="slack-c1",
            budget_usd=2.5,
            connectors=[bc.Grant.parse("github:write:issues.*"), "linear:read"],
            delegation={"max_depth": 1, "max_children": 2, "harnesses": []},
            allow_delegation=True,
        )
        self.assertEqual(op.id, "op1")
        self.assertEqual(op.state, "queued")
        self.assertFalse(op.done)
        self.assertFalse(op.replayed)
        ((_method, _path, headers, body),) = self.requests("POST", f"{REPO}/tasks")
        self.assertEqual(headers["authorization"], f"Bearer {TOKEN}")
        self.assertEqual(headers["content-type"], "application/json")
        self.assertEqual(len(headers["idempotency-key"]), 32)
        self.assertEqual(body["prompt"], "hello")
        self.assertEqual(body["budget"], {"max_usd": 2.5})
        # A grant needs a private home, so `isolated` follows the grant.
        self.assertIs(body["isolated"], True)
        self.assertEqual(
            body["provision"]["connectors"],
            [
                {"connector": "github", "operations": ["issues.*"], "mode": "write"},
                {"connector": "linear", "operations": ["*"], "mode": "read"},
            ],
        )
        self.assertEqual(body["delegation"], {"max_depth": 1, "max_children": 2, "harnesses": []})
        self.assertIs(body["allow_delegation"], True)
        # Defaults that are off are not sent at all.
        for absent in ("plan", "retry", "harnesses", "policy", "provider"):
            self.assertNotIn(absent, body)

    def test_a_given_key_is_sent_and_a_replay_is_marked(self):
        self.fake.answer(
            "POST",
            f"{REPO}/branches/b/send",
            (202, self.op("op2", "queued")),
            (200, (self.op("op2", "succeeded", end=3), {"Idempotent-Replayed": "true"})),
        )
        first = self.app.send("b", "again", key="k-1")
        second = self.app.send("b", "again", key="k-1")
        self.assertEqual(
            [r[2]["idempotency-key"] for r in self.requests("POST", f"{REPO}/branches/b/send")], ["k-1", "k-1"]
        )
        self.assertFalse(first.replayed)
        self.assertTrue(second.replayed)
        self.assertEqual(second.id, "op2")
        self.assertNotIn("_replayed", second)

    def test_a_send_without_connectors_sends_no_provision(self):
        self.fake.answer("POST", f"{REPO}/branches/b/send", (202, self.op("op3", "queued")))
        self.app.send("b", "more", budget_usd=1)
        body = self.requests("POST", f"{REPO}/branches/b/send")[0][3]
        self.assertEqual(body, {"prompt": "more", "budget": {"max_usd": 1}})

    def test_session_patch_is_explicit_and_does_not_merge_replacement_provisioning(self):
        self.fake.answer("POST", f"{REPO}/branches/b/send", (202, self.op("op-patch", "queued")))
        patch = {"model": "m", "effort": "low"}
        self.app.send("b", "more", session_patch=patch)
        body = self.requests("POST", f"{REPO}/branches/b/send")[0][3]
        self.assertEqual(body["session_patch"], patch)
        self.assertNotIn("provision", body)
        for invalid in [{}, {"model": " "}, {"model": None}, {"effort": True}, {"effort": []}, {"secrets": []}]:
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                self.app.send("b", "more", session_patch=invalid)
        for replacement in [{"provision": {}}, {"provision": {"secrets": []}}, {"connectors": []}]:
            with self.subTest(replacement=replacement), self.assertRaises(ValueError):
                self.app.send("b", "more", session_patch=patch, **replacement)
        self.assertEqual(len(self.requests("POST", f"{REPO}/branches/b/send")), 1)
        self.fake.answer("POST", f"{REPO}/branches/b/send", (202, self.op("op-replace", "queued")))
        self.app.send("b", "revoke", provision={"secrets": [], "mcp_servers": []})
        replacement = self.requests("POST", f"{REPO}/branches/b/send")[-1][3]
        self.assertEqual(replacement["provision"], {"secrets": [], "mcp_servers": []})
        self.assertNotIn("session_patch", replacement)

    def test_spawn_asks_for_the_childs_grant_in_the_object_form(self):
        self.fake.answer("POST", f"{REPO}/branches/p/spawn", (202, self.op("op4", "queued", "kid", kind="spawn")))
        self.app.spawn("p", "work", name="kid", connectors=["github:read:issues.list"], max_depth=0)
        body = self.requests("POST", f"{REPO}/branches/p/spawn")[0][3]
        self.assertEqual(body["connectors"], [{"connector": "github", "operations": ["issues.list"], "mode": "read"}])
        self.assertEqual(body["max_depth"], 0)
        self.assertNotIn("deny", body)

    def test_inspection_grants_read_back_as_grants(self):
        self.fake.always(
            "GET",
            f"{REPO}/branches/kid/inspection",
            200,
            {
                "name": "kid",
                "status": {"state": "ready"},
                "grants": [
                    {"connector": "github", "operations": ["issues.list"], "mode": "read"},
                    {
                        "connector": "linear",
                        "operations": ["*"],
                        "mode": "write",
                        "confirm": "allow",
                        "account": "work",
                    },
                ],
            },
        )
        inspection = self.app.inspect("kid")
        self.assertEqual([str(g) for g in inspection.grants], ["github:read:issues.list", "linear@work:write+confirm"])
        self.assertEqual(inspection.status, "ready")
        self.assertEqual(inspection.state, "ready")
        self.assertIsNone(inspection.turns)  # an absent field reads as None
        bare = bc.Inspection({"name": "x", "status": {"state": "running"}})
        self.assertEqual(bare.grants, [])

    def test_inspection_carries_the_branchs_own_reported_status(self):
        report = {"state": "blocked", "summary": "needs the API key", "link": "https://example.com/i/1", "at_ms": 1234}
        self.fake.always(
            "GET",
            f"{REPO}/branches/kid/inspection",
            200,
            {"name": "kid", "status": {"state": "running"}, "reported": report},
        )
        inspection = self.app.inspect("kid")
        # A claim beside the protocol's status, not instead of it.
        self.assertEqual(inspection.status, "running")
        reported = inspection.reported
        assert reported is not None
        self.assertIsInstance(reported, bc.StatusReport)
        self.assertEqual(reported.state, "blocked")
        self.assertEqual(reported.summary, "needs the API key")
        self.assertEqual(reported.link, "https://example.com/i/1")
        self.assertEqual(reported.at_ms, 1234)
        self.assertEqual(reported.raw, report)
        # Omitted until the branch reports one, and its link when it gave none.
        self.assertIsNone(bc.Inspection({"name": "x", "status": {"state": "running"}}).reported)
        bare = bc.Inspection({"name": "x", "reported": {"state": "working", "summary": "s", "at_ms": 1}})
        assert bare.reported is not None
        self.assertIsNone(bare.reported.link)

    def test_inspection_says_what_the_cost_limits_above_a_branch_leave_it(self):
        self.fake.always(
            "GET",
            f"{REPO}/branches/kid/inspection",
            200,
            {
                "name": "kid",
                "status": {"state": "running"},
                "max_usd": 0.5,
                "remaining_usd": 0.4,
                "subtree_cost_usd": 0.1,
                "tree_remaining_usd": 0.05,
                "tree_capped_by": "meta",
            },
        )
        inspection = self.app.inspect("kid")
        # Its own limit leaves more than the tree under its parent does: the
        # smaller figure is what it can really still spend.
        self.assertEqual(inspection.remaining_usd, 0.4)
        self.assertEqual(inspection.tree_remaining_usd, 0.05)
        self.assertEqual(inspection.tree_capped_by, "meta")
        # Omitted when no branch above it has a cost limit.
        bare = bc.Inspection({"name": "x", "status": {"state": "running"}})
        self.assertIsNone(bare.tree_remaining_usd)
        self.assertIsNone(bare.tree_capped_by)

    def test_spawn_carries_the_childs_result_contract(self):
        self.fake.answer("POST", f"{REPO}/branches/p/spawn", (202, self.op("op5", "queued", "kid", kind="spawn")))
        contract = {"schema": {"type": "object", "required": ["files"]}, "required": True}
        self.app.spawn("p", "work", name="kid", result=contract)
        body = self.requests("POST", f"{REPO}/branches/p/spawn")[0][3]
        self.assertEqual(body["result"], contract)
        # Without one it is null, which the server reads as none.
        self.fake.answer("POST", f"{REPO}/branches/p/spawn", (202, self.op("op6", "queued", "kid2", kind="spawn")))
        self.app.spawn("p", "work", name="kid2")
        self.assertIsNone(self.requests("POST", f"{REPO}/branches/p/spawn")[1][3].get("result"))

    def test_root_tasks_and_children_keep_explicit_deliverable_contracts(self):
        contract = {"schema": {"type": "object"}, "required": True, "purpose": "deliverable"}
        self.fake.answer("POST", f"{REPO}/tasks", (202, self.op("root", "queued", kind="task")))
        self.fake.answer("POST", f"{REPO}/branches/p/spawn", (202, self.op("child", "queued", kind="spawn")))
        self.app.task("Research sources", result=contract)
        self.app.spawn("p", "Compare sources", result=contract)
        self.assertEqual([request[3]["result"] for request in self.fake.requests], [contract, contract])
        self.assertEqual(
            [request[:2] for request in self.fake.requests],
            [("POST", f"{REPO}/tasks"), ("POST", f"{REPO}/branches/p/spawn")],
        )

    def test_handoff_posts_only_the_frozen_relay_request_and_preserves_its_metadata(self):
        path = f"{REPO}/branches/source/handoff"
        self.fake.answer("POST", path, (200, HANDOFF_MESSAGE), (200, HANDOFF_MESSAGE))
        expected = HANDOFF_MESSAGE["handoff"]["version"]
        sent = self.app.handoff("source", "consumer", ["art7"], "Use these sources", expected)
        self.assertIsInstance(sent, bc.Message)
        self.assertEqual(sent.handoff, HANDOFF_MESSAGE["handoff"])
        self.assertEqual(sent.handoff["version"], expected)
        self.assertEqual(sent.handoff["artifacts"][0]["digest"], "ab" * 32)
        self.assertEqual(sent.handoff["result"], {"summary": "frozen evidence", "answer": None})
        self.assertTrue(sent.next_turn)
        self.app.handoff("source", "consumer")
        self.assertEqual(len(self.fake.requests), 2, "handoff must not start, send, share or inspect implicitly")
        first, second = self.fake.requests
        self.assertEqual(first[:2], ("POST", path))
        self.assertEqual(
            first[3],
            {
                "from": "source",
                "to": "consumer",
                "artifacts": ["art7"],
                "note": "Use these sources",
                "expected": expected,
            },
        )
        self.assertEqual(len(first[2]["idempotency-key"]), 32)
        self.assertEqual(second[:2], ("POST", path))
        self.assertEqual(second[3], {"from": "source", "to": "consumer", "note": ""})

    def test_report_carries_a_result_and_keeps_an_empty_one(self):
        message = {"id": 1, "from": "kid", "to": "p", "kind": "report", "text": "done", "at_ms": 1}
        self.fake.always("POST", f"{REPO}/branches/kid/report", 200, message)
        self.app.report("kid", "plain")
        self.app.report("kid", "found", result={"files": ["a.rs"]})
        # `{}`, `[]` and `False` are results, not absent ones.
        for empty in ({}, [], False, 0):
            self.app.report("kid", "empty", result=empty)
        bodies = [r[3] for r in self.requests("POST", f"{REPO}/branches/kid/report")]
        self.assertEqual(bodies[0], {"text": "plain"})
        self.assertEqual(bodies[1], {"text": "found", "result": {"files": ["a.rs"]}})
        self.assertEqual([b["result"] for b in bodies[2:]], [{}, [], False, 0])

    def test_inspection_carries_the_contract_and_the_result(self):
        contract = {"schema": {"type": "array"}, "required": False}
        self.fake.always(
            "GET",
            f"{REPO}/branches/kid/inspection",
            200,
            {"name": "kid", "status": {"state": "ready"}, "result_contract": contract, "result": [1, 2]},
        )
        inspection = self.app.inspect("kid")
        self.assertEqual(inspection.result_contract, contract)
        self.assertEqual(inspection.result, [1, 2])
        # Omitted until there is one; a falsy result is still a result.
        bare = bc.Inspection({"name": "x", "status": {"state": "running"}})
        self.assertIsNone(bare.result_contract)
        self.assertIsNone(bare.result)
        self.assertEqual(bc.Inspection({"result": 0}).result, 0)
        self.assertEqual(bc.Inspection({"result": {}}).result, {})

    def test_a_check_is_an_operation_whose_result_is_the_report(self):
        failed = {
            "branch": "b",
            "check": ["make", "test"],
            "work": "a1b2c3d",
            "target": "by/root",
            "checked": "d4e5f6a",
            "outcome": "failed",
            "output_tail": "2 tests failed",
        }
        self.fake.answer("POST", f"{REPO}/branches/b/check", (202, self.op("op3", "queued", kind="check")))
        self.fake.answer(
            "GET",
            "/v1/operations/op3",
            (200, self.op("op3", "running", kind="check")),
            (200, self.op("op3", "succeeded", kind="check", result={"check_report": failed})),
        )
        op = self.app.check("b", key="k-9")
        self.assertEqual((op.kind, op.branch), ("check", "b"))
        self.assertIsNone(op.check_report, "nothing until the check has ended")
        ((_, _, headers, body),) = self.requests("POST", f"{REPO}/branches/b/check")
        self.assertEqual((headers["idempotency-key"], body), ("k-9", {}))
        done = self.client.wait(op, check=True)
        report = done.check_report
        assert report is not None
        self.assertIsInstance(report, bc.CheckReport)
        # A check that fails is a succeeded operation, not an exception.
        self.assertEqual((report.outcome, report.output_tail), ("failed", "2 tests failed"))
        self.assertEqual(report.check, ["make", "test"])
        self.assertFalse(report.passed)
        # `passed` is the engine's: the check passed, or there is none and nothing conflicted.
        for fields, passed in [
            ({"check": ["t"], "outcome": "passed"}, True),
            ({"check": ["t"], "outcome": "timed_out"}, False),
            ({"check": ["t"], "outcome": "not_run", "conflicts": ["a.txt"]}, False),
            ({"check": None, "outcome": "not_run"}, True),
            ({"outcome": "not_run", "conflicts": []}, True),
            ({"outcome": "not_run", "conflicts": ["a.txt"]}, False),
        ]:
            self.assertIs(bc.CheckReport({"branch": "b", "work": "w", **fields}).passed, passed, fields)
        # Without a key, one is generated.
        self.fake.answer("POST", f"{REPO}/branches/c/check", (202, self.op("op4", "queued", "c", kind="check")))
        self.app.check("c")
        ((_, _, headers, _),) = self.requests("POST", f"{REPO}/branches/c/check")
        self.assertEqual(len(headers["idempotency-key"]), 32)

    def test_a_check_is_refused_with_typed_errors(self):
        for status, body, expected in [
            (403, _error("scope_required", "needs run", {"scope": "run"}), bc.DeniedError),
            (404, _error("unknown_branch", "no branch named b"), bc.NotFoundError),
            (422, _error("idempotency_key_reused", "", {"operation": "op3"}), bc.IdempotencyKeyReusedError),
        ]:
            self.fake.answer("POST", f"{REPO}/branches/b/check", (status, body))
            with self.assertRaises(expected) as raised:
                self.app.check("b")
            self.assertEqual(type(raised.exception), expected, body)
        # An operation that could not run the check at all fails with its error.
        self.fake.answer("POST", f"{REPO}/branches/b/check", (202, self.op("op5", "queued", kind="check")))
        self.fake.answer(
            "GET",
            "/v1/operations/op5",
            (200, self.op("op5", "failed", kind="check", error={"code": "git_error", "message": "no worktree"})),
        )
        with self.assertRaises(bc.OperationFailedError) as failed:
            self.client.wait(self.app.check("b"), check=True)
        self.assertEqual(failed.exception.code, "git_error")
        self.assertIsNone(failed.exception.operation.check_report)

    def test_report_status_posts_the_report_and_reads_what_was_recorded(self):
        report = {"state": "blocked", "summary": "needs the API key", "link": "https://example.com/i/1", "at_ms": 7}
        escalation = {
            "id": 4,
            "from": "kid",
            "to": "root",
            "kind": "escalation",
            "text": "status blocked: needs the API key (https://example.com/i/1)",
            "at_ms": 7,
        }
        self.fake.answer(
            "POST",
            f"{REPO}/branches/kid/report-status",
            (200, {"report": report, "escalation": escalation}),
            (200, {"report": {"state": "working", "summary": "reading", "at_ms": 8}}),
            (
                200,
                {
                    "report": {
                        "state": "idle",
                        "summary": "waiting on CI",
                        "at_ms": 9,
                        "ttl_seconds": 600,
                        "expires_at_ms": 600_009,
                    }
                },
            ),
        )
        reported = self.app.report_status("kid", "blocked", "needs the API key", link="https://example.com/i/1")
        self.assertIsInstance(reported, bc.Reported)
        self.assertIsInstance(reported.report, bc.StatusReport)
        self.assertEqual(reported.report.raw, report)
        assert reported.escalation is not None
        self.assertIsInstance(reported.escalation, bc.Message)
        self.assertEqual((reported.escalation["from"], reported.escalation.to), ("kid", "root"))
        working = self.app.report_status("kid", "working", "reading")
        self.assertEqual(working.report.state, "working")
        self.assertIsNone(working.report.link)
        self.assertIsNone(working.escalation, "only blocked and failure reach the parent")
        self.assertFalse(working.report.stale)
        timed = self.app.report_status("kid", "idle", "waiting on CI", ttl_seconds=600)
        self.assertEqual(timed.report.expires_at_ms, 600_009)
        self.assertFalse(timed.report.stale)
        self.assertTrue(bc.StatusReport({"stale": True, "stale_because": "expired"}).stale)
        sent = [r[3] for r in self.requests("POST", f"{REPO}/branches/kid/report-status")]
        self.assertEqual(
            sent,
            [
                {"state": "blocked", "summary": "needs the API key", "link": "https://example.com/i/1"},
                {"state": "working", "summary": "reading"},
                {"state": "idle", "summary": "waiting on CI", "ttl_seconds": 600},
            ],
        )

    def test_a_refused_report_names_its_field(self):
        path = f"{REPO}/branches/kid/report-status"
        for field, message in [
            ("state", 'state must be one of working, idle, blocked, complete, failure, not "done"'),
            ("summary", "the summary is 281 characters; keep it to one line of at most 280"),
            ("link", 'the link must be an http or https URL, not "ftp://x/"'),
            ("ttl_seconds", "ttl_seconds must be a whole number of seconds from 1 to 604800 (7 days), not 0"),
        ]:
            self.fake.answer("POST", path, (400, _error("invalid_request", message, {"field": field})))
            with self.assertRaises(bc.InvalidRequestError) as raised:
                self.app.report_status("kid", "done", "x")
            self.assertEqual(raised.exception.field, field)
            self.assertEqual(raised.exception.message, message)
            self.assertEqual(raised.exception.status, 400)
        # A refusal that names no field has none.
        self.fake.answer("POST", path, (400, _error("invalid_request", "invalid request body: missing field")))
        with self.assertRaises(bc.InvalidRequestError) as raised:
            self.app.report_status("kid", "working", "x")
        self.assertIsNone(raised.exception.field)
        for status, body, expected in [
            (403, _error("scope_required", "", {"scope": "run"}), bc.DeniedError),
            (404, _error("unknown_branch", "no branch named kid"), bc.NotFoundError),
        ]:
            self.fake.answer("POST", path, (status, body))
            with self.assertRaises(expected):
                self.app.report_status("kid", "working", "x")

    def test_the_health_check_sends_no_token(self):
        self.assertTrue(self.client.healthy())
        ((_, _, headers, _),) = self.requests("GET", "/healthz")
        self.assertNotIn("authorization", headers)


class Artifacts(ServerCase):
    def test_preview_metadata_keeps_authentication_options_and_encoded_identifiers(self):
        path = f"{REPO}/branches/review%20%2F%20draft/artifacts/a%2Fone/preview"
        answer = {"status": "passed", "kind": "table", "sheet": "Costs & risks", "rows": [["item", 42]]}
        self.fake.answer("GET", path, (200, answer))
        preview = self.app.preview_artifact(
            "review / draft", "a/one", view="table", sheet="Costs & risks", offset=0, limit=25
        )
        self.assertIsInstance(preview, bc.Record)
        self.assertEqual(preview.raw, answer)
        ((_, target, headers, body),) = self.requests("GET", path)
        self.assertEqual(
            parse_qs(urlsplit(target).query),
            {"view": ["table"], "sheet": ["Costs & risks"], "offset": ["0"], "limit": ["25"]},
        )
        self.assertEqual(headers["authorization"], f"Bearer {TOKEN}")
        self.assertEqual(body, b"")

    def test_preview_assets_are_authenticated_raw_bytes_with_matching_options(self):
        path = f"{REPO}/branches/b/artifacts/a1/preview/assets/page%203.png"
        image = b"\x89PNG\r\n\x1a\n\x00\xff"
        self.fake.answer("GET", path, (200, image))
        self.assertEqual(self.app.read_preview_asset("b", "a1", "page 3.png", view="pages", page=3), image)
        ((_, target, headers, _),) = self.requests("GET", path)
        self.assertEqual(parse_qs(urlsplit(target).query), {"view": ["pages"], "page": ["3"]})
        self.assertEqual(headers["authorization"], f"Bearer {TOKEN}")

    def test_preview_errors_preserve_server_codes_and_never_fetch_an_asset_url(self):
        path = f"{REPO}/branches/b/artifacts/a1/preview"
        self.fake.answer("GET", path, (403, _error("scope_required", "read scope required")))
        with self.assertRaises(bc.DeniedError) as denied:
            self.app.preview_artifact("b", "a1")
        self.assertEqual(denied.exception.code, "scope_required")
        asset_path = f"{path}/assets/https%3A%2F%2Fother.invalid%2Fimage.png"
        self.fake.answer("GET", asset_path, (404, _error("unknown_preview_asset")))
        with self.assertRaises(bc.NotFoundError):
            self.app.read_preview_asset("b", "a1", "https://other.invalid/image.png")
        self.assertEqual(len(self.requests("GET", asset_path)), 1)

    def test_publish_sends_the_query_names_the_server_reads(self):
        self.fake.answer("POST", f"{REPO}/branches/b/artifacts", (201, {"id": "a1"}), (201, {"id": "a2"}))
        self.app.publish_artifact("b", b"data", "x.txt", "text/plain", labels={"team": "core", "run": "7"})
        self.app.publish_artifact("b", b"more", "y.csv", kind="text/csv")  # `kind`, the old name of media_type
        first, second = self.requests("POST", f"{REPO}/branches/b/artifacts")
        query = parse_qs(urlsplit(first[1]).query)
        self.assertEqual(query, {"name": ["x.txt"], "media_type": ["text/plain"], "label": ["team=core", "run=7"]})
        self.assertEqual(first[3], b"data")
        self.assertEqual(parse_qs(urlsplit(second[1]).query), {"name": ["y.csv"], "media_type": ["text/csv"]})
        self.assertEqual(second[2]["content-type"], "application/octet-stream")


class WorkspaceRequests(unittest.TestCase):
    """Exact wire requests against the existing transport boundary; no server."""

    def setUp(self) -> None:
        self.client = bc.Client("http://127.0.0.1:9", TOKEN)

    def assert_request(self, call: Callable[[], Any], answer: Any, *args: Any, **options: Any) -> Any:
        response = dict(answer) if isinstance(answer, dict) else answer
        with mock.patch.object(self.client, "request", return_value=response) as stub:
            result = call()
            stub.assert_called_once_with(*args, **options)
        return result

    @staticmethod
    def operation() -> dict[str, Any]:
        return {
            "id": "op1",
            "repo": "managed",
            "kind": "workspace",
            "state": "queued",
            "branches": ["s1"],
            "cursor": 0,
            "created_at_ms": 1,
            "workspace_id": "w1",
            "session_id": "s1",
        }

    def test_project_creation_and_lists_use_source_grants_without_host_paths(self):
        c = self.client
        project = {"id": "p1", "name": "Research", "source": {"kind": "knowledge"}, "result_mode": "artifacts"}
        result = self.assert_request(
            lambda: c.create_project("Research", key="create-project"),
            project,
            "POST",
            "/v1/projects",
            {"name": "Research", "source": {"kind": "knowledge"}, "result_mode": "artifacts"},
            key="create-project",
            query=None,
        )
        self.assertIsInstance(result, bc.Record)
        self.assertEqual(result.raw, project)
        source = {"kind": "repository", "repo": "registered-repo"}
        self.assert_request(
            lambda: c.create_project("Code", source=source, result_mode="mixed", key="repo-project"),
            project,
            "POST",
            "/v1/projects",
            {"name": "Code", "source": source, "result_mode": "mixed"},
            key="repo-project",
            query=None,
        )
        result = self.assert_request(c.projects, {"projects": [project]}, "GET", "/v1/projects", query=None)
        self.assertEqual(result, [project])
        self.assertIsInstance(result[0], bc.Record)
        self.assert_request(lambda: c.project("p /one"), project, "GET", "/v1/projects/p%20%2Fone", query=None)
        workspace = {"id": "w1", "revision": 0, "sources": [], "accepted_deliverables": []}
        self.assert_request(
            lambda: c.workspaces("p & one"),
            {"workspaces": [workspace]},
            "GET",
            "/v1/workspaces",
            query={"project": "p & one"},
        )
        result = self.assert_request(
            lambda: c.workspace("w /one"), workspace, "GET", "/v1/workspaces/w%20%2Fone", query=None
        )
        self.assertEqual(result.revision, 0)
        created = self.assert_request(
            lambda: c.create_workspace("p1", "Brief", key="reserve"),
            self.operation(),
            "POST",
            "/v1/workspaces",
            {"project_id": "p1", "title": "Brief"},
            key="reserve",
            query=None,
        )
        self.assertIsInstance(created, bc.Operation)
        self.assertEqual((created.id, created.workspace_id, created.session_id), ("op1", "w1", "s1"))

    def test_session_admission_preserves_explicit_inputs_and_result_schema_nulls(self):
        c = self.client
        session = {"id": "s1", "workspace_id": "w1", "native_session": "native-1", "result": None, "result_event": 7}
        result = self.assert_request(
            lambda: c.workspace_sessions("w1"), {"sessions": [session]}, "GET", "/v1/workspaces/w1/sessions", query=None
        )
        self.assertIn("result", result[0])
        self.assertIsNone(result[0].result)
        contract = {"schema": {"const": None}, "required": True}
        policy = {"preset": "read-only"}
        provision = {"secrets": [{"name": "OPERATOR_KNOWN"}]}
        provider = {"kind": "microsandbox", "image": "fixture:1", "keep": "pause"}
        body = {
            "name": "Review",
            "prompt": "Check evidence",
            "harness": "codex",
            "model": "configured-model",
            "effort": "high",
            "policy": policy,
            "provision": provision,
            "provider": provider,
            "unapproved_tools": True,
            "result_contract": contract,
            "access": "reviewer",
            "sources": [],
            "artifacts": ["a1"],
            "candidate": "immutable-commit",
        }
        admitted = self.assert_request(
            lambda: c.start_workspace_session(
                "w1",
                "Review",
                "Check evidence",
                harness="codex",
                model="configured-model",
                effort="high",
                policy=policy,
                provision=provision,
                provider=provider,
                unapproved_tools=True,
                result_contract=contract,
                access="reviewer",
                sources=[],
                artifacts=["a1"],
                candidate="immutable-commit",
                key="start",
            ),
            self.operation(),
            "POST",
            "/v1/workspaces/w1/sessions",
            body,
            key="start",
            query=None,
        )
        self.assertEqual(admitted.workspace_id, "w1")
        self.assert_request(
            lambda: c.start_workspace_session("w1", "Writer", "Draft", harness="agy", key="minimal"),
            self.operation(),
            "POST",
            "/v1/workspaces/w1/sessions",
            {"name": "Writer", "prompt": "Draft", "harness": "agy", "access": "writer"},
            key="minimal",
            query=None,
        )
        self.assert_request(
            lambda: c.start_workspace_session(
                "w1", "Writer", "Draft", harness="agy", unapproved_tools=False, key="no-ack"
            ),
            self.operation(),
            "POST",
            "/v1/workspaces/w1/sessions",
            {"name": "Writer", "prompt": "Draft", "harness": "agy", "access": "writer", "unapproved_tools": False},
            key="no-ack",
            query=None,
        )

    def test_execution_presets_are_discovered_and_named_on_session_start(self):
        c = self.client
        offered = {"required": True, "presets": [{"id": "substrate-gke", "can_start": True}]}
        listed = self.assert_request(
            lambda: c.execution_presets("w /1"),
            offered,
            "GET",
            "/v1/execution-presets",
            query={"workspace": "w /1"},
        )
        self.assertTrue(listed.required)
        self.assertEqual(listed.presets[0]["id"], "substrate-gke")
        self.assert_request(
            c.execution_presets,
            {"required": False, "presets": []},
            "GET",
            "/v1/execution-presets",
            query={"workspace": None},
        )
        self.assert_request(
            lambda: c.start_workspace_session(
                "w1", "Analyst", "Summarise", harness="antigravity", preset="substrate-gke", key="preset"
            ),
            self.operation(),
            "POST",
            "/v1/workspaces/w1/sessions",
            {
                "name": "Analyst",
                "prompt": "Summarise",
                "harness": "antigravity",
                "preset": "substrate-gke",
                "access": "writer",
            },
            key="preset",
            query=None,
        )

    def test_native_messages_cancellation_lifecycle_and_acceptance_keep_their_contracts(self):
        c = self.client
        page = {"messages": [{"id": "s1:4", "role": "assistant", "text": "Evidence", "time": 12}], "next_cursor": "5"}
        result = self.assert_request(
            lambda: c.workspace_messages("w1", "s1", "2+3/4"),
            page,
            "GET",
            "/v1/workspaces/w1/sessions/s1/messages",
            query={"after": "2+3/4"},
        )
        self.assertEqual(result.next_cursor, "5")
        self.assertEqual(result.messages[0]["text"], "Evidence")
        for action in [None, "send", "steer", "resume"]:
            body = {"prompt": "Continue"}
            if action is not None:
                body["action"] = action
            self.assert_request(
                partial(c.send_workspace_message, "w1", "s1", "Continue", action=action, key="send"),
                self.operation(),
                "POST",
                "/v1/workspaces/w1/sessions/s1/messages",
                body,
                key="send",
                query=None,
            )
        self.assert_request(
            lambda: c.cancel_workspace_session("w1", "s1", key="cancel"),
            self.operation(),
            "POST",
            "/v1/workspaces/w1/sessions/s1/cancel",
            {},
            key="cancel",
            query=None,
        )
        self.assert_request(
            lambda: c.workspace_action("w1", "archive", revision=4, key="archive"),
            self.operation(),
            "POST",
            "/v1/workspaces/w1/actions/archive",
            {"revision": 4},
            key="archive",
            query=None,
        )
        accepted = {"workspace_id": "w1", "revision": 5, "artifacts": ["deck"], "code_integration": "independent"}
        result = self.assert_request(
            lambda: c.finalize_workspace("w1", ["deck"], revision=4, key="accept"),
            accepted,
            "POST",
            "/v1/workspaces/w1/finalize",
            {"revision": 4, "artifacts": ["deck"]},
            key="accept",
            query=None,
        )
        self.assertEqual(result.code_integration, "independent")

    def test_collaboration_sends_revisions_but_never_client_authorship(self):
        c = self.client
        member = {"workspace_id": "w1", "subject": "verified/subject", "role": "reviewer"}
        self.assert_request(
            lambda: c.workspace_members("w1"), {"members": [member]}, "GET", "/v1/workspaces/w1/members", query=None
        )
        self.assert_request(
            lambda: c.workspace_member("w1", "verified/subject"),
            member,
            "GET",
            "/v1/workspaces/w1/members/verified%2Fsubject",
            query=None,
        )
        self.assert_request(
            lambda: c.set_workspace_member("w1", "verified/subject", "reviewer", revision=0, key="member"),
            member,
            "POST",
            "/v1/workspaces/w1/members/verified%2Fsubject",
            {"revision": 0, "role": "reviewer"},
            key="member",
            query=None,
        )
        self.assert_request(
            lambda: c.remove_workspace_member("w1", "verified/subject", revision=1, key="remove"),
            {"removed": True},
            "DELETE",
            "/v1/workspaces/w1/members/verified%2Fsubject",
            {"revision": 1},
            key="remove",
        )
        comment = {"id": "c1", "subject": "verified/subject", "credential": "paired-device", "text": "Check total"}
        self.assert_request(
            lambda: c.workspace_comments("w1"), {"comments": [comment]}, "GET", "/v1/workspaces/w1/comments", query=None
        )
        result = self.assert_request(
            lambda: c.add_workspace_comment("w1", "Check total", revision=2, key="comment"),
            comment,
            "POST",
            "/v1/workspaces/w1/comments",
            {"revision": 2, "text": "Check total"},
            key="comment",
            query=None,
        )
        self.assertEqual(result.subject, "verified/subject")
        self.assert_request(
            lambda: c.workspace_presence("w1"),
            {"presence": [{"subject": "verified/subject", "expires_at_ms": 99}]},
            "GET",
            "/v1/workspaces/w1/presence",
            query=None,
        )
        self.assert_request(
            lambda: c.touch_workspace_presence("w1", ttl_seconds=120, key="presence"),
            {"expires_at_ms": 99},
            "POST",
            "/v1/workspaces/w1/presence",
            {"ttl_seconds": 120},
            key="presence",
            query=None,
        )

    def test_sources_and_artifacts_keep_original_bytes_and_upload_headers(self):
        c = self.client
        original = b"\x00original\xff"
        source = {"id": "src1", "digest": "abc", "size_bytes": len(original)}
        self.assert_request(
            lambda: c.workspace_sources("w1"), {"sources": [source]}, "GET", "/v1/workspaces/w1/sources", query=None
        )
        result = self.assert_request(
            lambda: c.upload_workspace_source(
                "w1", original, "Quarterly source", revision=7, media_type="text/csv", key="upload"
            ),
            source,
            "POST",
            "/v1/workspaces/w1/sources",
            original,
            key="upload",
            headers={"X-Source-Title": "Quarterly source", "X-Workspace-Revision": "7", "Content-Type": "text/csv"},
        )
        self.assertEqual(result.digest, "abc")
        self.assertEqual(
            self.assert_request(
                lambda: c.read_workspace_source("w /one", "src/one"),
                (200, {}, original),
                "GET",
                "/v1/workspaces/w%20%2Fone/sources/src%2Fone/content",
                raw=True,
            ),
            original,
        )
        artifact = {"id": "a1", "name": "Report.docx", "publisher_branch": "s1", "turn": 2}
        self.assert_request(
            lambda: c.workspace_artifacts("w1"),
            {"artifacts": [artifact]},
            "GET",
            "/v1/workspaces/w1/artifacts",
            query=None,
        )
        self.assertEqual(
            self.assert_request(
                lambda: c.read_workspace_artifact("w1", "a/one"),
                (200, {}, original),
                "GET",
                "/v1/workspaces/w1/artifacts/a%2Fone/content",
                raw=True,
            ),
            original,
        )

    def test_workspace_preview_aliases_preserve_options_and_raw_asset_bytes(self):
        c = self.client
        query = {"view": "pages", "page": 3, "sheet": None, "offset": None, "limit": None}
        manifest = {"status": "passed", "kind": "pages", "image": "page-3.png", "page": 3}
        for kind, preview, asset in [
            ("sources", c.preview_workspace_source, c.read_workspace_source_preview_asset),
            ("artifacts", c.preview_workspace_artifact, c.read_workspace_artifact_preview_asset),
        ]:
            path = f"/v1/workspaces/w%2Fone/{kind}/a%2Fone/preview"
            result = self.assert_request(
                partial(preview, "w/one", "a/one", view="pages", page=3), manifest, "GET", path, query=query
            )
            self.assertEqual(result.image, "page-3.png")
            image = b"\x89PNG\x00\xff"
            result = self.assert_request(
                partial(asset, "w/one", "a/one", "page-3.png", view="pages", page=3),
                (200, {}, image),
                "GET",
                f"{path}/assets/page-3.png",
                query=query,
                raw=True,
            )
            self.assertEqual(result, image)
        query = {"view": "table", "page": None, "sheet": "Costs & risks", "offset": 0, "limit": 50}
        self.assert_request(
            lambda: c.preview_workspace_artifact("w1", "a1", view="table", sheet="Costs & risks", offset=0, limit=50),
            {"kind": "table", "rows": []},
            "GET",
            "/v1/workspaces/w1/artifacts/a1/preview",
            query=query,
        )

    def test_runtime_terminal_and_forge_operations_use_only_the_bound_session(self):
        c = self.client
        prefix = "/v1/workspaces/w%2Fone/sessions/s%2Fone"
        self.assert_request(
            lambda: c.workspace_runtime("w/one", "s/one"),
            {"state": "unavailable", "terminal_available": False},
            "GET",
            f"{prefix}/runtime",
            query=None,
        )
        self.assert_request(
            lambda: c.control_workspace_runtime("w/one", "s/one", "resume", key="wake"),
            self.operation(),
            "POST",
            f"{prefix}/runtime/actions/resume",
            {},
            key="wake",
            query=None,
        )
        handle = self.assert_request(
            lambda: c.open_workspace_terminal("w/one", "s/one", rows=37, columns=91, key="terminal"),
            {"id": "t/one", "persistent": False},
            "POST",
            f"{prefix}/terminals",
            {"rows": 37, "columns": 91},
            key="terminal",
            query=None,
        )
        self.assertFalse(handle.persistent)
        terminal = f"{prefix}/terminals/t%2Fone"
        data = b"\x1b[32mhello\xff"
        page = {
            "cursor": len(data),
            "data": base64.b64encode(data).decode("ascii"),
            "encoding": "base64",
            "truncated": False,
            "closed": False,
            "error": None,
        }
        result = self.assert_request(
            lambda: c.workspace_terminal_output("w/one", "s/one", "t/one", after=0, limit=64),
            page,
            "GET",
            f"{terminal}/output",
            query={"after": 0, "limit": 64},
        )
        self.assertIsInstance(result, bc.TerminalOutput)
        self.assertEqual(result.data_bytes, data)
        self.assert_request(
            lambda: c.write_workspace_terminal("w/one", "s/one", "t/one", b"echo hello\n", sequence=1),
            {"sequence": 1, "next_sequence": 2},
            "POST",
            f"{terminal}/input",
            {"sequence": 1, "data": "ZWNobyBoZWxsbwo="},
            key=None,
            query=None,
        )
        self.assert_request(
            lambda: c.resize_workspace_terminal("w/one", "s/one", "t/one", 24, 80),
            {"ok": True},
            "POST",
            f"{terminal}/resize",
            {"rows": 24, "columns": 80},
            key=None,
            query=None,
        )
        self.assert_request(
            lambda: c.close_workspace_terminal("w/one", "s/one", "t/one"),
            {"ok": True},
            "POST",
            f"{terminal}/close",
            {},
            key=None,
            query=None,
        )
        snapshot = {"branch": "s1", "candidate": "commit-1", "pull_request": None, "forge": {"available": False}}
        self.assert_request(
            lambda: c.workspace_pull_request("w/one", "s/one"), snapshot, "GET", f"{prefix}/pr", query=None
        )
        self.assert_request(
            lambda: c.publish_workspace_pull_request("w/one", "s/one", title="Review", base="main", key="publish"),
            self.operation(),
            "POST",
            f"{prefix}/pr",
            {"title": "Review", "base": "main", "draft": False},
            key="publish",
            query=None,
        )
        self.assert_request(
            lambda: c.refresh_workspace_pull_request("w/one", "s/one", key="refresh"),
            self.operation(),
            "POST",
            f"{prefix}/pr/refresh",
            {},
            key="refresh",
            query=None,
        )

    def test_lost_or_denied_terminals_are_never_reopened_or_replayed(self):
        for code in ["terminal_lost", "terminal_input_uncertain", "denied"]:
            error = bc.error_for(410 if code == "terminal_lost" else 403, _error(code))
            with mock.patch.object(self.client, "request", side_effect=error) as stub:
                with self.assertRaises(bc.BranchyardError) as raised:
                    self.client.write_workspace_terminal("w1", "s1", "t1", b"important write\n", sequence=2)
                self.assertEqual(raised.exception.code, code)
                self.assertEqual(stub.call_count, 1)
                self.assertTrue(stub.call_args.args[1].endswith("/terminals/t1/input"))

    def test_queued_metadata_replay_and_generated_keys_remain_available(self):
        answer = {**self.operation(), "_replayed": True}
        with mock.patch.object(bc, "new_key", return_value="generated-key"):
            operation = self.assert_request(
                lambda: self.client.create_workspace("p1", "Brief"),
                answer,
                "POST",
                "/v1/workspaces",
                {"project_id": "p1", "title": "Brief"},
                key="generated-key",
                query=None,
            )
        self.assertTrue(operation.replayed)
        self.assertEqual(operation.workspace_id, "w1")
        self.assertNotIn("_replayed", operation.raw)
        self.assertRaises(
            ValueError, getattr, bc.TerminalOutput({"encoding": "utf8", "data": "not base64"}), "data_bytes"
        )
        self.assertRaises(ValueError, getattr, bc.TerminalOutput({"encoding": "base64", "data": "!bad"}), "data_bytes")


class WorkspaceTransport(ServerCase):
    def test_source_upload_download_and_revision_delete_use_authenticated_http(self):
        source = "/v1/workspaces/w1/sources"
        original = b"name,value\noriginal,\xff\n"
        self.fake.answer("POST", source, (200, {"id": "src1", "size_bytes": len(original)}))
        self.client.upload_workspace_source(
            "w1", original, "Input.csv", revision=3, media_type="text/csv", key="source-once"
        )
        ((_, _, headers, body),) = self.requests("POST", source)
        self.assertEqual(body, original)
        self.assertEqual(headers["authorization"], f"Bearer {TOKEN}")
        self.assertEqual(headers["x-source-title"], "Input.csv")
        self.assertEqual(headers["x-workspace-revision"], "3")
        self.assertEqual(headers["content-type"], "text/csv")
        self.assertEqual(headers["idempotency-key"], "source-once")
        self.fake.answer("GET", f"{source}/src1/content", (200, original))
        self.assertEqual(self.client.read_workspace_source("w1", "src1"), original)
        membership = "/v1/workspaces/w1/members/verified%2Fsubject"
        self.fake.answer("DELETE", membership, (200, {"removed": True}))
        self.client.remove_workspace_member("w1", "verified/subject", revision=4, key="remove-once")
        ((_, _, headers, body),) = self.requests("DELETE", membership)
        self.assertEqual(body, {"revision": 4})
        self.assertEqual(headers["authorization"], f"Bearer {TOKEN}")
        self.assertEqual(headers["idempotency-key"], "remove-once")


class Records(unittest.TestCase):
    def test_handoff_messages_keep_the_payload_while_legacy_messages_keep_their_defaults(self):
        handed = bc.Message(HANDOFF_MESSAGE)
        self.assertIsInstance(handed.handoff, bc.Record)
        self.assertEqual(handed.handoff, HANDOFF_MESSAGE["handoff"])
        self.assertTrue(handed.next_turn)
        old = bc.Message({"id": 1, "from": "kid", "to": "lead", "kind": "report", "text": "done", "at_ms": 1})
        self.assertIsNone(old.handoff)
        self.assertFalse(old.next_turn)
        self.assertEqual(old.text, "done")

    def test_a_missing_key_reads_none_and_dir_lists_the_keys_a_record_holds(self):
        op = bc.Operation({"id": "op1", "state": "running", "not-a-name": 1, 7: "x"})
        self.assertIsNone(op.stat)  # a typo reads None: documented, and what dir() is for
        names = dir(op)
        self.assertEqual([n for n in names if n.startswith("sta")], ["state"])
        self.assertIn("id", names)
        self.assertIn("done", names)  # its properties are still there
        self.assertIn("raw", names)
        self.assertNotIn("not-a-name", names)
        self.assertNotIn("stat", names)
        self.assertEqual(op.raw, dict(op))
        with self.assertRaises(AttributeError):
            op.__nope__  # noqa: B018

    def test_every_public_name_the_module_exports_is_documented(self):
        undocumented = []
        for name in bc.__all__:
            item = getattr(bc, name)
            if not (item.__doc__ or "").strip():
                undocumented.append(name)
            if not isinstance(item, type):
                continue
            for attr, value in vars(item).items():
                if attr.startswith("_"):
                    continue
                function = value.fget if isinstance(value, property) else value
                if callable(function) and not (function.__doc__ or "").strip():
                    undocumented.append(f"{name}.{attr}")
        # `BranchyardError.kind` has its docstring on the getter and none on the setter.
        self.assertEqual(undocumented, [])


class Errors(ServerCase):
    def test_errors_map_to_exceptions_by_their_stable_code(self):
        cases = [
            (401, _error("unauthorized"), bc.UnauthorizedError),
            (404, _error("unknown_branch", "no branch b"), bc.NotFoundError),
            (409, _error("branch_busy", "busy", {"holder": "op9"}), bc.BranchBusyError),
            (409, _error("running"), bc.RunningError),
            (409, _error("branch_exists"), bc.ConflictError),
            (403, _error("connectors_not_configured"), bc.NotAllowedError),
            (403, _error("delegation_not_allowed"), bc.NotAllowedError),
            (403, _error("denied"), bc.DeniedError),
            (403, _error("scope_required", "", {"scope": "run"}), bc.DeniedError),
            (400, _error("invalid_request"), bc.InvalidRequestError),
            (422, _error("check_failed", "", {"output_tail": "boom"}), bc.CheckFailedError),
            (422, _error("idempotency_key_reused", "", {"operation": "op1"}), bc.IdempotencyKeyReusedError),
            (500, _error("internal"), bc.ServerError),
            (503, _error("shutting_down"), bc.ServerError),
            (418, _error("teapot"), bc.BranchyardError),
        ]
        for status, body, expected in cases:
            self.fake.answer("GET", f"{REPO}/branches/b", (status, body))
            with self.assertRaises(expected) as raised:
                self.app.branch("b")
            error = raised.exception
            self.assertEqual(error.code, body["error"]["code"])
            self.assertEqual(error.status, status)
            self.assertEqual(type(error), expected, body)
        # Details ride along.
        self.fake.answer("POST", f"{REPO}/branches/b/send", (409, _error("branch_busy", "busy", {"holder": "op9"})))
        with self.assertRaises(bc.BranchBusyError) as busy:
            self.app.send("b", "x")
        self.assertEqual(busy.exception.holder, "op9")
        self.assertEqual(str(busy.exception), "branch_busy: busy")

    def test_rate_limits_carry_retry_after(self):
        self.fake.answer(
            "GET", f"{REPO}/branches/b", (429, (_error("quota_exceeded", "", {"max": 2}), {"Retry-After": "7"}))
        )
        with self.assertRaises(bc.QuotaExceededError) as raised:
            self.app.branch("b")
        self.assertEqual(raised.exception.retry_after, 7.0)
        self.assertEqual(raised.exception.detail, {"max": 2})

    def test_a_body_that_is_not_the_api_is_a_transport_error(self):
        self.fake.answer("GET", f"{REPO}/branches/b", (502, b"<html>bad gateway</html>"))
        with self.assertRaises(bc.TransportError):
            self.app.branch("b")
        self.stop()
        with self.assertRaises(bc.TransportError):
            self.app.branch("b")

    def test_exists_distinguishes_an_unknown_branch_from_other_failures(self):
        self.fake.answer(
            "GET", f"{REPO}/branches/b", (404, _error("unknown_branch")), (403, _error("repo_not_allowed"))
        )
        self.assertFalse(self.app.exists("b"))
        with self.assertRaises(bc.DeniedError):
            self.app.exists("b")

    def test_a_refused_connection_says_what_to_start(self):
        # What urllib raises when nothing listens at the address; no port is freed to get it.
        refused = urllib.error.URLError(ConnectionRefusedError(111, "Connection refused"))
        with mock.patch("urllib.request.urlopen", side_effect=refused):
            with self.assertRaises(bc.TransportError) as raised:
                self.app.branches()
            message = raised.exception.message
            self.assertIn(f"GET {self.url}/v1/repos/app/branches", message)
            self.assertIn("Nothing is listening there. to fix: start a server with `by serve --repo app=.`", message)
            self.assertIn('Client.local(".")', message)
            # The stream says the same when it cannot connect.
            with self.assertRaises(bc.TransportError) as streamed:
                for _ in self.app.stream(0, read_timeout=1.0, max_failures=0):
                    pass
            self.assertIn("to fix: start a server", streamed.exception.message)
        # Other failures to connect keep their own words, with no advice that does not apply.
        timed_out = urllib.error.URLError(TimeoutError("timed out"))
        with mock.patch("urllib.request.urlopen", side_effect=timed_out), self.assertRaises(bc.TransportError) as other:
            self.app.branches()
        self.assertNotIn("to fix", other.exception.message)

    def test_an_unknown_repository_names_the_fix(self):
        self.fake.answer("GET", "/v1/repos/nope/branches", (404, _error("unknown_repo", "no repository named nope")))
        with self.assertRaises(bc.NotFoundError) as raised:
            self.client.repo("nope").branches()
        self.assertEqual(raised.exception.code, "unknown_repo")
        self.assertEqual(
            raised.exception.message,
            "no repository named nope. to fix: name a served repository (`client.repos()` lists them); "
            "`by serve --repo NAME=.` serves one",
        )
        # A server that says which it serves, or says the fix itself, is not told twice.
        served = _error("unknown_repo", "no repository named nope", {"served": ["a", "b"]})
        self.fake.answer("GET", "/v1/repos/nope/branches", (404, served))
        with self.assertRaises(bc.NotFoundError) as raised:
            self.client.repo("nope").branches()
        self.assertIn("This server serves: a, b. to fix:", raised.exception.message)
        said = _error("unknown_repo", "no repository named nope. to fix: use `by serve --repo nope=.`")
        self.fake.answer("GET", "/v1/repos/nope/branches", (404, said))
        with self.assertRaises(bc.NotFoundError) as raised:
            self.client.repo("nope").branches()
        self.assertEqual(raised.exception.message, "no repository named nope. to fix: use `by serve --repo nope=.`")

    def test_the_client_refuses_a_bad_url_or_no_token(self):
        with self.assertRaises(ValueError):
            bc.Client("ftp://x", "t")
        with self.assertRaises(ValueError):
            bc.Client("http://x")
        with self.assertRaises(ValueError):
            bc.Client("http://x", "t", ca_file="/dev/null")


class Waiting(ServerCase):
    def test_wait_long_polls_until_done_and_checks_the_outcome(self):
        self.fake.answer(
            "GET",
            "/v1/operations/op1",
            (200, self.op("op1", "queued")),
            (200, self.op("op1", "running")),
            (
                200,
                self.op("op1", "succeeded", end=4, result={"branches": [{"name": "b", "status": {"state": "ready"}}]}),
            ),
        )
        started = time.monotonic()
        op = self.client.wait("op1")
        self.assertTrue(op.done)
        self.assertEqual(op.result_branches[0].state, "ready")
        self.assertTrue(op.result_branches[0].settled)
        # Each ask is a long poll the server holds, at most its 30 s cap;
        # nothing sleeps between them.
        asks = self.requests("GET", "/v1/operations/op1")
        self.assertEqual(len(asks), 3)
        waits = [float(parse_qs(urlsplit(r[1]).query)["wait"][0]) for r in asks]
        self.assertEqual(waits, [30.0, 30.0, 30.0])
        self.assertLess(time.monotonic() - started, 5)
        self.fake.answer(
            "GET",
            "/v1/operations/op2",
            (200, self.op("op2", "failed", error={"code": "unknown_harness", "message": "no such harness x"})),
        )
        with self.assertRaises(bc.OperationFailedError) as raised:
            self.client.wait("op2", check=True)
        self.assertEqual(raised.exception.code, "unknown_harness")
        self.assertEqual(raised.exception.operation.id, "op2")
        # A finished operation given back is not fetched again.
        finished = bc.Operation(self.op("op3", "succeeded"))
        self.assertIs(self.client.wait(finished), finished)
        self.assertEqual(self.requests("GET", "/v1/operations/op3"), [])

    def branch_ended(self, id: str, status: dict, name: str = "b", **extra: Any) -> dict:
        """A succeeded operation whose one result branch has `status`: the operation is
        the request, and the branch is the harness's turn."""
        return self.op(id, "succeeded", name, end=4, result={"branches": [{"name": name, "status": status}]}, **extra)

    def test_check_raises_when_the_branch_the_operation_made_ended_failed(self):
        # The run failed ("no handshake within 120s"); the operation still succeeded.
        failed = self.branch_ended("op1", {"state": "failed", "reason": "no handshake within 120s"}, "fix-parser")
        self.fake.answer("GET", "/v1/operations/op1", (200, failed), (200, failed))
        with self.assertRaises(bc.BranchFailedError) as raised:
            self.client.wait("op1", check=True)
        error = raised.exception
        self.assertIsInstance(error, bc.OperationFailedError)
        self.assertEqual((error.branch, error.state, error.code), ("fix-parser", "failed", "failed"))
        self.assertEqual(error.reason, "no handshake within 120s")
        self.assertIsNone(error.limit)
        self.assertEqual(error.branch_info.status, "failed")
        self.assertEqual(error.branch_status, {"state": "failed", "reason": "no handshake within 120s"})
        self.assertEqual(error.branch_status, "failed")  # a Status compares equal to its state's name
        self.assertEqual(error.operation.id, "op1")
        self.assertEqual(error.detail["branch"], "fix-parser")
        self.assertEqual(error.detail["status"], {"state": "failed", "reason": "no handshake within 120s"})
        self.assertIn("branch fix-parser ended failed: no handshake within 120s", error.message)
        self.assertIn("to fix: ", error.message)
        self.assertIn("by log fix-parser", error.message)
        # Without `check` the operation comes back, and says which branch to look at.
        op = self.client.wait("op1")
        self.assertEqual([b.name for b in op.failed_branches], ["fix-parser"])

    def test_check_raises_for_a_branch_over_its_budget_or_interrupted(self):
        over = self.branch_ended("op1", {"state": "budget_exceeded", "limit": "max_usd"})
        cut = self.branch_ended("op2", {"state": "interrupted"})
        self.fake.answer("GET", "/v1/operations/op1", (200, over))
        self.fake.answer("GET", "/v1/operations/op2", (200, cut))
        with self.assertRaises(bc.BranchFailedError) as raised:
            self.client.wait("op1", check=True)
        self.assertEqual((raised.exception.code, raised.exception.limit), ("budget_exceeded", "max_usd"))
        self.assertIsNone(raised.exception.reason)
        self.assertIn("ended budget_exceeded: max_usd", raised.exception.message)
        self.assertIn("budget_usd", raised.exception.message)
        with self.assertRaises(bc.BranchFailedError) as raised:
            self.client.wait(bc.Operation(cut), check=True)
        error = raised.exception
        self.assertEqual((error.code, error.reason, error.limit), ("interrupted", None, None))
        self.assertIn("to fix: ", raised.exception.message)

    def test_check_passes_a_branch_that_finished_its_work(self):
        for state in ("ready", "no_changes", "merged", "awaiting_plan_approval", "waiting_on_children"):
            done = bc.Operation(self.branch_ended("op1", {"state": state}))
            self.assertIs(self.client.wait(done, check=True), done, state)
            self.assertEqual(done.failed_branches, [])
        # An operation with no branches in its result has none to fail.
        bare = bc.Operation(self.op("op2", "succeeded"))
        self.assertIs(self.client.wait(bare, check=True), bare)

    def test_check_names_every_branch_of_a_map_that_ended_badly(self):
        branches = [
            {"name": "m-a", "status": {"state": "ready"}},
            {"name": "m-b", "status": {"state": "failed", "reason": "boom"}},
            {"name": "m-c", "status": {"state": "budget_exceeded", "limit": "max_turns"}},
        ]
        mapped = bc.Operation(self.op("op1", "succeeded", "m-a", kind="map", result={"branches": branches}))
        with self.assertRaises(bc.BranchFailedError) as raised:
            self.client.wait(mapped, check=True)
        self.assertEqual(raised.exception.branch, "m-b")
        self.assertEqual([b.name for b in raised.exception.branches], ["m-b", "m-c"])
        self.assertIn("(and 1 more: m-c)", raised.exception.message)

    def test_a_failed_operation_still_raises_the_operation_error_not_the_branch_one(self):
        both = self.op(
            "op1",
            "failed",
            error={"code": "unknown_harness", "message": "no such harness x"},
            result={"branches": [{"name": "b", "status": {"state": "failed", "reason": "x"}}]},
        )
        with self.assertRaises(bc.OperationFailedError) as raised:
            self.client.wait(bc.Operation(both), check=True)
        self.assertNotIsInstance(raised.exception, bc.BranchFailedError)
        self.assertEqual(raised.exception.code, "unknown_harness")

    def test_a_reply_whose_branch_failed_raises_and_still_reads_what_was_said(self):
        self.fake.feed = [_prompt(3, "b", "go"), _delta(4, "b", "I tried"), _ended(5, "b")]
        failed = self.branch_ended("op1", {"state": "failed", "reason": "crashed"}, cursor=2)
        self.fake.answer("GET", "/v1/operations/op1", (200, failed))
        reply = bc.Reply(self.app, "b", bc.Operation(self.op("op1", "running", cursor=2)))
        with self.assertRaises(bc.BranchFailedError) as raised:
            reply.wait()
        self.assertEqual(raised.exception.branch, "b")
        self.assertEqual(reply.text, "I tried")
        self.assertEqual(reply.branch_info.state, "failed")  # type: ignore[union-attr]

    def test_wait_times_out(self):
        self.fake.always("GET", "/v1/operations/op1", 200, self.op("op1", "running"))
        with self.assertRaises(TimeoutError):
            self.client.wait("op1", timeout=0.05)
        # Each ask is for no longer than what is left of the timeout.
        waits = [float(parse_qs(urlsplit(r[1]).query)["wait"][0]) for r in self.requests("GET", "/v1/operations/op1")]
        self.assertTrue(waits and all(0 <= w <= 0.05 for w in waits), waits)

    def test_wait_for_asks_again_past_the_servers_cap_until_settled(self):
        self.fake.answer(
            "POST",
            f"{REPO}/wait",
            (200, {"settled": [], "pending": ["b"], "timed_out": True}),
            (200, {"settled": [], "pending": ["b"], "timed_out": True}),
            (200, {"settled": [{"name": "b", "status": {"state": "ready"}}], "pending": []}),
        )
        waited = self.app.wait_for(["b"])
        self.assertEqual(waited.pending, [])
        asks = self.requests("POST", f"{REPO}/wait")
        self.assertEqual(len(asks), 3)
        self.assertTrue(all(0 < r[3]["timeout_seconds"] <= 30 for r in asks))
        self.assertNotIn("any", asks[0][3])


class Streaming(ServerCase):
    def test_the_stream_yields_entries_and_resumes_after_a_cut(self):
        self.fake.feed = [
            _prompt(1, "a", "go"),
            _delta(2, "a", "x"),
            _delta(3, "b", "y"),
            _delta(4, "a", "z"),
            _ended(5, "a"),
        ]
        self.fake.cut_after = 2
        stream = self.app.stream(0)
        seen = []
        for entry in stream:
            seen.append(entry.seq)
            if entry.seq == 5:
                stream.close()
        self.assertEqual(seen, [1, 2, 3, 4, 5])
        self.assertEqual(stream.cursor, 5)
        self.assertEqual(stream.head, 5)
        # Reconnected once, from the last position seen.
        self.assertEqual(self.fake.stream_opens, [0, 2])
        first_headers = self.requests("GET", f"{REPO}/events/stream")[0][2]
        self.assertEqual(first_headers["accept"], "text/event-stream")

    def test_the_stream_filters_branches_and_reads_text(self):
        self.fake.feed = [_prompt(1, "a", "go"), _delta(2, "b", "other"), _delta(3, "a", "mine"), _ended(4, "a")]
        texts = []
        stream = self.app.stream(0, branches=["a"])
        for entry in stream:
            texts.append((entry.branch, entry.text, entry.turn_ended))
            if entry.turn_ended:
                stream.close()
        self.assertEqual(texts, [("a", None, False), ("a", "mine", False), ("a", None, True)])

    def test_a_quiet_stream_calls_the_idle_hook_and_can_be_closed_there(self):
        self.fake.feed = [_prompt(1, "a", "go")]
        self.fake.hold = 2.0
        stream = self.app.stream(1, read_timeout=0.2)
        idles = []

        def idle() -> None:
            idles.append(time.monotonic())
            if len(idles) == 2:
                stream.close()

        stream.on_idle = idle
        started = time.monotonic()
        self.assertEqual(list(stream), [])
        self.assertEqual(len(idles), 2)
        self.assertLess(time.monotonic() - started, 1.5)
        self.assertEqual(self.fake.stream_opens, [1, 1])

    def test_the_stream_is_a_context_manager_that_closes_however_the_block_ends(self):
        self.fake.feed = [_prompt(1, "a", "go"), _delta(2, "a", "x")]
        with self.app.stream(0) as stream:
            entries = iter(stream)  # held, as a `for` loop holds it, so the connection stays open
            first = next(entries)
            self.assertIsNotNone(stream._response)
        self.assertEqual(first.seq, 1)
        self.assertIsNone(stream._response)
        self.assertEqual(list(stream), [])
        with self.assertRaises(KeyError), self.app.stream(0) as failing:
            for _entry in failing:
                raise KeyError("the surface's own failure")
        self.assertIsNone(failing._response)
        self.assertEqual(list(failing), [])

    def test_a_cursor_past_the_end_is_an_invalid_request(self):
        # The fake has no feed; the real server answers 400 cursor_out_of_range.
        handler_fake = self.fake

        def stream_error(_body: Any) -> tuple[int, Any]:
            return 400, _error("cursor_out_of_range", "past the end", {"head": 3})

        handler_fake.routes[("GET", f"{REPO}/events/stream")] = stream_error
        # Route the stream path through the scripted table for this test.
        original = Handler._stream

        def scripted(self_: Handler, query: str, headers: dict[str, str]) -> None:
            status, answer = handler_fake.response("GET", f"{REPO}/events/stream", None)
            self_._send(status, answer)

        Handler._stream = scripted  # type: ignore[method-assign]
        try:
            with self.assertRaises(bc.InvalidRequestError) as raised:
                list(self.app.stream(9, max_failures=0))
            self.assertEqual(raised.exception.detail, {"head": 3})
        finally:
            Handler._stream = original  # type: ignore[method-assign]


class Replies(ServerCase):
    def test_a_reply_is_this_branchs_text_between_the_operations_cursors(self):
        self.fake.feed = [
            _delta(1, "slack-c1", "earlier"),
            _prompt(3, "slack-c1", "hello"),
            _delta(4, "other", "not mine"),
            _delta(5, "slack-c1", "Hel"),
            _delta(6, "slack-c1", "lo"),
            _ended(7, "slack-c1"),
            _delta(8, "slack-c1", "a later turn"),
        ]
        op = bc.Operation(self.op("op1", "succeeded", "slack-c1", cursor=2, end=7))
        reply = bc.Reply(self.app, "slack-c1", op)
        self.assertEqual(reply.text, "Hello")
        self.assertEqual(reply.text, "Hello")  # cached, read once
        self.assertEqual(self.fake.stream_opens, [2])
        self.assertFalse(reply.steered)

    def test_a_reply_streams_while_the_operation_runs_and_stops_when_it_ends(self):
        self.fake.feed = [_prompt(3, "b", "hello"), _delta(4, "b", "Hi"), _ended(5, "b")]
        self.fake.hold = 0.5
        self.fake.answer(
            "GET",
            "/v1/operations/op1",
            (200, self.op("op1", "running", cursor=2)),
            (
                200,
                self.op(
                    "op1",
                    "succeeded",
                    cursor=2,
                    end=5,
                    result={"branches": [{"name": "b", "status": {"state": "ready"}}]},
                ),
            ),
        )
        op = bc.Operation(self.op("op1", "running", cursor=2))
        reply = bc.Reply(self.app, "b", op)
        self.assertEqual(reply.text, "")  # not finished: nothing yet
        self.assertEqual(list(reply.deltas(read_timeout=0.3)), ["Hi"])
        self.assertTrue(reply.operation is not None and reply.operation.done)
        self.assertEqual(reply.branch_info.state, "ready")  # type: ignore[union-attr]

    def test_a_reply_for_an_operation_that_recorded_nothing_is_empty_without_a_request(self):
        op = bc.Operation(self.op("op1", "failed", cursor=2, end=2, error={"code": "unknown_harness", "message": "x"}))
        self.assertEqual(bc.Reply(self.app, "b", op).text, "")
        self.assertEqual(self.fake.stream_opens, [])
        with self.assertRaises(bc.OperationFailedError):
            bc.Reply(self.app, "b", op).wait()
        with self.assertRaises(ValueError):
            bc.Reply(self.app, "b", None).wait()


class Surfaces(ServerCase):
    def setUp(self) -> None:
        super().setUp()
        self.surface = self.app.surface("slack", harness="claude-code", budget_usd=5, connectors=["github:read"])
        self.conversation = self.surface.conversation("C 01/23")

    def test_the_first_message_creates_the_branch_and_the_next_continues_it(self):
        self.assertEqual(self.conversation.branch, "slack-c-01-23")
        branch = f"{REPO}/branches/slack-c-01-23"
        self.fake.answer(
            "GET",
            branch,
            (404, _error("unknown_branch")),
            (200, {"name": "slack-c-01-23", "status": {"state": "ready"}}),
        )
        self.fake.answer("POST", f"{REPO}/tasks", (202, self.op("op1", "queued", "slack-c-01-23", kind="task")))
        self.fake.answer("POST", f"{branch}/send", (202, self.op("op2", "queued", "slack-c-01-23")))
        first = self.conversation.say("hello", budget_usd=1)
        self.assertEqual(first.operation.id, "op1")  # type: ignore[union-attr]
        task = self.requests("POST", f"{REPO}/tasks")[0][3]
        self.assertEqual(task["name"], "slack-c-01-23")
        self.assertEqual(task["harness"], "claude-code")
        self.assertEqual(task["budget"], {"max_usd": 1})  # the message's option wins over the surface's
        self.assertIs(task["isolated"], True)
        self.assertEqual(task["provision"]["connectors"][0]["connector"], "github")
        second = self.conversation.say("more")
        self.assertEqual(second.operation.id, "op2")  # type: ignore[union-attr]
        sent = self.requests("POST", f"{branch}/send")[0][3]
        self.assertEqual(sent, {"prompt": "more"})

    def test_a_branch_created_in_between_is_continued(self):
        branch = f"{REPO}/branches/slack-c-01-23"
        self.fake.answer("GET", branch, (404, _error("unknown_branch")))
        self.fake.answer("POST", f"{REPO}/tasks", (409, _error("branch_exists")))
        self.fake.answer("POST", f"{branch}/send", (202, self.op("op2", "queued", "slack-c-01-23")))
        self.assertEqual(self.conversation.say("hello").operation.id, "op2")  # type: ignore[union-attr]

    def _busy(self, *then: tuple[int, Any]) -> str:
        branch = f"{REPO}/branches/slack-c-01-23"
        self.fake.always("GET", branch, 200, {"name": "slack-c-01-23", "status": {"state": "running"}})
        self.fake.answer("POST", f"{branch}/send", (409, _error("branch_busy", "busy", {"holder": "op7"})), *then)
        return branch

    def test_skip_raises_while_the_branch_is_busy(self):
        self._busy()
        with self.assertRaises(bc.BranchBusyError):
            self.conversation.say("hello", busy="skip")
        with self.assertRaises(ValueError):
            self.conversation.say("hello", busy="later")

    def test_queue_waits_for_the_holder_then_sends_with_the_same_key(self):
        branch = self._busy((202, self.op("op8", "queued", "slack-c-01-23")))
        self.fake.answer(
            "GET", "/v1/operations/op7", (200, self.op("op7", "running")), (200, self.op("op7", "succeeded", end=3))
        )
        reply = self.conversation.say("hello", busy="queue", key="k-9")
        self.assertEqual(reply.operation.id, "op8")  # type: ignore[union-attr]
        sends = self.requests("POST", f"{branch}/send")
        self.assertEqual([r[2]["idempotency-key"] for r in sends], ["k-9", "k-9"])
        self.assertEqual(len(self.requests("GET", "/v1/operations/op7")), 2)

    def test_queue_gives_up_at_the_timeout(self):
        self._busy()
        self.fake.always("GET", "/v1/operations/op7", 200, self.op("op7", "running"))
        with self.assertRaises(TimeoutError):
            self.conversation.say("hello", busy="queue", timeout=0.2)

    def test_steer_joins_the_running_turn_and_the_reply_is_that_turns(self):
        branch = self._busy()
        self.fake.answer(
            "POST",
            f"{branch}/steer",
            (
                200,
                {
                    "id": 1,
                    "branch": "slack-c-01-23",
                    "by": "tester",
                    "text": "hello",
                    "requested_at_ms": 1,
                    "state": {"state": "accepted"},
                },
            ),
        )
        self.fake.always("GET", "/v1/operations/op7", 200, self.op("op7", "running", "slack-c-01-23", cursor=4))
        reply = self.conversation.say("hello", busy="steer")
        self.assertTrue(reply.steered)
        self.assertEqual(reply.steer.state, {"state": "accepted"})  # type: ignore[union-attr]
        self.assertEqual(reply.operation.id, "op7")  # type: ignore[union-attr]
        self.assertEqual(self.requests("POST", f"{branch}/steer")[0][3], {"text": "hello"})

    def test_a_refused_steer_falls_back_to_queueing(self):
        branch = self._busy((202, self.op("op8", "queued", "slack-c-01-23")))
        self.fake.answer("POST", f"{branch}/steer", (409, _error("steer_refused", "no turn")))
        self.fake.answer("GET", "/v1/operations/op7", (200, self.op("op7", "succeeded", end=2)))
        reply = self.conversation.say("hello", busy="steer")
        self.assertFalse(reply.steered)
        self.assertEqual(reply.operation.id, "op8")  # type: ignore[union-attr]

    def test_conversations_lists_the_surfaces_branches(self):
        self.fake.always(
            "GET",
            f"{REPO}/branches",
            200,
            {
                "branches": [
                    {"name": "slack-a", "status": {"state": "ready"}},
                    {"name": "other", "status": {"state": "ready"}},
                    {"name": "slack-b", "status": {"state": "running"}},
                ]
            },
        )
        self.assertEqual([b.name for b in self.surface.conversations()], ["slack-a", "slack-b"])


class Approvals(ServerCase):
    def test_approvals_are_listed_and_answered_as_the_caller(self):
        ask = {
            "id": "ap1",
            "branch": "b",
            "turn": 2,
            "subject": "local:tester",
            "about": {
                "kind": "operation",
                "connector": "github",
                "operation": "github.issues.create",
                "class": "create",
            },
            "created_ms": 1,
        }
        self.fake.answer("GET", f"{REPO}/approvals", (200, {"approvals": [ask]}))
        self.fake.answer("POST", f"{REPO}/approvals/ap1/allow", (200, dict(ask, answer={"decision": "allow"})))
        self.fake.answer(
            "GET",
            f"{REPO}/effects",
            (
                200,
                {
                    "effects": [
                        {
                            "id": "e1",
                            "branch": "b",
                            "connector": "github",
                            "operation": "github.issues.create",
                            "state": "performed",
                        }
                    ]
                },
            ),
        )
        waiting = self.app.approvals()
        self.assertEqual([a.id for a in waiting], ["ap1"])
        self.assertTrue(waiting[0].pending)
        self.assertEqual(waiting[0].about["connector"], "github")
        answered = self.app.allow("ap1", reason="fine", surface="companion")
        self.assertFalse(answered.pending)
        self.assertEqual(
            self.requests("POST", f"{REPO}/approvals/ap1/allow")[0][3], {"reason": "fine", "surface": "companion"}
        )
        effects = self.app.effects(branch="b")
        self.assertEqual(effects[0].operation, "github.issues.create")
        self.assertEqual(self.requests("GET", f"{REPO}/effects")[0][1], f"{REPO}/effects?branch=b")


class Checkpoints(ServerCase):
    REWOUND: ClassVar[dict] = {
        "branch": "b",
        "from": 3,
        "to": 1,
        "commit": "c0ffee",
        "session": {"mode": "summary", "turns": [1], "reason": "its session went on to turn 3"},
    }

    def test_rewind_posts_the_checkpoint_with_a_key_and_reads_the_answer(self):
        self.fake.answer("POST", f"{REPO}/branches/b/rewind", (200, self.REWOUND))
        rewound = self.app.rewind("b", 1)
        self.assertIsInstance(rewound, bc.Rewound)
        ((_method, _path, headers, body),) = self.requests("POST", f"{REPO}/branches/b/rewind")
        self.assertEqual(body, {"to": 1})
        self.assertEqual(len(headers["idempotency-key"]), 32)
        self.assertEqual((rewound.branch, rewound["from"], rewound.to, rewound.commit), ("b", 3, 1, "c0ffee"))
        self.assertEqual(rewound.session_mode, "summary")
        self.assertEqual(rewound.session["turns"], [1])

    def test_rewind_refusals_are_typed(self):
        cases = [
            (404, _error("unknown_checkpoint", "no checkpoint 9", {"branch": "b", "turn": 9}), bc.NotFoundError),
            (409, _error("checkpoint_unavailable", "ref moved"), bc.ConflictError),
            (409, _error("branch_busy", "busy", {"holder": "op7"}), bc.BranchBusyError),
            (409, _error("running"), bc.RunningError),
            (403, _error("denied", "merged"), bc.DeniedError),
        ]
        for status, body, expected in cases:
            self.fake.answer("POST", f"{REPO}/branches/b/rewind", (status, body))
            with self.assertRaises(expected) as raised:
                self.app.rewind("b", 9)
            self.assertIs(type(raised.exception), expected, body)
            self.assertEqual(raised.exception.code, body["error"]["code"])
        self.assertEqual(raised.exception.status, 403)

    def test_fork_at_a_checkpoint_sends_at_and_without_it_sends_none(self):
        self.fake.always("POST", f"{REPO}/branches/b/fork", 202, self.op("op1", "queued", "b-2", kind="fork"))
        op = self.app.fork("b", "try again", name="b-2", at=0, harness="codex")
        self.assertEqual(op.id, "op1")
        self.app.fork("b", "plain")
        at, plain = (r[3] for r in self.requests("POST", f"{REPO}/branches/b/fork"))
        # `at=0` (the base) is sent; a false `fresh_session` is left out.
        self.assertEqual(at, {"prompt": "try again", "name": "b-2", "at": 0, "harness": "codex"})
        self.assertEqual(plain, {"prompt": "plain"})

    def test_fork_at_a_checkpoint_the_branch_lacks_is_refused_before_queueing(self):
        self.fake.answer(
            "POST", f"{REPO}/branches/b/fork", (404, _error("unknown_checkpoint", "b has no checkpoint 5"))
        )
        with self.assertRaises(bc.NotFoundError) as raised:
            self.app.fork("b", "x", at=5)
        self.assertEqual(raised.exception.code, "unknown_checkpoint")


class Maps(ServerCase):
    def test_a_map_is_an_operation_whose_items_carry_their_ids_and_whose_task_is_a_tasks(self):
        self.fake.always("POST", f"{REPO}/maps", 202, self.op("op1", "queued", kind="map", branches=[]))
        op = self.app.map(
            "Triage {{item.title}}",
            [{"id": "a", "title": "one"}, {"id": 7, "title": "two"}],
            name="triage",
            schema={"type": "object"},
            concurrency=2,
            harness="codex",
            budget_usd=0.5,
            connectors=["github:read"],
        )
        self.assertEqual((op.id, op.kind), ("op1", "map"))
        ((_method, _path, headers, body),) = self.requests("POST", f"{REPO}/maps")
        self.assertEqual(len(headers["idempotency-key"]), 32)
        self.assertEqual(
            body["items"],
            [{"id": "a", "value": {"id": "a", "title": "one"}}, {"id": "7", "value": {"id": 7, "title": "two"}}],
        )
        self.assertEqual((body["name"], body["schema"], body["concurrency"]), ("triage", {"type": "object"}, 2))
        self.assertEqual(
            body["task"],
            {
                "prompt": "Triage {{item.title}}",
                "harness": "codex",
                "budget": {"max_usd": 0.5},
                "isolated": True,
                "provision": {"connectors": [{"connector": "github", "operations": ["*"], "mode": "read"}]},
            },
        )
        for absent in ("retries", "remove_done", "retry_failed", "reduce"):
            self.assertNotIn(absent, body)
        # A dict gives the ids; a value without one is refused here.
        self.app.map("p", {"x": 1, "y": [2]})
        self.assertEqual(
            self.requests("POST", f"{REPO}/maps")[1][3]["items"], [{"id": "x", "value": 1}, {"id": "y", "value": [2]}]
        )
        for bad in ([{"title": "no id"}], ["plain"], [{"id": True}], [{"id": " "}]):
            with self.assertRaises(ValueError):
                self.app.map("p", bad)
        self.assertEqual(len(self.requests("POST", f"{REPO}/maps")), 2)

    def test_maps_are_resumed_listed_shown_and_forgotten(self):
        self.fake.answer("POST", f"{REPO}/maps/triage/resume", (202, self.op("op2", "queued", kind="map")))
        self.fake.answer("GET", f"{REPO}/maps", (200, {"maps": [{"name": "triage", "total": 2, "done": 1}]}))
        self.fake.answer("GET", f"{REPO}/maps/triage", (200, {"name": "triage", "rows": [], "columns": ["result"]}))
        self.fake.answer("DELETE", f"{REPO}/maps/triage", (200, {"removed": "triage"}))
        self.assertEqual(self.app.resume_map("triage", retry_failed=True, key="k1").id, "op2")
        ((_m, _p, headers, body),) = self.requests("POST", f"{REPO}/maps/triage/resume")
        self.assertEqual((headers["idempotency-key"], body), ("k1", {"retry_failed": True}))
        self.assertEqual([(m.name, m.done) for m in self.app.maps()], [("triage", 1)])
        self.assertEqual(self.app.map_report("triage").columns, ["result"])
        self.assertIsNone(self.app.remove_map("triage"))

    def test_task_records_are_listed_and_one_is_found_by_its_key(self):
        view = {"id": "t-1234", "prompt": "fix", "attempts": [{"branch": "fix"}]}
        self.fake.answer("GET", f"{REPO}/task-records", (200, {"tasks": [view]}))
        self.fake.answer("GET", f"{REPO}/task-records/t-12", (200, view), (404, _error("not_found", "no task")))
        self.assertEqual([t.id for t in self.app.task_records()], ["t-1234"])
        self.assertEqual(self.app.task_record("t-12").attempts, [{"branch": "fix"}])
        with self.assertRaises(bc.NotFoundError):
            self.app.task_record("t-12")

    def test_map_refusals_are_typed(self):
        self.fake.answer("POST", f"{REPO}/maps", (400, _error("invalid_request", "a map takes no task.plan")))
        self.fake.answer("POST", f"{REPO}/maps/old/resume", (400, _error("invalid_request", "not started here")))
        with self.assertRaises(bc.InvalidRequestError):
            self.app.map("p", {"a": 1}, plan=True)
        with self.assertRaises(bc.InvalidRequestError):
            self.app.resume_map("old")


class Knowledge(ServerCase):
    ENTRY: ClassVar[dict] = {
        "id": 3,
        "scope": {"path": "src/**"},
        "text": "Run the parser tests",
        "source": {"from": "person", "name": "ana"},
        "status": "adopted",
        "created_ms": 1,
    }

    def test_entries_are_added_listed_decided_edited_exported_and_removed(self):
        entry = self.ENTRY
        self.fake.answer("POST", f"{REPO}/knowledge", (200, entry), (200, dict(entry, status="proposed")))
        self.fake.answer("GET", f"{REPO}/knowledge", (200, {"entries": [entry]}))
        self.fake.answer("GET", f"{REPO}/knowledge/3", (200, entry))
        self.fake.answer("POST", f"{REPO}/knowledge/3/adopt", (200, entry))
        self.fake.answer("POST", f"{REPO}/knowledge/3/reject", (200, dict(entry, status="rejected")))
        self.fake.answer("POST", f"{REPO}/knowledge/3/edit", (200, dict(entry, text="new")))
        self.fake.answer("DELETE", f"{REPO}/knowledge/3", (200, entry))
        self.fake.answer(
            "GET", f"{REPO}/knowledge/export", (200, {"markdown": "- Run the parser tests\n", "entries": [3]})
        )
        self.fake.answer("POST", f"{REPO}/branches/b/distill", (200, {"branch": "b", "proposed": [entry]}))

        self.assertEqual(self.app.add_knowledge("Run the parser tests", path="src/**").id, 3)
        self.assertEqual(self.app.add_knowledge("Maybe", propose=True).status, "proposed")
        first, second = (r[3] for r in self.requests("POST", f"{REPO}/knowledge"))
        self.assertEqual(first, {"text": "Run the parser tests", "scope": {"path": "src/**"}})
        self.assertEqual(second, {"text": "Maybe", "propose": True})
        self.assertEqual([e.id for e in self.app.knowledge(status="adopted")], [3])
        self.assertEqual(self.requests("GET", f"{REPO}/knowledge")[0][1], f"{REPO}/knowledge?status=adopted")
        self.assertEqual(self.app.knowledge_entry(3).scope, {"path": "src/**"})
        self.app.adopt_knowledge(3)
        self.assertEqual(self.app.reject_knowledge(3, reason="stale").status, "rejected")
        self.assertEqual(self.app.edit_knowledge(3, text="new", kind="").text, "new")
        self.assertEqual(self.app.remove_knowledge(3).id, 3)
        self.assertEqual(self.app.export_knowledge().entries, [3])
        self.assertEqual(self.app.distill("b").proposed[0]["id"], 3)
        self.assertEqual(self.requests("POST", f"{REPO}/knowledge/3/adopt")[0][3], {})
        self.assertEqual(self.requests("POST", f"{REPO}/knowledge/3/reject")[0][3], {"reason": "stale"})
        # `""` clears a field, so it is sent; an absent one is not.
        self.assertEqual(self.requests("POST", f"{REPO}/knowledge/3/edit")[0][3], {"text": "new", "kind": ""})
        self.assertEqual(self.requests("POST", f"{REPO}/branches/b/distill")[0][3], {})
        for method, path in (
            ("POST", f"{REPO}/knowledge"),
            ("POST", f"{REPO}/knowledge/3/adopt"),
            ("POST", f"{REPO}/knowledge/3/edit"),
            ("POST", f"{REPO}/branches/b/distill"),
        ):
            self.assertEqual(len(self.requests(method, path)[0][2]["idempotency-key"]), 32, path)

    def test_an_unknown_entry_is_not_found_and_a_bad_scope_is_invalid(self):
        self.fake.answer("GET", f"{REPO}/knowledge/9", (404, _error("unknown_knowledge", "no knowledge entry 9")))
        self.fake.answer("POST", f"{REPO}/knowledge", (422, _error("unsupported", "not a usable path glob")))
        with self.assertRaises(bc.NotFoundError) as missing:
            self.app.knowledge_entry(9)
        self.assertEqual(missing.exception.code, "unknown_knowledge")
        with self.assertRaises(bc.BranchyardError) as bad:
            self.app.add_knowledge("x", path="[")
        self.assertEqual((bad.exception.code, bad.exception.status), ("unsupported", 422))


class Fleet(ServerCase):
    SERVICE: ClassVar[dict] = {"id": "s1", "kind": "model_gateway", "health": "healthy", "endpoints": []}

    def test_discovery_needs_no_token(self):
        public = {"service": "branchyard", "api": "/v1", "repos": ["app"], "services": []}
        self.fake.answer("GET", "/.well-known/branchyard", (200, public))
        self.assertEqual(self.client.well_known().repos, ["app"])
        ((_m, _p, headers, _b),) = self.requests("GET", "/.well-known/branchyard")
        self.assertNotIn("authorization", headers)

    def test_services_are_listed_registered_deregistered_and_reclaimed(self):
        self.fake.answer("GET", "/v1/services", (200, {"services": [self.SERVICE]}))
        self.fake.answer("POST", "/v1/services", (200, self.SERVICE))
        self.fake.answer("DELETE", "/v1/services/s1", (200, self.SERVICE), (409, _error("service_held", "held")))
        self.fake.answer("POST", "/v1/services/gc", (200, {"services": [self.SERVICE]}))
        self.fake.answer("GET", "/v1/inventory", (200, {"workers": [{"id": "w1", "this": True, "inventory": None}]}))
        self.assertEqual([s.id for s in self.client.services(kind="model_gateway")], ["s1"])
        self.assertEqual(self.requests("GET", "/v1/services")[0][1], "/v1/services?kind=model_gateway")
        self.client.register_service(
            "model_gateway", id="s1", capabilities={"models": ["m"]}, endpoints=[{"url": "http://x"}], ttl_seconds=60
        )
        ((_m, _p, headers, body),) = self.requests("POST", "/v1/services")
        self.assertEqual(
            body,
            {
                "id": "s1",
                "kind": "model_gateway",
                "capabilities": {"models": ["m"]},
                "endpoints": [{"url": "http://x"}],
                "ttl_seconds": 60,
            },
        )
        self.assertEqual(len(headers["idempotency-key"]), 32)
        self.assertEqual(self.client.deregister_service("s1").kind, "model_gateway")
        with self.assertRaises(bc.ConflictError) as held:
            self.client.deregister_service("s1")
        self.assertEqual(held.exception.code, "service_held")
        self.assertEqual([s.id for s in self.client.reclaim_services()], ["s1"])
        (worker,) = self.client.inventory()
        self.assertTrue(worker.this)
        self.assertIsNone(worker.inventory)

    def test_admin_routes_without_the_scope_are_denied(self):
        self.fake.answer("POST", "/v1/services/gc", (403, _error("scope_required", "", {"scope": "admin"})))
        with self.assertRaises(bc.DeniedError) as denied:
            self.client.reclaim_services()
        self.assertEqual(denied.exception.detail, {"scope": "admin"})


class Undo(ServerCase):
    def test_reconcile_and_undo_plan_and_undo(self):
        self.fake.answer("POST", f"{REPO}/effects/reconcile", (200, {"settled": [["e1", "performed"]], "unknown": []}))
        self.fake.answer("GET", f"{REPO}/branches/b/undo", (200, {"branch": "b", "to": 1, "reversible": []}))
        self.fake.always("POST", f"{REPO}/branches/b/undo", 200, {"plan": {"branch": "b"}, "outcomes": []})
        self.assertEqual(self.app.reconcile_effects().settled, [["e1", "performed"]])
        self.assertEqual(self.requests("POST", f"{REPO}/effects/reconcile")[0][3], {})
        self.assertEqual(self.app.undo_plan("b", to=1).to, 1)
        self.assertEqual(self.requests("GET", f"{REPO}/branches/b/undo")[0][1], f"{REPO}/branches/b/undo?to=1")
        self.assertEqual(self.app.undo("b", only=["e1"]).outcomes, [])
        self.app.undo("b", to=2)
        every, later = self.requests("POST", f"{REPO}/branches/b/undo")
        self.assertEqual(every[3], {"to": 0, "only": ["e1"]})
        self.assertEqual(later[3], {"to": 2})
        self.assertEqual(len(every[2]["idempotency-key"]), 32)

    def test_undo_of_an_unknown_branch_is_not_found(self):
        self.fake.answer("POST", f"{REPO}/branches/nope/undo", (404, _error("unknown_branch")))
        with self.assertRaises(bc.NotFoundError):
            self.app.undo("nope")


class ScratchLocks(ServerCase):
    LOCK: ClassVar[dict] = {"name": "cache", "holder_branch": "b", "acquired_at": 5}

    def test_a_lock_is_taken_released_and_read(self):
        self.fake.answer("POST", f"{REPO}/branches/b/scratch/cache/lock", (200, self.LOCK))
        self.fake.answer("POST", f"{REPO}/branches/b/scratch/cache/unlock", (200, {"ok": True}))
        self.fake.answer("GET", f"{REPO}/scratch/cache/lock", (200, {"lock": self.LOCK}), (200, {}))
        self.assertEqual(self.app.lock_scratch("b", "cache", key="k1").holder_branch, "b")
        ((_m, _p, headers, body),) = self.requests("POST", f"{REPO}/branches/b/scratch/cache/lock")
        self.assertEqual((headers["idempotency-key"], body), ("k1", {}))
        self.assertIsNone(self.app.unlock_scratch("b", "cache"))
        self.assertEqual(self.app.scratch_lock_state("cache"), self.LOCK)
        self.assertIsNone(self.app.scratch_lock_state("cache"))

    def test_a_lock_another_turn_holds_is_running(self):
        self.fake.answer("POST", f"{REPO}/branches/c/scratch/cache/lock", (409, _error("running", "held by b")))
        with self.assertRaises(bc.RunningError):
            self.app.lock_scratch("c", "cache")


class TriggerSecretsAndPush(ServerCase):
    def test_a_trigger_secret_is_set_or_generated(self):
        self.fake.answer("POST", "/v1/triggers/hook/secret", (200, {}), (200, {"secret": "generated"}))
        self.assertIsNone(self.client.set_trigger_secret("hook", "mine").secret)
        self.assertEqual(self.client.set_trigger_secret("hook").secret, "generated")
        given, generated = self.requests("POST", "/v1/triggers/hook/secret")
        self.assertEqual((given[3], generated[3]), ({"secret": "mine"}, {}))
        self.assertEqual(len(given[2]["idempotency-key"]), 32)

    def test_web_push_is_described_subscribed_tested_and_unsubscribed(self):
        info = {"enabled": True, "public_key": "BPk", "services": ["push.example"], "subscriptions": [], "kinds": []}
        self.fake.answer("GET", "/v1/app/push", (200, info))
        self.fake.answer("POST", "/v1/app/push/subscriptions", (200, {"subscriptions": 1, "delivered": 0}))
        self.fake.answer("POST", "/v1/app/push/test", (200, {"subscriptions": 1, "delivered": 1}))
        self.fake.answer("DELETE", "/v1/app/push/subscriptions", (200, {"subscriptions": 0, "delivered": 0}))
        self.assertEqual(self.client.push_info().public_key, "BPk")
        endpoint = "https://push.example/e1"
        self.assertEqual(self.client.subscribe_push(endpoint, "p", "a", kinds=["approval"]).subscriptions, 1)
        self.assertEqual(
            self.requests("POST", "/v1/app/push/subscriptions")[0][3],
            {"endpoint": endpoint, "keys": {"p256dh": "p", "auth": "a"}, "kinds": ["approval"]},
        )
        self.assertEqual(self.client.test_push().delivered, 1)
        self.assertEqual(self.client.unsubscribe_push(endpoint).subscriptions, 0)
        self.assertEqual(self.requests("DELETE", "/v1/app/push/subscriptions")[0][3], {"endpoint": endpoint})

    def test_push_on_a_server_that_sends_none_is_not_allowed(self):
        self.fake.answer("POST", "/v1/app/push/subscriptions", (403, _error("push_not_enabled")))
        self.fake.answer("GET", "/v1/app/push", (404, _error("not_found", "no such route")))
        with self.assertRaises(bc.NotAllowedError):
            self.client.subscribe_push("https://push.example/e1", "p", "a")
        with self.assertRaises(bc.NotFoundError):
            self.client.push_info()


class GrantsAndNames(unittest.TestCase):
    def test_a_grant_round_trips_between_the_flag_and_the_object(self):
        cases = {
            "github": ("github:read", {"connector": "github", "operations": ["*"], "mode": "read"}),
            "github:read": ("github:read", {"connector": "github", "operations": ["*"], "mode": "read"}),
            "github:write": ("github:write", {"connector": "github", "operations": ["*"], "mode": "write"}),
            "github:write:issues.*": (
                "github:write:issues.*",
                {"connector": "github", "operations": ["issues.*"], "mode": "write"},
            ),
            "github:write+confirm:issues.create": (
                "github:write+confirm:issues.create",
                {"connector": "github", "operations": ["issues.create"], "mode": "write", "confirm": "allow"},
            ),
            "github@work:read:issues.list, pulls.list": (
                "github@work:read:issues.list,pulls.list",
                {"connector": "github", "operations": ["issues.list", "pulls.list"], "mode": "read", "account": "work"},
            ),
        }
        for text, (canonical, obj) in cases.items():
            grant = bc.Grant.parse(text)
            self.assertEqual(str(grant), canonical, text)
            self.assertEqual(grant.to_json(), obj, text)
            self.assertEqual(bc.Grant.from_json(obj), grant, text)
            self.assertEqual(bc.Grant.from_json(canonical), grant, text)
        self.assertEqual(bc.Grant.read("github", "issues.list"), bc.Grant.parse("github:read:issues.list"))
        self.assertEqual(bc.Grant.write("linear", confirm=True, account="w"), bc.Grant.parse("linear@w:write+confirm"))
        self.assertEqual(bc.Grant.from_json(bc.Grant.parse("x:read")), bc.Grant.parse("x:read"))
        self.assertEqual(
            bc.grants_json([bc.Grant.parse("a"), "b:write", {"connector": "c"}]),
            [
                {"connector": "a", "operations": ["*"], "mode": "read"},
                {"connector": "b", "operations": ["*"], "mode": "write"},
                {"connector": "c", "operations": ["*"], "mode": "read"},
            ],
        )

    def test_a_bad_grant_is_refused_with_the_servers_words(self):
        for text, words in [
            ("github:admin", "not read, write or write+confirm"),
            ("shipping/v2:read", "letters, digits"),
            ("github@a b:read", "account"),
            ("github:read:", "operation"),
            ("github:read:a,,b", "operation"),
        ]:
            with self.assertRaises(ValueError, msg=text) as raised:
                bc.Grant.parse(text)
            self.assertIn(words, str(raised.exception), text)
        with self.assertRaises(ValueError):
            bc.Grant("github", mode="read", confirm=True)
        with self.assertRaises(ValueError):
            bc.Grant.from_json({"operations": ["*"]})

    def test_branch_names_fold_free_text(self):
        self.assertEqual(bc.branch_name("slack", "C0123"), "slack-c0123")
        self.assertEqual(bc.branch_name("slack", "#general", "1699999999.000100"), "slack-general-1699999999.000100")
        self.assertEqual(bc.branch_name("discord", "user:42"), "discord-user-42")
        self.assertEqual(len(bc.branch_name("a", "x" * 100)), 60)
        with self.assertRaises(ValueError):
            bc.branch_name("", "---")

    def test_records_read_as_attributes_and_statuses_compare_to_their_state(self):
        op = bc.Operation(
            {"id": "o", "state": "failed", "branches": ["a", "b"], "error": {"code": "x", "message": "m"}}
        )
        self.assertTrue(op.done)
        self.assertTrue(op.failed)
        self.assertIsNone(op.branch)
        self.assertIsNone(op.inspection)
        self.assertEqual(op.raw, dict(op))
        info = bc.BranchInfo({"name": "a", "status": {"state": "budget_exceeded", "limit": "max_usd"}})
        self.assertEqual(info.status, "budget_exceeded")
        self.assertNotEqual(info.status, "ready")
        self.assertEqual(info.status.limit, "max_usd")
        self.assertEqual(str(info.status), "budget_exceeded")
        self.assertTrue(info.settled)
        self.assertFalse(bc.BranchInfo({"status": {"state": "waiting_on_children"}}).settled)
        entry = bc.FeedEntry(_delta(1, "a", "t"))
        self.assertEqual(entry.text, "t")
        self.assertIsNone(entry.status)
        self.assertEqual(
            bc.FeedEntry({"seq": 2, "branch": "a", "activity": {"status": {"state": "ready"}}}).status, "ready"
        )


class ModuleSpawnAndReport(unittest.TestCase):
    """`sdk/python/branchyard.py`, which runs `by --json`: what it asks `by`
    for, with `_run` replaced, and what it makes of the answers."""

    def run_by(self, call, answer):
        seen = []

        def fake(args, result_on_failure=False, stdin=None):
            seen.append((args, stdin))
            return answer

        with mock.patch.object(bm, "_run", fake):
            returned = call()
        return seen, returned

    SPAWNED: ClassVar[dict[str, Any]] = {
        "name": "kid",
        "git_branch": "by/kid",
        "harness": "h",
        "profile": "p",
        "base": "b",
        "depth": 1,
        "status": {"state": "running"},
        "budget": {},
    }

    def test_spawn_gives_by_the_schema_and_whether_it_is_required(self):
        schema = {"type": "object", "required": ["files"]}
        seen, spawned = self.run_by(
            lambda: bm.spawn("work", name="kid", result={"schema": schema, "required": True}), self.SPAWNED
        )
        ((args, stdin),) = seen
        self.assertEqual(spawned.name, "kid")
        self.assertEqual(stdin, "work")
        self.assertEqual(args[args.index("--result-schema") + 1], json.dumps(schema))
        self.assertIn("--result-required", args)
        # Optional: the schema alone.
        seen, _ = self.run_by(lambda: bm.spawn("work", result={"schema": schema}), self.SPAWNED)
        self.assertIn("--result-schema", seen[0][0])
        self.assertNotIn("--result-required", seen[0][0])
        # None at all: neither flag.
        seen, _ = self.run_by(lambda: bm.spawn("work"), self.SPAWNED)
        self.assertNotIn("--result-schema", seen[0][0])

    def test_spawn_refuses_a_result_that_is_not_a_contract_before_running_by(self):
        for bad in ({}, {"required": True}, {"schema": {}, "requried": True}, "object", ["schema"]):
            with self.assertRaises(ValueError):
                self.run_by(lambda: bm.spawn("work", result=bad), self.SPAWNED)  # noqa: B023

    def test_spawn_selects_deliverable_only_for_an_explicit_required_contract(self):
        schema = {"type": "object"}
        for purpose in ("report", "deliverable"):
            with self.subTest(purpose=purpose):
                seen, _ = self.run_by(
                    lambda purpose=purpose: bm.spawn(
                        "Research", result={"schema": schema, "required": True, "purpose": purpose}
                    ),
                    self.SPAWNED,
                )
                ((args, stdin),) = seen
                self.assertEqual(stdin, "Research")
                self.assertEqual(json.loads(args[args.index("--result-schema") + 1]), schema)
                self.assertIn("--result-required", args)
                self.assertEqual("--result-deliverable" in args, purpose == "deliverable")
        seen, _ = self.run_by(
            lambda: bm.spawn("Research", result={"schema": schema, "required": False, "purpose": "report"}),
            self.SPAWNED,
        )
        self.assertNotIn("--result-required", seen[0][0])
        self.assertNotIn("--result-deliverable", seen[0][0])

    def test_spawn_refuses_bad_purpose_or_requirement_before_any_cli_call(self):
        invalid = [
            *({"schema": {}, "purpose": purpose} for purpose in ("score", None, [], {})),
            *({"schema": {}, "required": required} for required in ("yes", 1, None)),
            {"schema": {}, "purpose": "deliverable"},
            {"schema": {}, "purpose": "deliverable", "required": False},
        ]
        with mock.patch.object(bm, "_run") as run:
            for contract in invalid:
                with self.subTest(contract=contract), self.assertRaises(ValueError):
                    bm.spawn("Research", result=contract)
            run.assert_not_called()

    def test_spawn_passes_explicit_capture_path_and_limits_without_changing_schema(self):
        schema = {"type": "object", "additionalProperties": False}
        result = {
            "schema": schema,
            "required": True,
            "purpose": "deliverable",
            "capture": {"manifest": "outputs/artifacts.json", "max_files": 4, "max_bytes": 4096},
        }
        seen, _ = self.run_by(lambda: bm.spawn("Create a report", result=result), self.SPAWNED)
        args = seen[0][0]
        self.assertEqual(args[args.index("--result-artifacts") + 1], "outputs/artifacts.json")
        self.assertEqual(args[args.index("--result-artifacts-max-files") + 1], "4")
        self.assertEqual(args[args.index("--result-artifacts-max-bytes") + 1], "4096")
        self.assertEqual(json.loads(args[args.index("--result-schema") + 1]), schema)
        with mock.patch.object(bm, "_run") as run:
            for capture in [
                {},
                {"manifest": ""},
                {"manifest": "x", "max_files": True},
                {"manifest": "x", "max_bytes": 0},
                {"manifest": "x", "actor": "other"},
            ]:
                with self.subTest(capture=capture), self.assertRaises(ValueError):
                    bm.spawn("Create a report", result={**result, "capture": capture})
            run.assert_not_called()

    def test_handoff_builds_one_cli_request_and_keeps_the_frozen_payload(self):
        expected = HANDOFF_MESSAGE["handoff"]["version"]
        seen, sent = self.run_by(
            lambda: bm.handoff("source", "consumer", ["art7"], "Use these sources", expected),
            HANDOFF_MESSAGE,
        )
        self.assertEqual(
            seen,
            [
                (
                    [
                        "handoff",
                        "--to",
                        "consumer",
                        "--artifact",
                        "art7",
                        "--note",
                        "Use these sources",
                        "--expected-version",
                        json.dumps(expected),
                        "--",
                        "source",
                    ],
                    None,
                )
            ],
        )
        self.assertEqual(sent.handoff, HANDOFF_MESSAGE["handoff"])
        self.assertTrue(sent.next_turn)
        seen, _ = self.run_by(lambda: bm.handoff("source", "consumer"), HANDOFF_MESSAGE)
        self.assertEqual(seen, [(["handoff", "--to", "consumer", "--", "source"], None)])
        old = bm._message({"id": 1, "from": "kid", "to": "lead", "kind": "report", "text": "done", "at_ms": 1})
        self.assertIsNone(old.handoff)
        self.assertFalse(old.next_turn)

    def test_report_gives_by_the_result_as_json(self):
        message = {"id": 1, "from": "kid", "to": "p", "kind": "report", "text": "done", "at_ms": 1}
        seen, sent = self.run_by(lambda: bm.report("found", result={"files": ["a.rs"]}), message)
        self.assertEqual(seen[0][0], ["report", "--result", '{"files": ["a.rs"]}', "--", "found"])
        self.assertEqual(sent.text, "done")
        seen, _ = self.run_by(lambda: bm.report("plain"), message)
        self.assertEqual(seen[0][0], ["report", "--", "plain"])
        # An empty result is still a result.
        seen, _ = self.run_by(lambda: bm.report("empty", result={}), message)
        self.assertEqual(seen[0][0], ["report", "--result", "{}", "--", "empty"])

    def test_inspect_and_wait_carry_the_contract_and_the_result(self):
        contract = {"schema": {"type": "object"}, "required": True}
        inspection = {
            "name": "kid",
            "status": {"state": "ready"},
            "result_contract": contract,
            "result": {"files": []},
        }
        _, inspected = self.run_by(lambda: bm.inspect("kid"), inspection)
        self.assertEqual(inspected.result_contract, contract)
        self.assertEqual(inspected.result, {"files": []})
        _, waited = self.run_by(lambda: bm.wait_all("kid"), {"settled": [inspection], "pending": []})
        self.assertEqual(waited.settled[0].result, {"files": []})
        self.assertEqual(waited.settled[0].result_contract, contract)
        _, bare = self.run_by(lambda: bm.inspect("kid"), {"name": "kid", "status": {"state": "running"}})
        self.assertIsNone(bare.result)
        self.assertIsNone(bare.result_contract)


FAKE_BY = f"""#!{sys.executable}
import json, os, signal, sys, time
args = sys.argv[1:]
with open(os.environ["FAKE_BY_LOG"], "a") as f:
    f.write(json.dumps(args) + "\\n")
if args and args[0] == "serve":
    data = args[args.index("--data-dir") + 1]
    with open(os.path.join(data, "token"), "w") as f:
        f.write(os.environ["FAKE_BY_TOKEN"] + "\\n")
    if os.environ.get("FAKE_BY_FAIL"):
        print("by serve: boom", file=sys.stderr)
        sys.exit(3)
    print("listening on " + os.environ["FAKE_BY_URL"], flush=True)
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    while True:
        time.sleep(1)
answers = json.loads(os.environ.get("FAKE_BY_ANSWERS", "{{}}"))
# By the command and its first argument after `--json` ("send ghost"),
# else by the command alone.
answer = answers.get(" ".join(args[0:1] + args[2:3])) if len(args) > 2 else None
if answer is None and args:
    answer = answers.get(args[0])
if answer is None:
    print("by: no such command", file=sys.stderr)
    sys.exit(2)
print(json.dumps(answer["body"]))
sys.exit(answer.get("code", 0))
"""


class LocalMode(ServerCase):
    """`Client.local` and `Local` over a fake `by`: a script that starts a
    "server" (this case's fake, by URL), answers `up`, `doctor` and
    `harnesses` as scripted, and records every call."""

    def setUp(self) -> None:
        super().setUp()
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.by = self.root / "by"
        self.by.write_text(FAKE_BY)
        self.by.chmod(0o755)
        self.log = self.root / "calls.log"
        self.env = mock.patch.dict(
            os.environ,
            {"FAKE_BY_LOG": str(self.log), "FAKE_BY_TOKEN": TOKEN, "FAKE_BY_URL": self.url},
        )
        self.env.start()

    def tearDown(self) -> None:
        self.env.stop()
        self.tmp.cleanup()
        super().tearDown()

    def calls(self) -> list[list[str]]:
        text = self.log.read_text() if self.log.exists() else ""
        return [json.loads(line) for line in text.splitlines() if line]

    def test_local_starts_by_serve_reads_its_token_and_stops_it(self):
        self.fake.answer("GET", "/v1/repos", (200, {"repos": [{"name": "app", "root": str(self.root)}]}))
        with bc.Client.local(str(self.root), by=str(self.by), name="app", args=["--allow-delegation"]) as client:
            self.assertEqual(client.url, self.url)
            self.assertEqual(client.token, TOKEN)
            self.assertEqual(client.local_name, "app")
            self.assertIsNotNone(client.server)
            self.assertIsNone(client.server.poll(), "the server runs while the client is open")
            self.assertTrue(client.server_dir and Path(client.server_dir).is_dir())
            self.assertEqual([r.name for r in client.repos()], ["app"])
            self.assertEqual(self.requests("GET", "/v1/repos")[0][2].get("authorization"), f"Bearer {TOKEN}")
            data_dir = client.server_dir
            process = client.server
        self.assertIsNotNone(process.poll(), "closed: the server is stopped")
        self.assertFalse(Path(data_dir).exists(), "closed: its directory is gone")
        serve = self.calls()[0]
        self.assertEqual(serve[:6], ["serve", "--listen", "127.0.0.1:0", "--repo", f"app={self.root}", "--data-dir"])
        self.assertEqual(serve[6], data_dir)
        self.assertEqual(serve[7:], ["--quiet", "--allow-delegation"])
        # A plain client has nothing to close.
        plain = bc.Client(self.url, TOKEN)
        plain.close()
        self.assertIsNone(plain.server)

    def test_local_reports_a_server_that_does_not_start_with_what_it_said(self):
        with mock.patch.dict(os.environ, {"FAKE_BY_FAIL": "1"}), self.assertRaises(bc.LocalError) as raised:
            bc.Client.local(str(self.root), by=str(self.by))
        self.assertEqual(raised.exception.code, "local")
        self.assertIn("boom", str(raised.exception))
        self.assertIn("did not start", str(raised.exception))
        with self.assertRaises(bc.LocalError) as raised:
            bc.Client.local(str(self.root), by=str(self.root / "no-such-by"))
        self.assertIn("could not run", str(raised.exception))

    def test_a_missing_by_says_how_to_get_one(self):
        # `by` is neither given nor in BRANCHYARD_BY, and no directory of PATH has it.
        empty = str(self.root / "empty-path")
        os.mkdir(empty)
        with mock.patch.dict(os.environ, {"PATH": empty}):
            os.environ.pop("BRANCHYARD_BY", None)
            for attempt in (
                lambda: bc.Client.local(str(self.root)),
                lambda: bc.Local(str(self.root)).doctor(),
            ):
                with self.assertRaises(bc.LocalError) as raised:
                    attempt()
                message = raised.exception.message
                self.assertIn("could not run by", message)
                self.assertIn("`by` is not on PATH (or set BRANCHYARD_BY)", message)
                self.assertIn("to fix: install released binaries with `curl -fsSL", message)
                self.assertIn(
                    "https://github.com/vamsiramakrishnan/branchyard/releases/latest/download/install.sh "
                    "| sh -s -- --no-setup`",
                    message,
                )
                self.assertIn(
                    "or build from a checkout with `cargo install --locked --path crates/branchyard-cli`", message
                )
                self.assertNotIn("no release is published yet", message)
        # A path that does not exist is not a `by` that is missing from PATH.
        with self.assertRaises(bc.LocalError) as raised:
            bc.Local(str(self.root), by=str(self.root / "no-such-by")).doctor()
        self.assertNotIn("not on PATH", raised.exception.message)
        self.assertIn("to fix: point BRANCHYARD_BY", raised.exception.message)
        # One that cannot be executed says to make it so.
        plain = self.root / "plain-file"
        plain.write_text("not a program")
        with self.assertRaises(bc.LocalError) as raised:
            bc.Local(str(self.root), by=str(plain)).doctor()
        self.assertIn("to fix: make it executable", raised.exception.message)

    def test_local_runs_up_doctor_and_harnesses_with_json(self):
        plan = {
            "steps": [
                {"name": "store", "state": "ready", "detail": "/r/.branchyard"},
                {
                    "name": "harness.codex",
                    "state": "planned",
                    "detail": "not installed",
                    "action": {"kind": "install", "plan": {"harness": "codex", "command": "npm i -g @openai/codex"}},
                },
                {"name": "login.codex", "state": "needs_person", "detail": "not logged in", "todo": "codex login"},
            ],
            "state": "needs_person",
            "ready": False,
        }
        report = {
            "checks": [
                {"name": "repository.git", "status": "ok", "detail": "main", "fix": None},
                {"name": "config.project", "status": "fail", "detail": "line 3", "fix": "fix it"},
            ],
            "status": "fail",
        }
        inventory = {"harnesses": [{"id": "codex", "version": "0.157.1", "login": {"state": "logged_in"}}], "tools": {}}
        answers = {
            "up": {"body": plan, "code": 1},
            "doctor": {"body": report, "code": 1},
            "harnesses": {"body": inventory},
        }
        with mock.patch.dict(os.environ, {"FAKE_BY_ANSWERS": json.dumps(answers)}):
            local = bc.Local(str(self.root), by=str(self.by))
            got = local.up(plan=True, yes=True)
            self.assertIsInstance(got, bc.Plan)
            self.assertFalse(got.ready)
            self.assertEqual(got.state, "needs_person")
            self.assertEqual([s.name for s in got.steps], ["store", "harness.codex", "login.codex"])
            self.assertEqual([s.name for s in got.todo], ["harness.codex", "login.codex"])
            self.assertEqual(got.step("harness.codex").action["kind"], "install")
            self.assertTrue(got.step("store").ready)
            self.assertIsNone(got.step("nope"))
            doctor = local.doctor()
            self.assertEqual(doctor.status, "fail")
            self.assertEqual([c.name for c in doctor.failed], ["config.project"])
            self.assertEqual(doctor.check("repository.git").detail, "main")
            found = local.harnesses(refresh=True)
            self.assertEqual(found.harnesses[0]["id"], "codex")
            code, raw = local.run("up")
            self.assertEqual((code, raw["ready"]), (1, False))
        self.assertEqual(
            self.calls(),
            [
                ["up", "--plan", "--yes", "--json"],
                ["doctor", "--json"],
                ["harnesses", "--refresh", "--json"],
                ["up", "--json"],
            ],
        )

    def test_local_raises_bys_own_error_and_a_missing_answer(self):
        refused = {"error": {"kind": "config", "message": "branchyard.toml:3: unknown key"}}
        answers = {"up": {"body": refused, "code": 1}}
        with mock.patch.dict(os.environ, {"FAKE_BY_ANSWERS": json.dumps(answers)}):
            local = bc.Local(str(self.root), by=str(self.by))
            with self.assertRaises(bc.BranchyardError) as raised:
                local.up()
            self.assertEqual(raised.exception.code, "config")
            self.assertIn("unknown key", raised.exception.message)
            with self.assertRaises(bc.LocalError) as failed:
                local.doctor()
            self.assertIn("no such command", str(failed.exception))
            self.assertIn("exited 2", str(failed.exception))


# ---------------------------------------------------------------------------
# One API inside and outside a harness: `connect()` over the fake `by` and
# over the fake server, running the same script.

KID_INFO = {
    "name": "kid",
    "git_branch": "by/kid",
    "harness": "codex",
    "profile": "codex",
    "base": "main",
    "depth": 1,
    "status": {"state": "ready"},
    "budget": {"max_usd": 0.5},
}
KID_INSPECTION = {
    "name": "kid",
    "status": {"state": "ready"},
    "harness": "codex",
    "parent": "lead",
    "children": [],
    "result_contract": {"schema": {"type": "object"}, "required": False},
    "result": {"files": ["parser.rs"]},
}
MERGED = {"branch": "kid", "target": "by/lead", "previous": "c0", "commit": "c1"}
REPORTED_MESSAGE = {"id": 7, "from": "lead", "to": "root", "kind": "report", "text": "done", "at_ms": 5}
REPORTED_STATUS = {"report": {"state": "complete", "summary": "parser fixed", "at_ms": 6}}
CHECKS = {"checks": [{"check": ["cargo", "test"], "branches": ["flaky"], "outcome": "failed"}]}
REFUSALS = {
    "ghost": ("unknown_branch", "no branch named ghost", None),
    "main": ("denied", "main is not a descendant of lead", None),
    "flaky": ("check_failed", "flaky's check failed", CHECKS),
}


def one_script(by: bc.Delegator) -> list[Any]:
    """What a meta-harness does, written once: spawn, wait, integrate,
    report, and three refusals, with what each call returned."""
    kid = by.spawn("Fix the parser test", harness="codex", name="kid", budget_usd=0.5, check=["cargo", "test"])
    done = by.wait(kid.name)
    merged = by.integrate(kid.name)
    sent = by.report("done", result={"files": done.result["files"]})
    status = by.report_status("complete", "parser fixed")
    seen: list[Any] = [
        [type(r).__name__ for r in (kid, done, merged, sent, status)],
        [kid.name, kid.state, done.state, done.result, merged.commit, sent.text, sent["from"], status.report.state],
    ]
    for call in (lambda: by.send("ghost", "hello"), lambda: by.inspect("main"), lambda: by.integrate("flaky")):
        try:
            call()
        except bc.BranchyardError as error:
            checks = error.checks if isinstance(error, bc.CheckFailedError) else None
            seen.append((type(error).__name__, error.code, error.kind, error.message, checks))
    return seen


class OneApi(ServerCase):
    """`connect()` runs `one_script` inside a harness, through the fake `by`
    as `branchyard.py` runs it, and outside, over HTTP on the fake server:
    the same results, the same exceptions."""

    def setUp(self) -> None:
        super().setUp()
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.by = root / "by"
        self.by.write_text(FAKE_BY)
        self.by.chmod(0o755)
        self.log = root / "calls.log"

    def tearDown(self) -> None:
        self.tmp.cleanup()
        super().tearDown()

    def test_operator_session_patch_reaches_http_and_is_refused_inside_a_harness(self):
        queued = self.op("patch-one-api", "queued")
        self.fake.answer("POST", f"{REPO}/branches/b/send", (202, queued))
        self.fake.answer(
            "GET",
            "/v1/operations/patch-one-api",
            (200, {**queued, "state": "succeeded", "result": {"branches": [{**KID_INFO, "name": "b"}]}}),
        )
        outside = bc.connect(self.app, branch="lead")
        patch = {"effort": "low"}
        self.assertEqual(outside.send("b", "more", session_patch=patch).name, "b")
        body = self.requests("POST", f"{REPO}/branches/b/send")[0][3]
        self.assertEqual(body["session_patch"], patch)
        with mock.patch.dict(os.environ, {"BRANCHYARD_DELEGATION": "token"}):
            inside = bc.connect()
            with self.assertRaises(bc.DeniedError):
                inside.send("b", "more", session_patch=patch)
        with mock.patch.object(bm, "_run") as run:
            with self.assertRaisesRegex(TypeError, "unexpected keyword argument 'session_patch'"):
                bm.send("b", "more", session_patch=patch)
            run.assert_not_called()

    def inside(self) -> list[Any]:
        answers: dict[str, Any] = {
            "spawn": {"body": KID_INFO},
            "wait": {"body": {"settled": [KID_INSPECTION], "pending": []}},
            "integrate kid": {"body": MERGED},
            "report": {"body": REPORTED_MESSAGE},
            "report-status": {"body": REPORTED_STATUS},
        }
        for branch, (kind, message, detail) in REFUSALS.items():
            command = {"ghost": "send", "main": "inspect", "flaky": "integrate"}[branch]
            error = {"kind": kind, "message": message, **({"detail": detail} if detail else {})}
            answers[f"{command} {branch}"] = {"body": {"error": error}, "code": 1}
        env = {
            "BRANCHYARD_BY": str(self.by),
            "BRANCHYARD_DELEGATION": "token",
            "FAKE_BY_LOG": str(self.log),
            "FAKE_BY_ANSWERS": json.dumps(answers),
        }
        with mock.patch.dict(os.environ, env):
            os.environ.pop("BRANCHYARD_REMOTE", None)
            by = bc.connect()
            self.assertTrue(by.inside)
            return one_script(by)

    def outside(self) -> list[Any]:
        spawned = self.op("op1", "queued", "kid", kind="spawn")
        self.fake.answer("POST", f"{REPO}/branches/lead/spawn", (202, spawned))
        result = {"branches": [KID_INFO], "inspection": KID_INSPECTION}
        self.fake.answer("GET", "/v1/operations/op1", (200, {**spawned, "state": "succeeded", "result": result}))
        self.fake.answer("POST", f"{REPO}/wait", (200, {"settled": [KID_INSPECTION], "pending": []}))
        merging = self.op("op2", "queued", "kid", kind="integrate")
        self.fake.answer("POST", f"{REPO}/branches/kid/integrate", (202, merging))
        merged = {**merging, "state": "succeeded", "result": {"branches": [KID_INFO], "merged": MERGED}}
        self.fake.answer("GET", "/v1/operations/op2", (200, merged))
        self.fake.answer("POST", f"{REPO}/branches/lead/report", (200, REPORTED_MESSAGE))
        self.fake.answer("POST", f"{REPO}/branches/lead/report-status", (200, REPORTED_STATUS))
        code, message, _ = REFUSALS["ghost"]
        self.fake.answer("POST", f"{REPO}/branches/ghost/send", (404, _error(code, message)))
        code, message, _ = REFUSALS["main"]
        self.fake.answer("GET", f"{REPO}/branches/main/inspection", (403, _error(code, message)))
        # A failed check is the integrate operation's failure over HTTP.
        failing = self.op("op3", "queued", "flaky", kind="integrate")
        self.fake.answer("POST", f"{REPO}/branches/flaky/integrate", (202, failing))
        code, message, detail = REFUSALS["flaky"]
        failed = {**failing, "state": "failed", "error": {"code": code, "message": message, "detail": detail}}
        self.fake.answer("GET", "/v1/operations/op3", (200, failed))
        by = bc.connect(self.app, branch="lead")
        self.assertFalse(by.inside)
        return one_script(by)

    def test_the_same_script_gives_the_same_results_and_errors_inside_and_outside(self):
        inside = self.inside()
        outside = self.outside()
        self.assertEqual(inside, outside)
        self.assertEqual(
            inside[0], ["BranchInfo", "Inspection", "Record", "Message", "Reported"], "the same Record types"
        )
        self.assertEqual(
            inside[1], ["kid", "ready", "ready", {"files": ["parser.rs"]}, "c1", "done", "lead", "complete"]
        )
        self.assertEqual(
            inside[2:],
            [
                ("NotFoundError", "unknown_branch", "unknown_branch", "no branch named ghost", None),
                ("DeniedError", "denied", "denied", "main is not a descendant of lead", None),
                ("CheckFailedError", "check_failed", "check_failed", "flaky's check failed", CHECKS["checks"]),
            ],
        )
        # Each backend was asked the same thing in its own terms.
        calls = [json.loads(line) for line in self.log.read_text().splitlines()]
        spawn = next(c for c in calls if c[0] == "spawn")
        self.assertEqual(spawn[spawn.index("--check") + 1], "cargo test")
        self.assertEqual(spawn[spawn.index("--name") + 1], "kid")
        ((_, _, _, body),) = self.requests("POST", f"{REPO}/branches/lead/spawn")
        self.assertEqual((body["check"], body["name"], body["budget"]), (["cargo", "test"], "kid", {"max_usd": 0.5}))
        ((_, _, _, report),) = [r for r in self.fake.requests if r[1] == f"{REPO}/branches/lead/report"]
        self.assertEqual(report, {"text": "done", "result": {"files": ["parser.rs"]}})

    def test_handoff_routes_inside_and_outside_without_forcing_an_acting_branch(self):
        answer = {**HANDOFF_MESSAGE, "future_metadata": {"kept": True}}
        expected = HANDOFF_MESSAGE["handoff"]["version"]
        self.fake.answer("POST", f"{REPO}/branches/source/handoff", (200, answer))
        env = {
            "BRANCHYARD_BY": str(self.by),
            "BRANCHYARD_DELEGATION": "token",
            "FAKE_BY_LOG": str(self.log),
            "FAKE_BY_ANSWERS": json.dumps({"handoff": {"body": answer}}),
        }
        with mock.patch.dict(os.environ, env):
            os.environ.pop("BRANCHYARD_REMOTE", None)
            os.environ.pop("BRANCHYARD_BRANCH", None)
            inside = bc.connect()
            outside = bc.connect(self.app)
            self.assertTrue(inside.inside)
            self.assertFalse(outside.inside)
            self.assertIsNone(outside.branch)
            local = inside.handoff("source", "consumer", ["art7"], "Use these sources", expected)
            remote = outside.handoff("source", "consumer", ["art7"], "Use these sources", expected)
        self.assertIsInstance(local, bc.Message)
        self.assertEqual(local.raw, answer)
        self.assertEqual(remote.raw, answer)
        self.assertEqual(local.handoff, remote.handoff)
        self.assertTrue(local.next_turn and remote.next_turn)
        calls = [json.loads(line) for line in self.log.read_text().splitlines()]
        self.assertEqual(
            calls,
            [
                [
                    "handoff",
                    "--json",
                    "--to",
                    "consumer",
                    "--artifact",
                    "art7",
                    "--note",
                    "Use these sources",
                    "--expected-version",
                    json.dumps(expected),
                    "--",
                    "source",
                ]
            ],
        )
        self.assertEqual(len(self.fake.requests), 1, "no implicit share, turn, send or discovery")
        method, path, _, body = self.fake.requests[0]
        self.assertEqual((method, path), ("POST", f"{REPO}/branches/source/handoff"))
        self.assertEqual(
            body,
            {
                "from": "source",
                "to": "consumer",
                "artifacts": ["art7"],
                "note": "Use these sources",
                "expected": expected,
            },
        )

    def test_every_delegation_call_has_branchyard_pys_signature(self):
        calls = [
            "spawn",
            "send",
            "steer",
            "integrate",
            "check",
            "wait",
            "wait_any",
            "wait_all",
            "inspect",
            "events",
            "report",
            "report_status",
            "handoff",
        ]
        for name in calls:
            theirs = inspect.signature(getattr(bm, name)).parameters.values()
            ours = list(inspect.signature(getattr(bc.Delegator, name)).parameters.values())[1:]
            # `**options` is the backend's own spawn: not part of the portable signature.
            ours = [p for p in ours if p.kind is not inspect.Parameter.VAR_KEYWORD]
            if name == "send":
                # The operator facade can patch a session; the harness-only API cannot.
                patch = [p for p in ours if p.name == "session_patch"]
                self.assertEqual(
                    [(p.name, p.kind, p.default) for p in patch],
                    [("session_patch", inspect.Parameter.KEYWORD_ONLY, None)],
                )
                ours = [p for p in ours if p.name != "session_patch"]
            self.assertEqual(
                [(p.name, p.kind, p.default) for p in ours], [(p.name, p.kind, p.default) for p in theirs], name
            )
        # A new call on one side must be on the other, or named here as not portable.
        shared = {n for n in vars(bc.Delegator) if not n.startswith("_") and hasattr(bm, n)}
        self.assertEqual(shared - set(calls), set(), "a Delegator call with a branchyard.py twin the test skips")

    # Every keyword `spawn` takes, with a value each side must carry.
    SPAWN_ALL: ClassVar[dict[str, Any]] = {
        "harness": "codex",
        "name": "parser",
        "base": "main",
        "budget_usd": 0.5,
        "max_turns": 7,
        "max_minutes": 2.5,
        "check": "cargo test --lib",
        "max_depth": 2,
        "max_children": 3,
        "harnesses": ["codex", "gemini-cli"],
        "deny": ["Bash(rm:*)"],
        "seat": "builder",
        "depends_on": ["lint"],
        "after": "integrated",
        "bindings": {"cache": "read_only", "out": "rw"},
        "connectors": ["github:read:issues.list", bc.Grant.parse("linear:read")],
        "plan": True,
        "model": "small",
        "result": {"schema": {"type": "object"}, "required": True},
        "why": "mechanical",
        "owns": ["src/parser/**", "tests/parser_*.rs"],
        "land": True,
        "review": {"harness": "codex", "rubric": "be strict", "budget_usd": 0.1},
    }

    def test_spawn_carries_every_portable_keyword_inside(self):
        answers = {"spawn": {"body": KID_INFO}}
        env = {
            "BRANCHYARD_BY": str(self.by),
            "BRANCHYARD_DELEGATION": "token",
            "FAKE_BY_LOG": str(self.log),
            "FAKE_BY_ANSWERS": json.dumps(answers),
        }
        with mock.patch.dict(os.environ, env):
            os.environ.pop("BRANCHYARD_REMOTE", None)
            kid = bc.connect().spawn("Fix the parser", **self.SPAWN_ALL)
        self.assertEqual(kid.name, "kid")
        (call,) = [json.loads(line) for line in self.log.read_text().splitlines()]

        def values(flag: str) -> list[str]:
            return [call[i + 1] for i, word in enumerate(call) if word == flag]

        self.assertEqual(call[:2], ["spawn", "--json"])
        expected = {
            "--harness": ["codex"],
            "--name": ["parser"],
            "--base": ["main"],
            "--budget-usd": ["0.5"],
            "--max-turns": ["7"],
            "--max-minutes": ["2.5"],
            "--check": ["cargo test --lib"],
            "--max-depth": ["2"],
            "--max-children": ["3"],
            "--harnesses": ["codex,gemini-cli"],
            "--deny": ["Bash(rm:*)"],
            "--seat": ["builder"],
            "--depends-on": ["lint"],
            "--after": ["integrated"],
            "--bind": ["cache:read_only", "out:rw"],
            "--connector": ["github:read:issues.list", "linear:read"],
            "--model": ["small"],
            "--why": ["mechanical"],
            "--owns": ["src/parser/**", "tests/parser_*.rs"],
            "--review": ["codex"],
            "--review-rubric": ["be strict"],
            "--review-budget-usd": ["0.1"],
        }
        for flag, want in expected.items():
            self.assertEqual(values(flag), want, flag)
        self.assertIn("--plan", call)
        self.assertIn("--land", call)
        self.assertEqual(json.loads(values("--result-schema")[0]), {"type": "object"})
        self.assertIn("--result-required", call)

    def test_spawn_carries_every_portable_keyword_outside(self):
        spawned = self.op("op1", "queued", "parser", kind="spawn")
        self.fake.answer("POST", f"{REPO}/branches/lead/spawn", (202, spawned))
        result = {"branches": [KID_INFO]}
        self.fake.answer("GET", "/v1/operations/op1", (200, {**spawned, "state": "succeeded", "result": result}))
        kid = bc.connect(self.app, branch="lead").spawn("Fix the parser", **self.SPAWN_ALL)
        self.assertEqual(kid.name, "kid")
        ((_, _, _, body),) = self.requests("POST", f"{REPO}/branches/lead/spawn")
        self.assertEqual(
            body,
            {
                "prompt": "Fix the parser",
                "harness": "codex",
                "name": "parser",
                "base": "main",
                "budget": {"max_usd": 0.5, "max_turns": 7, "max_seconds": 150.0},
                "check": ["cargo", "test", "--lib"],
                "connectors": [
                    {"connector": "github", "operations": ["issues.list"], "mode": "read"},
                    {"connector": "linear", "operations": ["*"], "mode": "read"},
                ],
                "max_depth": 2,
                "max_children": 3,
                "deny": ["Bash(rm:*)"],
                "seat": "builder",
                "depends_on": ["lint"],
                "after": "integrated",
                "bindings": [
                    {"scratch": "cache", "access": "read_only"},
                    {"scratch": "out", "access": "exclusive_write"},
                ],
                "harnesses": ["codex", "gemini-cli"],
                "plan": True,
                "model": "small",
                "result": {"schema": {"type": "object"}, "required": True},
                "why": "mechanical",
                "owns": ["src/parser/**", "tests/parser_*.rs"],
                "land": True,
                "review": {"harness": "codex", "rubric": "be strict", "budget_usd": 0.1},
            },
        )

    def test_a_reviewer_is_a_harness_or_a_dict_on_both_sides_and_anything_else_is_refused(self):
        self.assertEqual(bc._reviewer("codex"), {"harness": "codex"})
        self.assertEqual(bc._reviewer(None), None)
        for bad in ({"model": "x"}, {"harness": "codex", "color": "red"}):
            with self.assertRaises(ValueError):
                bc._reviewer(bad)
            with self.assertRaises(ValueError):
                bm._review(bad)
        self.assertEqual(
            bc._reviewer({"harness": "codex", "model": "m"}), bm._review({"harness": "codex", "model": "m"})
        )

    def test_a_failed_operation_keeps_the_operation_on_its_error(self):
        failing = self.op("op3", "queued", "flaky", kind="integrate")
        self.fake.answer("POST", f"{REPO}/branches/flaky/integrate", (202, failing))
        failed = {**failing, "state": "failed", "error": {"code": "check_timed_out", "message": "slow"}}
        self.fake.answer("GET", "/v1/operations/op3", (200, failed))
        with self.assertRaises(bc.CheckTimedOutError) as raised:
            bc.connect(self.app, branch="lead").integrate("flaky")
        self.assertIsInstance(raised.exception, bc.CheckFailedError)
        self.assertEqual(raised.exception.operation.id, "op3")

    def test_outside_a_call_that_acts_as_a_branch_needs_one(self):
        with mock.patch.dict(os.environ):
            os.environ.pop("BRANCHYARD_BRANCH", None)
            by = bc.connect(self.app)
            with self.assertRaises(bc.InvalidRequestError):
                by.report("done")
        self.assertEqual(self.fake.requests, [])

    def test_connect_finds_the_server_and_its_only_repo_from_the_environment(self):
        token = Path(self.tmp.name) / "token"
        token.write_text(TOKEN + "\n")
        self.fake.answer("GET", "/v1/repos", (200, {"repos": [{"name": "app", "root": "/r"}]}))
        env = {"BRANCHYARD_REMOTE": self.url, "BRANCHYARD_TOKEN_FILE": str(token), "BRANCHYARD_BRANCH": "lead"}
        with mock.patch.dict(os.environ, env):
            for name in ("BRANCHYARD_DELEGATION", "BRANCHYARD_REPO", "BRANCHYARD_CA_FILE"):
                os.environ.pop(name, None)
            by = bc.connect()
        self.assertFalse(by.inside)
        self.assertEqual((by.repo.name if by.repo else None, by.branch), ("app", "lead"))


class OneErrorHierarchy(unittest.TestCase):
    """Both modules raise one set of classes, each with `code` and `kind`."""

    def test_the_modules_share_their_error_classes(self):
        for name in (
            "BranchyardError",
            "DeniedError",
            "NotFoundError",
            "RunningError",
            "NotRunningError",
            "SteerRefusedError",
            "CheckFailedError",
            "CheckTimedOutError",
            "CheckNotStartedError",
            "StaleRevisionError",
            "ConflictError",
            "InvalidRequestError",
            "UnsupportedError",
        ):
            self.assertIs(getattr(bm, name), getattr(bc, name), name)
        error = bc.error_for_code("stale_revision", "moved", {"expected": 1, "actual": 2})
        self.assertIsInstance(error, bc.StaleRevisionError)
        self.assertIsInstance(error, bc.ConflictError)
        self.assertEqual((error.code, error.kind, error.detail["actual"]), ("stale_revision", "stale_revision", 2))
        for code, cls in (
            ("unsupported", bc.UnsupportedError),
            ("not_running", bc.NotRunningError),
            ("check_not_started", bc.CheckNotStartedError),
            ("git", bc.ServerError),
            ("whatever", bc.BranchyardError),
        ):
            self.assertIs(type(bc.error_for_code(code, "m")), cls, code)

    def test_branchyard_py_argument_checks_raise_them_too(self):
        with self.assertRaises(bc.DeniedError) as refused:
            bm.send("kid")
        self.assertEqual((refused.exception.kind, refused.exception.code), ("denied", "denied"))
        self.assertEqual(refused.exception.detail, {})


# Every route the server registers (`docs/facts.json`, generated from the
# server's sources by `tools/docs_facts.py`) is either reached by a method of
# the HTTP client, called here, or excused with the reason it is not.
COVERED: dict[str, list[Callable[[bc.Client, bc.Repo], Any]]] = {
    "/v1/projects": [lambda c, _r: c.projects(), lambda c, _r: c.create_project("Research")],
    "/v1/projects/{project}": [lambda c, _r: c.project("p1")],
    "/v1/workspaces": [lambda c, _r: c.workspaces(), lambda c, _r: c.create_workspace("p1", "Brief")],
    "/v1/workspaces/{workspace}": [lambda c, _r: c.workspace("w1")],
    "/v1/workspaces/{workspace}/sessions": [
        lambda c, _r: c.workspace_sessions("w1"),
        lambda c, _r: c.start_workspace_session("w1", "Draft", "Write", harness="codex"),
    ],
    "/v1/workspaces/{workspace}/sessions/{session}/messages": [
        lambda c, _r: c.workspace_messages("w1", "s1"),
        lambda c, _r: c.send_workspace_message("w1", "s1", "Continue"),
    ],
    "/v1/workspaces/{workspace}/sessions/{session}/cancel": [lambda c, _r: c.cancel_workspace_session("w1", "s1")],
    "/v1/execution-presets": [lambda c, _r: c.execution_presets(), lambda c, _r: c.execution_presets("w1")],
    "/v1/workspaces/{workspace}/actions/{action}": [lambda c, _r: c.workspace_action("w1", "archive", revision=1)],
    "/v1/workspaces/{workspace}/finalize": [lambda c, _r: c.finalize_workspace("w1", ["a1"], revision=1)],
    "/v1/workspaces/{workspace}/members": [lambda c, _r: c.workspace_members("w1")],
    "/v1/workspaces/{workspace}/members/{subject}": [
        lambda c, _r: c.workspace_member("w1", "subject"),
        lambda c, _r: c.set_workspace_member("w1", "subject", "reviewer", revision=1),
        lambda c, _r: c.remove_workspace_member("w1", "subject", revision=1),
    ],
    "/v1/workspaces/{workspace}/comments": [
        lambda c, _r: c.workspace_comments("w1"),
        lambda c, _r: c.add_workspace_comment("w1", "Review", revision=1),
    ],
    "/v1/workspaces/{workspace}/presence": [
        lambda c, _r: c.workspace_presence("w1"),
        lambda c, _r: c.touch_workspace_presence("w1"),
    ],
    "/v1/workspaces/{workspace}/sources": [
        lambda c, _r: c.workspace_sources("w1"),
        lambda c, _r: c.upload_workspace_source("w1", b"source", "Source", revision=1),
    ],
    "/v1/workspaces/{workspace}/sources/{source}/content": [lambda c, _r: c.read_workspace_source("w1", "source")],
    "/v1/workspaces/{workspace}/sources/{source}/preview": [lambda c, _r: c.preview_workspace_source("w1", "source")],
    "/v1/workspaces/{workspace}/sources/{source}/preview/assets/{asset}": [
        lambda c, _r: c.read_workspace_source_preview_asset("w1", "source", "page-1.png")
    ],
    "/v1/workspaces/{workspace}/artifacts": [lambda c, _r: c.workspace_artifacts("w1")],
    "/v1/workspaces/{workspace}/artifacts/{artifact}/content": [lambda c, _r: c.read_workspace_artifact("w1", "a1")],
    "/v1/workspaces/{workspace}/artifacts/{artifact}/preview": [lambda c, _r: c.preview_workspace_artifact("w1", "a1")],
    "/v1/workspaces/{workspace}/artifacts/{artifact}/preview/assets/{asset}": [
        lambda c, _r: c.read_workspace_artifact_preview_asset("w1", "a1", "page-1.png")
    ],
    "/v1/workspaces/{workspace}/sessions/{session}/runtime": [lambda c, _r: c.workspace_runtime("w1", "s1")],
    "/v1/workspaces/{workspace}/sessions/{session}/runtime/actions/{action}": [
        lambda c, _r: c.control_workspace_runtime("w1", "s1", "pause")
    ],
    "/v1/workspaces/{workspace}/sessions/{session}/terminals": [lambda c, _r: c.open_workspace_terminal("w1", "s1")],
    "/v1/workspaces/{workspace}/sessions/{session}/terminals/{terminal}/output": [
        lambda c, _r: c.workspace_terminal_output("w1", "s1", "t1")
    ],
    "/v1/workspaces/{workspace}/sessions/{session}/terminals/{terminal}/input": [
        lambda c, _r: c.write_workspace_terminal("w1", "s1", "t1", b"echo hello\n", sequence=1)
    ],
    "/v1/workspaces/{workspace}/sessions/{session}/terminals/{terminal}/resize": [
        lambda c, _r: c.resize_workspace_terminal("w1", "s1", "t1", 24, 80)
    ],
    "/v1/workspaces/{workspace}/sessions/{session}/terminals/{terminal}/close": [
        lambda c, _r: c.close_workspace_terminal("w1", "s1", "t1")
    ],
    "/v1/workspaces/{workspace}/sessions/{session}/pr": [
        lambda c, _r: c.workspace_pull_request("w1", "s1"),
        lambda c, _r: c.publish_workspace_pull_request("w1", "s1"),
    ],
    "/v1/workspaces/{workspace}/sessions/{session}/pr/refresh": [
        lambda c, _r: c.refresh_workspace_pull_request("w1", "s1")
    ],
    "/healthz": [lambda c, _r: c.healthy()],
    "/v1/app/me": [lambda c, _r: c.me()],
    "/v1/harnesses": [lambda c, _r: c.harnesses()],
    "/v1/operations": [lambda c, _r: c.operation_by_key("k")],
    "/v1/operations/{id}": [lambda c, _r: c.operation("op1")],
    "/v1/repos": [lambda c, _r: c.repos()],
    "/v1/repos/{repo}/approvals": [lambda _c, r: r.approvals()],
    "/v1/repos/{repo}/approvals/{id}/allow": [lambda _c, r: r.allow("ap1")],
    "/v1/repos/{repo}/approvals/{id}/deny": [lambda _c, r: r.deny("ap1")],
    "/v1/repos/{repo}/branches": [lambda _c, r: r.branches()],
    "/v1/repos/{repo}/branches/{branch}": [lambda _c, r: r.branch("b"), lambda _c, r: r.remove("b")],
    "/v1/repos/{repo}/branches/{branch}/answer": [lambda _c, r: r.answer("b", 1, "yes")],
    "/v1/repos/{repo}/branches/{branch}/artifacts": [
        lambda _c, r: r.publish_artifact("b", b"x", "x.txt"),
        lambda _c, r: r.artifacts("b"),
    ],
    "/v1/repos/{repo}/branches/{branch}/artifacts/{id}": [lambda _c, r: r.read_artifact("b", "a1")],
    "/v1/repos/{repo}/branches/{branch}/artifacts/{id}/content": [lambda _c, r: r.read_artifact("b", "a1")],
    "/v1/repos/{repo}/branches/{branch}/artifacts/{id}/preview": [lambda _c, r: r.preview_artifact("b", "a1")],
    "/v1/repos/{repo}/branches/{branch}/artifacts/{id}/preview/assets/{asset}": [
        lambda _c, r: r.read_preview_asset("b", "a1", "page-1.png")
    ],
    "/v1/repos/{repo}/branches/{branch}/artifacts/{id}/share": [lambda _c, r: r.share_artifact("b", "a1", "c")],
    "/v1/repos/{repo}/branches/{branch}/ask": [lambda _c, r: r.ask("b", "?")],
    "/v1/repos/{repo}/branches/{branch}/cancel": [lambda _c, r: r.cancel("b")],
    "/v1/repos/{repo}/branches/{branch}/check": [lambda _c, r: r.check("b")],
    "/v1/repos/{repo}/branches/{branch}/children": [lambda _c, r: r.children("b")],
    "/v1/repos/{repo}/branches/{branch}/diff": [lambda _c, r: r.diff("b")],
    "/v1/repos/{repo}/branches/{branch}/discard": [lambda _c, r: r.discard("b")],
    "/v1/repos/{repo}/branches/{branch}/escalate": [lambda _c, r: r.escalate("b", "help")],
    "/v1/repos/{repo}/branches/{branch}/event-page": [lambda _c, r: r.event_page("b")],
    "/v1/repos/{repo}/branches/{branch}/events": [lambda _c, r: r.events("b"), lambda _c, r: r.said("b")],
    "/v1/repos/{repo}/branches/{branch}/fork": [lambda _c, r: r.fork("b", "go", at=1)],
    "/v1/repos/{repo}/branches/{branch}/graph": [
        lambda _c, r: r.graph("b"),
        lambda _c, r: r.apply_graph("b", 1, []),
    ],
    "/v1/repos/{repo}/branches/{branch}/handoff": [lambda _c, r: r.handoff("b", "c")],
    "/v1/repos/{repo}/branches/{branch}/inbox": [lambda _c, r: r.inbox("b")],
    "/v1/repos/{repo}/branches/{branch}/inspection": [lambda _c, r: r.inspect("b")],
    "/v1/repos/{repo}/branches/{branch}/integrate": [lambda _c, r: r.integrate("b", with_=["c"])],
    "/v1/repos/{repo}/branches/{branch}/merge": [lambda _c, r: r.merge("b")],
    "/v1/repos/{repo}/branches/{branch}/plan": [lambda _c, r: r.plan("b")],
    "/v1/repos/{repo}/branches/{branch}/plan/approve": [lambda _c, r: r.approve_plan("b")],
    "/v1/repos/{repo}/branches/{branch}/plan/reject": [lambda _c, r: r.reject_plan("b", "no")],
    "/v1/repos/{repo}/branches/{branch}/reincarnate": [lambda _c, r: r.reincarnate("b")],
    "/v1/repos/{repo}/branches/{branch}/report": [lambda _c, r: r.report("b", "done")],
    "/v1/repos/{repo}/branches/{branch}/report-status": [lambda _c, r: r.report_status("b", "working", "on it")],
    "/v1/repos/{repo}/branches/{branch}/rewind": [lambda _c, r: r.rewind("b", 1)],
    "/v1/repos/{repo}/branches/{branch}/scratch": [
        lambda _c, r: r.create_scratch("b", "s"),
        lambda _c, r: r.scratch_areas("b"),
    ],
    "/v1/repos/{repo}/branches/{branch}/scratch/{name}/share": [lambda _c, r: r.share_scratch("b", "s", "c")],
    "/v1/repos/{repo}/branches/{branch}/send": [lambda _c, r: r.send("b", "more")],
    "/v1/repos/{repo}/branches/{branch}/spawn": [lambda _c, r: r.spawn("b", "work")],
    "/v1/repos/{repo}/branches/{branch}/steer": [lambda _c, r: r.steer("b", "also")],
    "/v1/repos/{repo}/effects": [lambda _c, r: r.effects()],
    "/v1/repos/{repo}/effects/{id}": [lambda _c, r: r.effect("e1")],
    "/v1/repos/{repo}/effects/{id}/promote": [lambda _c, r: r.promote_effect("e1")],
    "/v1/repos/{repo}/events/stream": [lambda _c, r: list(r.stream(0, max_failures=0))],
    "/v1/repos/{repo}/operations": [lambda _c, r: r.operations()],
    "/v1/repos/{repo}/tasks": [lambda _c, r: r.task("go")],
    "/v1/repos/{repo}/wait": [lambda _c, r: r.wait_for(["b"])],
    "/v1/triggers": [lambda c, _r: c.triggers(), lambda c, _r: c.create_trigger({"name": "t"})],
    "/v1/triggers/{t}": [lambda c, _r: c.trigger("t"), lambda c, _r: c.delete_trigger("t")],
    "/v1/triggers/{t}/disable": [lambda c, _r: c.enable_trigger("t", False)],
    "/v1/triggers/{t}/enable": [lambda c, _r: c.enable_trigger("t")],
    "/v1/triggers/{t}/runs": [lambda c, _r: c.trigger_runs("t")],
    "/v1/triggers/{t}/test": [lambda c, _r: c.test_trigger("t")],
    "/v1/triggers/{t}/secret": [lambda c, _r: c.set_trigger_secret("t")],
    "/.well-known/branchyard": [lambda c, _r: c.well_known()],
    "/v1/inventory": [lambda c, _r: c.inventory()],
    "/v1/services": [lambda c, _r: c.services(), lambda c, _r: c.register_service("model_gateway")],
    "/v1/services/gc": [lambda c, _r: c.reclaim_services()],
    "/v1/services/{id}": [lambda c, _r: c.deregister_service("s1")],
    "/v1/app/push": [lambda c, _r: c.push_info()],
    "/v1/app/push/subscriptions": [
        lambda c, _r: c.subscribe_push("https://push.example/e", "p", "a"),
        lambda c, _r: c.unsubscribe_push("https://push.example/e"),
    ],
    "/v1/app/push/test": [lambda c, _r: c.test_push()],
    "/v1/repos/{repo}/maps": [lambda _c, r: r.map("p", {"a": 1}), lambda _c, r: r.maps()],
    "/v1/repos/{repo}/maps/{name}": [lambda _c, r: r.map_report("m"), lambda _c, r: r.remove_map("m")],
    "/v1/repos/{repo}/maps/{name}/resume": [lambda _c, r: r.resume_map("m")],
    "/v1/repos/{repo}/task-records": [lambda _c, r: r.task_records()],
    "/v1/repos/{repo}/task-records/{task}": [lambda _c, r: r.task_record("t1")],
    "/v1/repos/{repo}/knowledge": [lambda _c, r: r.knowledge(), lambda _c, r: r.add_knowledge("x")],
    "/v1/repos/{repo}/knowledge/export": [lambda _c, r: r.export_knowledge()],
    "/v1/repos/{repo}/knowledge/{id}": [lambda _c, r: r.knowledge_entry(1), lambda _c, r: r.remove_knowledge(1)],
    "/v1/repos/{repo}/knowledge/{id}/adopt": [lambda _c, r: r.adopt_knowledge(1)],
    "/v1/repos/{repo}/knowledge/{id}/edit": [lambda _c, r: r.edit_knowledge(1, text="x")],
    "/v1/repos/{repo}/knowledge/{id}/reject": [lambda _c, r: r.reject_knowledge(1)],
    "/v1/repos/{repo}/branches/{branch}/distill": [lambda _c, r: r.distill("b")],
    "/v1/repos/{repo}/effects/reconcile": [lambda _c, r: r.reconcile_effects()],
    "/v1/repos/{repo}/branches/{branch}/undo": [lambda _c, r: r.undo_plan("b"), lambda _c, r: r.undo("b")],
    "/v1/repos/{repo}/branches/{branch}/scratch/{name}/lock": [lambda _c, r: r.lock_scratch("b", "s")],
    "/v1/repos/{repo}/branches/{branch}/scratch/{name}/unlock": [lambda _c, r: r.unlock_scratch("b", "s")],
    "/v1/repos/{repo}/scratch/{name}/lock": [lambda _c, r: r.scratch_lock_state("s")],
}

NOT_A_CLIENT_ROUTE = "not a client route: neither SDK calls it"

# The routes the Python client has no method for, and why.
EXCUSED: dict[str, str] = {
    "/.well-known/jwks.json": f"{NOT_A_CLIENT_ROUTE}: the gateway's verification keys, read by Anvil",
    "/app": f"{NOT_A_CLIENT_ROUTE}: the companion web app's pages, for a browser",
    "/app/": f"{NOT_A_CLIENT_ROUTE}: the companion web app's pages, for a browser",
    "/app/pair": f"{NOT_A_CLIENT_ROUTE}: the companion's pairing, a phone redeeming a one-time code for "
    "a token of its own; a client already holds one",
    "/app/{file}": f"{NOT_A_CLIENT_ROUTE}: the companion web app's files, for a browser",
    "/metrics": f"{NOT_A_CLIENT_ROUTE}: Prometheus scrapes it",
    "/v1/triggers/{t}/fire": f"{NOT_A_CLIENT_ROUTE}: a webhook sender's, signed rather than bearer-authenticated",
    "/v1/triggers/{t}/fire/{token}": f"{NOT_A_CLIENT_ROUTE}: a webhook sender's, its token in the path",
}


def _route_pattern(route: str) -> str:
    return "".join("[^/]+" if part.startswith("{") else re.escape(part) for part in re.split(r"(\{[^}]+\})", route))


class RouteParity(unittest.TestCase):
    """The Python HTTP client against the server's route list: each route
    is reached by a method, called here against a recorder, or excused."""

    def setUp(self) -> None:
        facts = json.loads((ROOT / "docs/facts.json").read_text())
        self.routes = set(facts["http"]["routes"])

    def test_every_route_is_covered_or_excused_and_nothing_else(self) -> None:
        self.assertFalse(set(COVERED) & set(EXCUSED), "a route both covered and excused")
        self.assertEqual(sorted(self.routes - set(COVERED) - set(EXCUSED)), [], "routes neither covered nor excused")
        self.assertEqual(sorted((set(COVERED) | set(EXCUSED)) - self.routes), [], "routes the server no longer has")
        for route, reason in EXCUSED.items():
            self.assertTrue(reason.startswith(NOT_A_CLIENT_ROUTE), route)

    def test_each_covered_route_is_what_its_method_requests(self) -> None:
        client = bc.Client("http://127.0.0.1:9", TOKEN)
        repo = client.repo("app")
        seen: list[str] = []

        def record(method: str, path: str, body: Any = None, **options: Any) -> Any:
            seen.append(path)
            return (200, {}, b"") if options.get("raw") else {}

        def opened(request: Any, **_options: Any) -> Any:
            seen.append(urlsplit(request.full_url).path)
            raise OSError("recorded")

        with mock.patch.object(client, "request", record), mock.patch.object(bc.urllib.request, "urlopen", opened):
            for route, calls in COVERED.items():
                pattern = re.compile(_route_pattern(route) + "$")
                for call in calls:
                    seen.clear()
                    with contextlib.suppress(bc.TransportError):
                        call(client, repo)
                    self.assertTrue(any(pattern.match(path) for path in seen), f"{route}: requested {seen}")


if __name__ == "__main__":
    unittest.main()
