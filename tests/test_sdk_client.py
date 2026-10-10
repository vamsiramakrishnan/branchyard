"""`sdk/python/branchyard_client.py` against a fake `by serve`: a stdlib
HTTP server scripted per route, so every claim the module makes about the
wire is checked without a real server. The real server is covered by
`crates/branchyard-cli/tests/sdk_client.rs`, which runs
`tests/sdk_client_e2e.py` against `by serve`.

Run: `python tests/test_sdk_client.py`.
"""

import json
import sys
import threading
import time
import unittest
from collections import deque
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.parse import parse_qs, urlsplit

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "sdk" / "python"))

import branchyard_client as bc  # noqa: E402

TOKEN = "secret-token"
REPO = "/v1/repos/app"


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
        if headers.get("authorization") != f"Bearer {TOKEN}":
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

    def test_the_health_check_sends_no_token(self):
        self.assertTrue(self.client.healthy())
        ((_, _, headers, _),) = self.requests("GET", "/healthz")
        self.assertNotIn("authorization", headers)


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

    def test_the_client_refuses_a_bad_url_or_no_token(self):
        with self.assertRaises(ValueError):
            bc.Client("ftp://x", "t")
        with self.assertRaises(ValueError):
            bc.Client("http://x")
        with self.assertRaises(ValueError):
            bc.Client("http://x", "t", ca_file="/dev/null")


class Waiting(ServerCase):
    def test_wait_polls_until_done_and_checks_the_outcome(self):
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
        op = self.client.wait("op1", poll=0.01)
        self.assertTrue(op.done)
        self.assertEqual(op.result_branches[0].state, "ready")
        self.assertTrue(op.result_branches[0].settled)
        self.assertEqual(len(self.requests("GET", "/v1/operations/op1")), 3)
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

    def test_wait_times_out(self):
        self.fake.always("GET", "/v1/operations/op1", 200, self.op("op1", "running"))
        with self.assertRaises(TimeoutError):
            self.client.wait("op1", timeout=0.05, poll=0.01)

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


if __name__ == "__main__":
    unittest.main()
