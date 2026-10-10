"""Branchyard over HTTP, for a service that is not a harness: a Slack or
Discord bot, a web app, a scheduler, a connector's own back end.

`sdk/python/branchyard.py` is for code running *inside* a harness that
Branchyard runs: it shells out to `by` and acts as that harness's branch.
This module is for everything else: it speaks to `by serve`'s API
(`docs/server.md`) with a bearer token, so a third-party surface can start
work, keep a conversation going on one branch, stream what the harness
says, answer approvals, and give or inspect connector grants, all under
the constraints the server enforces.

Standard library only (`urllib`, `json`, `ssl`); Python 3.9 or newer.

    from branchyard_client import Client, Grant

    client = Client("http://127.0.0.1:8421", token_file=".branchyard/server/token")
    app = client.repo("app")

    # One branch per conversation, as a chat assistant keeps a session.
    chat = app.surface("slack", harness="claude-code", budget_usd=5,
                       connectors=[Grant.parse("github:write:issues.*")])
    reply = chat.conversation("C0123").say("Triage the open bugs").wait()
    print(reply.text)

    # What a branch may reach, as stored, and the most a child of its can be given.
    print([str(g) for g in app.inspect("slack-c0123").grants])

Every failure raises a `BranchyardError` subclass carrying the server's
stable error `code` (`docs/server.md#errors`); a connection failure is a
`TransportError`. Every `POST` that creates an operation carries an
`Idempotency-Key`, generated unless given, so a retry with the same key
never starts a second operation.

Not guaranteed: anything the server does not. This module adds no
authority; what a token may do, the server decides.
"""

import contextlib
import dataclasses
import json
import re
import socket
import ssl
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from collections.abc import Iterator, Sequence
from typing import Any, Callable, Optional, Union

__all__ = [
    "Approval",
    "BranchBusyError",
    "BranchEvents",
    "BranchInfo",
    "BranchyardError",
    "CheckFailedError",
    "Client",
    "ConflictError",
    "Conversation",
    "DeniedError",
    "Effect",
    "EventStream",
    "FeedEntry",
    "Grant",
    "IdempotencyKeyReusedError",
    "Inspection",
    "InvalidRequestError",
    "Message",
    "NotAllowedError",
    "NotFoundError",
    "Operation",
    "OperationFailedError",
    "QuotaExceededError",
    "Record",
    "Reply",
    "Repo",
    "RunningError",
    "ServerError",
    "Status",
    "Steer",
    "Surface",
    "TransportError",
    "UnauthorizedError",
    "branch_name",
    "new_key",
]

JSON = dict[str, Any]
Grants = Sequence[Union["Grant", str, JSON]]

# ---------------------------------------------------------------------------
# Errors


class BranchyardError(Exception):
    """A refusal or failure, as the server reported it: `code` is stable
    (`docs/server.md#errors`), `message` is for people, `detail` is the
    code's extra data when it has any, `status` the HTTP status."""

    def __init__(self, code: str, message: str, detail: Optional[JSON] = None, status: int = 0):
        super().__init__(message)
        self.code = code
        self.message = message
        self.detail = detail or {}
        self.status = status

    def __str__(self) -> str:
        return f"{self.code}: {self.message}"


class TransportError(BranchyardError):
    """The server could not be reached, or answered with something that is
    not its API (a proxy page, a cut-off body). `code` is `transport`."""

    def __init__(self, message: str):
        super().__init__("transport", message)


class UnauthorizedError(BranchyardError):
    """`unauthorized`: a missing or wrong bearer token."""


class NotFoundError(BranchyardError):
    """`unknown_repo`, `unknown_branch`, `unknown_operation`,
    `unknown_trigger` or `not_found`: no such thing, or another tenant's."""


class DeniedError(BranchyardError):
    """`denied`, `scope_required` or `repo_not_allowed`: the caller's
    principal, the envelope or the branch's authority refused it."""


class NotAllowedError(BranchyardError):
    """The server's operator did not allow what the request names:
    `connectors_not_configured`, `delegation_not_allowed`,
    `command_not_allowed`, `provider_not_allowed`, `secret_not_allowed`,
    `unapproved_tools_not_allowed`, `precheck_not_allowed`,
    `push_not_enabled`."""


class InvalidRequestError(BranchyardError):
    """`invalid_request`, `invalid_name`, `unknown_harness`,
    `cursor_out_of_range`, `unsupported_media_type`, `body_too_large`."""


class BranchBusyError(BranchyardError):
    """`branch_busy`: another operation holds the branch; `holder` is its
    id. Wait for it (`Client.wait`), steer the running turn instead
    (`Repo.steer`), or skip."""

    @property
    def holder(self) -> Optional[str]:
        value = self.detail.get("holder")
        return value if isinstance(value, str) else None


class RunningError(BranchyardError):
    """`running`: a turn runs on the branch in another engine (a local
    `by`), or a scratch lock is held."""


class ConflictError(BranchyardError):
    """An engine refusal: `branch_exists`, `no_candidate`, `target_moved`,
    `conflict`, `dirty_target`, `already_merged`, `fenced`,
    `detached_head`, `trigger_exists`, `no_plan`, `stale_revision`."""


class CheckFailedError(BranchyardError):
    """`check_failed`, `check_timed_out` or `check_not_started`: a merge or
    integration's check; `detail.output_tail` or `detail.checks`."""


class QuotaExceededError(BranchyardError):
    """`quota_exceeded` or `rate_limited`; `retry_after` seconds when the
    server said."""

    def __init__(self, code: str, message: str, detail: Optional[JSON] = None, status: int = 0, retry_after=None):
        super().__init__(code, message, detail, status)
        self.retry_after: Optional[float] = retry_after


class IdempotencyKeyReusedError(BranchyardError):
    """`idempotency_key_reused`: the same key with a different request;
    `detail.operation` is the operation the key already names."""


class ServerError(BranchyardError):
    """A server-side failure (`internal`, `git_error`, ...) or
    `shutting_down`."""


class OperationFailedError(BranchyardError):
    """An operation ended `failed` or `interrupted`; `operation` is it, and
    `code`/`message` are its error's. Raised by `Client.wait(..., check=True)`
    and `Reply.wait`."""

    def __init__(self, operation: "Operation"):
        error = operation.error or {}
        super().__init__(
            error.get("code", operation.state),
            error.get("message", f"operation {operation.id} {operation.state}"),
            error.get("detail"),
        )
        self.operation = operation


_BY_CODE: dict[str, Callable[..., BranchyardError]] = {
    "unauthorized": UnauthorizedError,
    "not_found": NotFoundError,
    "method_not_allowed": NotFoundError,
    "unknown_repo": NotFoundError,
    "unknown_branch": NotFoundError,
    "unknown_operation": NotFoundError,
    "unknown_trigger": NotFoundError,
    "denied": DeniedError,
    "scope_required": DeniedError,
    "repo_not_allowed": DeniedError,
    "connectors_not_configured": NotAllowedError,
    "delegation_not_allowed": NotAllowedError,
    "command_not_allowed": NotAllowedError,
    "provider_not_allowed": NotAllowedError,
    "secret_not_allowed": NotAllowedError,
    "unapproved_tools_not_allowed": NotAllowedError,
    "precheck_not_allowed": NotAllowedError,
    "push_not_enabled": NotAllowedError,
    "invalid_request": InvalidRequestError,
    "invalid_name": InvalidRequestError,
    "unknown_harness": InvalidRequestError,
    "cursor_out_of_range": InvalidRequestError,
    "unsupported_media_type": InvalidRequestError,
    "body_too_large": InvalidRequestError,
    "invalid_pairing_code": InvalidRequestError,
    "branch_busy": BranchBusyError,
    "running": RunningError,
    "branch_exists": ConflictError,
    "no_candidate": ConflictError,
    "target_moved": ConflictError,
    "conflict": ConflictError,
    "dirty_target": ConflictError,
    "already_merged": ConflictError,
    "fenced": ConflictError,
    "detached_head": ConflictError,
    "trigger_exists": ConflictError,
    "no_plan": ConflictError,
    "stale_revision": ConflictError,
    "check_failed": CheckFailedError,
    "check_timed_out": CheckFailedError,
    "check_not_started": CheckFailedError,
    "quota_exceeded": QuotaExceededError,
    "rate_limited": QuotaExceededError,
    "idempotency_key_reused": IdempotencyKeyReusedError,
}


def error_for(status: int, body: Any, headers: Optional[dict[str, str]] = None) -> BranchyardError:
    """The exception for an error response: by its `code`, else by its
    status (5xx `ServerError`, the rest `BranchyardError`)."""
    error = body.get("error") if isinstance(body, dict) else None
    if not isinstance(error, dict) or "code" not in error:
        return TransportError(f"HTTP {status} without an API error body: {str(body)[:200]}")
    code = str(error["code"])
    message = str(error.get("message", code))
    detail = error.get("detail") if isinstance(error.get("detail"), dict) else None
    cls = _BY_CODE.get(code)
    if cls is QuotaExceededError:
        retry_after = None
        value = (headers or {}).get("retry-after")
        if value and value.strip().isdigit():
            retry_after = float(value)
        return QuotaExceededError(code, message, detail, status, retry_after)
    if cls is None:
        cls = ServerError if status >= 500 else BranchyardError
    return cls(code, message, detail, status)


# ---------------------------------------------------------------------------
# Grants

_CONNECTOR = re.compile(r"^[A-Za-z0-9_-]{1,64}$")
_ACCOUNT = re.compile(r"^[A-Za-z0-9_.-]{1,64}$")
_OPERATION = re.compile(r"^[A-Za-z0-9_.*?/:-]{1,200}$")


@dataclasses.dataclass(frozen=True)
class Grant:
    """One entry of a branch's connector grant (`docs/connectors.md#grants`):
    what its turns may call through the gateway. The flag form is
    `CONNECTOR[@ACCOUNT][:MODE[:OPERATION,...]]`, `MODE` one of `read`
    (the default), `write` or `write+confirm`; the wire form is the object
    `{"connector", "operations", "mode", "confirm"?, "account"?}`. The
    server accepts either form wherever a grant goes.

    A grant needs a home private to the branch (`isolated`, or a sandbox
    provider). A delegated child's grant is only ever narrower than its
    parent's: ask for more and the child gets the intersection.
    """

    connector: str
    operations: tuple[str, ...] = ("*",)
    mode: str = "read"
    confirm: bool = False
    account: Optional[str] = None

    def __post_init__(self) -> None:
        if not _CONNECTOR.match(self.connector):
            raise ValueError(f"connector {self.connector!r} must be 1 to 64 letters, digits, '_' and '-'")
        if self.account is not None and not _ACCOUNT.match(self.account):
            raise ValueError(
                f"connector {self.connector}: account {self.account!r} may hold only letters, digits, '_', '-' and '.'"
            )
        if self.mode not in ("read", "write"):
            raise ValueError(f"connector {self.connector}: mode {self.mode!r} is not read or write")
        if self.confirm and self.mode != "write":
            raise ValueError(f"connector {self.connector}: confirm needs mode write")
        if not self.operations:
            raise ValueError(f"connector {self.connector}: no operations")
        for op in self.operations:
            if not _OPERATION.match(op):
                raise ValueError(f"connector {self.connector}: operation {op!r} is not a valid glob")
        # A tuple, whatever sequence was given.
        object.__setattr__(self, "operations", tuple(self.operations))

    @classmethod
    def read(cls, connector: str, *operations: str, account: Optional[str] = None) -> "Grant":
        """Reads of `operations` (every one when none), as `connector:read:...`."""
        return cls(connector, operations or ("*",), "read", False, account)

    @classmethod
    def write(cls, connector: str, *operations: str, confirm: bool = False, account: Optional[str] = None) -> "Grant":
        """Reads and mutations of `operations`; `confirm` adds `write+confirm`,
        letting the branch's turns confirm effects that ask."""
        return cls(connector, operations or ("*",), "write", confirm, account)

    @classmethod
    def parse(cls, text: str) -> "Grant":
        """The flag form, exactly as `--connector` and the wire read it."""
        head, *rest = text.split(":", 2)
        account: Optional[str] = None
        connector = head
        if "@" in head:
            connector, account = head.split("@", 1)
        mode_text = rest[0] if rest else "read"
        if mode_text == "read":
            mode, confirm = "read", False
        elif mode_text == "write":
            mode, confirm = "write", False
        elif mode_text == "write+confirm":
            mode, confirm = "write", True
        else:
            raise ValueError(f"connector {connector}: mode {mode_text!r} is not read, write or write+confirm")
        operations = tuple(op.strip() for op in rest[1].split(",")) if len(rest) > 1 else ("*",)
        return cls(connector, operations, mode, confirm, account)

    @classmethod
    def from_json(cls, value: Union["Grant", str, JSON]) -> "Grant":
        """A grant from either wire form, or a `Grant` as is."""
        if isinstance(value, Grant):
            return value
        if isinstance(value, str):
            return cls.parse(value)
        if not isinstance(value, dict) or "connector" not in value:
            raise ValueError(f"not a connector grant: {value!r}")
        return cls(
            str(value["connector"]),
            tuple(value.get("operations") or ("*",)),
            str(value.get("mode", "read")),
            value.get("confirm") == "allow",
            value.get("account"),
        )

    def to_json(self) -> JSON:
        """The object form; `confirm` only when allowed, `account` only when set."""
        entry: JSON = {"connector": self.connector, "operations": list(self.operations), "mode": self.mode}
        if self.confirm:
            entry["confirm"] = "allow"
        if self.account is not None:
            entry["account"] = self.account
        return entry

    def __str__(self) -> str:
        text = self.connector
        if self.account is not None:
            text += f"@{self.account}"
        text += ":" + ("write+confirm" if self.confirm else self.mode)
        if self.operations != ("*",):
            text += ":" + ",".join(self.operations)
        return text


def grants_json(grants: Optional[Grants]) -> list[JSON]:
    """`grants` in the object form, each checked client-side first."""
    return [Grant.from_json(g).to_json() for g in grants or ()]


# ---------------------------------------------------------------------------
# Records: the server's JSON, with attribute access


class Record(dict):
    """A server object as a dict whose keys also read as attributes
    (`op.state`, `branch.status`); a missing key reads as `None`, so a
    newer server's fields never break an older client, and an older
    server's absent fields are simply `None`. `raw` is the dict itself."""

    def __getattr__(self, name: str) -> Any:
        if name.startswith("__"):
            raise AttributeError(name)
        return self.get(name)

    @property
    def raw(self) -> JSON:
        return dict(self)


class Status(Record):
    """A branch's status, `{"state": ..., ...}`, comparing equal to its
    state's name: `status == "ready"`."""

    def __eq__(self, other: object) -> bool:
        if isinstance(other, str):
            return self.get("state") == other
        return dict.__eq__(self, other)

    def __ne__(self, other: object) -> bool:
        return not self.__eq__(other)

    __hash__ = None  # type: ignore[assignment]

    def __str__(self) -> str:
        return str(self.get("state", ""))


class BranchInfo(Record):
    """`BranchInfo`: `name`, `git_branch`, `harness`, `profile`, `parent`,
    `children`, `depth`, `base`, `candidate`, `status`, `turns`,
    `cost_usd`, `created_at`, `stalled`, ..."""

    @property
    def status(self) -> Status:
        return Status(self.get("status") or {})

    @property
    def state(self) -> str:
        return str(self.status.get("state", ""))

    @property
    def settled(self) -> bool:
        """Not running, waiting, blocked, waiting on children or awaiting
        plan approval."""
        return self.state not in ("running", "waiting", "blocked", "waiting_on_children", "awaiting_plan_approval")


class Inspection(Record):
    """`Inspection`, as `by inspect --json`: `name`, `status`, `harness`,
    `parent`, `children`, `depth`, `turns`, `cost_usd`, `subtree_cost_usd`,
    `max_usd`, `remaining_usd`, `envelope`, `allowed_harnesses`,
    `last_message`, `check`, `model`, `grants`, ... (`docs/delegation.md`)."""

    @property
    def status(self) -> Status:
        return Status(self.get("status") or {})

    @property
    def state(self) -> str:
        return str(self.status.get("state", ""))

    @property
    def grants(self) -> list[Grant]:
        """Its connector grant as stored: what its turns' tokens carry, and
        the most a child of its can be given. Empty without a grant."""
        return [Grant.from_json(g) for g in self.get("grants") or ()]


class Steer(Record):
    """`Steer`: `id`, `branch`, `by`, `text`, `requested_at_ms`, `state`
    (`{"state": "accepted" | "written" | "pending"}`), `boundary`."""


class Message(Record):
    """`Message`: `id`, `from`/`to` branches, `kind`, `text`, `at_ms`, ..."""


class Approval(Record):
    """`ApprovalAsk`: `id`, `branch`, `turn`, `subject`, `about`
    (`{"kind": "tool" | "operation" | "promote", ...}`), `effect`,
    `request`, `created_ms`, `deadline_ms`, `answer`."""

    @property
    def pending(self) -> bool:
        return self.get("answer") is None


class Effect(Record):
    """`EffectEntry`: `id`, `task`, `branch`, `turn`, `subject`, `connector`,
    `operation`, `account`, `class`, `state`, `undo`, `approval`, ..."""


class FeedEntry(Record):
    """One entry of a repository's feed: `seq`, `branch`, `at_ms`,
    `activity` (an `Activity`, externally tagged: `{"harness": {...}}`,
    `{"prompt": "..."}`, `{"status": {...}}`, ...)."""

    @property
    def harness_event(self) -> Optional[JSON]:
        """The harness event when the activity is one, `{"type": ..., ...}`."""
        activity = self.get("activity")
        event = activity.get("harness") if isinstance(activity, dict) else None
        return event if isinstance(event, dict) else None

    @property
    def text(self) -> Optional[str]:
        """The text of a `message_delta`, what the harness says; else `None`."""
        event = self.harness_event
        if event and event.get("type") == "message_delta":
            return str(event.get("text", ""))
        return None

    @property
    def turn_ended(self) -> bool:
        event = self.harness_event
        return event is not None and event.get("type") == "turn_ended"

    @property
    def status(self) -> Optional[Status]:
        """The branch's new status when the activity is a status change."""
        activity = self.get("activity")
        value = activity.get("status") if isinstance(activity, dict) else None
        return Status(value) if isinstance(value, dict) else None


class BranchEvents(Record):
    """`{"events": [RecordedEvent], "cursor": M}`: pass `cursor` back for
    the events after these."""

    @property
    def events(self) -> list[JSON]:
        return list(self.get("events") or ())


class Operation(Record):
    """An operation (`docs/server.md#operations`): `id`, `repo`, `kind`,
    `state` (`queued`, `running`, `succeeded`, `failed`, `interrupted`),
    `branches`, `cursor` and `end_cursor` (feed positions), `result`
    (`branches`, `descendants`, `merged`, `inspection`), `error`,
    `waiting`, `priority`. `replayed` is true when the server answered a
    repeated idempotency key with the original operation."""

    replayed: bool = False

    @property
    def done(self) -> bool:
        return self.get("state") in ("succeeded", "failed", "interrupted")

    @property
    def failed(self) -> bool:
        return self.get("state") in ("failed", "interrupted")

    @property
    def branch(self) -> Optional[str]:
        """The one branch it works on, when it is one."""
        branches = self.get("branches") or []
        return str(branches[0]) if len(branches) == 1 else None

    @property
    def result_branches(self) -> list[BranchInfo]:
        result = self.get("result") or {}
        return [BranchInfo(b) for b in result.get("branches") or ()]

    @property
    def inspection(self) -> Optional[Inspection]:
        result = self.get("result") or {}
        value = result.get("inspection")
        return Inspection(value) if isinstance(value, dict) else None


def new_key() -> str:
    """A fresh idempotency key. Keep it to retry the same request safely."""
    return uuid.uuid4().hex


_NAME_BAD = re.compile(r"[^a-z0-9_.-]+")


def branch_name(*parts: str, max_len: int = 60) -> str:
    """A branch name from free text (a channel id, a thread, a user), as
    Branchyard accepts one: lowercased, letters, digits, `_`, `.` and `-`
    kept, every other run of characters folded to one `-`, the parts
    joined by `-`, trimmed to `max_len`. Raises `ValueError` when nothing
    is left."""
    pieces = [_NAME_BAD.sub("-", p.lower()).strip("-.") for p in parts if p]
    name = "-".join(p for p in pieces if p)[:max_len].strip("-.")
    if not name:
        raise ValueError(f"no branch name in {parts!r}")
    return name


# ---------------------------------------------------------------------------
# The client


def _drop_none(body: JSON) -> JSON:
    return {k: v for k, v in body.items() if v is not None and v is not False and v != [] and v != {}}


class Client:
    """A connection to one `by serve`: `url` such as `http://127.0.0.1:8421`,
    the bearer `token` or the `token_file` holding it on its first line
    (the server's own is `DATA-DIR/token`), `ca_file` for a server with
    its own certificate authority, `timeout` per request in seconds."""

    def __init__(
        self,
        url: str,
        token: Optional[str] = None,
        *,
        token_file: Optional[str] = None,
        ca_file: Optional[str] = None,
        timeout: float = 30.0,
        user_agent: str = "branchyard-client-python",
    ):
        if token is None and token_file is not None:
            with open(token_file, encoding="utf-8") as f:
                token = f.readline().strip()
        if not token:
            raise ValueError("give a token or a token_file")
        parsed = urllib.parse.urlsplit(url)
        if parsed.scheme not in ("http", "https") or not parsed.netloc:
            raise ValueError(f"url {url!r} is not http:// or https://")
        self.url = url.rstrip("/")
        self.token = token
        self.timeout = timeout
        self.user_agent = user_agent
        self._context: Optional[ssl.SSLContext] = None
        if parsed.scheme == "https":
            self._context = ssl.create_default_context(cafile=ca_file)
        elif ca_file is not None:
            raise ValueError("ca_file needs an https:// url")

    # -- transport ---------------------------------------------------------

    def request(
        self,
        method: str,
        path: str,
        body: Any = None,
        *,
        key: Optional[str] = None,
        query: Optional[dict[str, Any]] = None,
        headers: Optional[dict[str, Optional[str]]] = None,
        raw: bool = False,
        timeout: Optional[float] = None,
    ) -> Any:
        """One request. `body` is sent as JSON (or as bytes when it is
        bytes), `key` as `Idempotency-Key`; a `headers` entry of `None`
        removes a default header. Returns the parsed JSON body, with
        `_replayed` set when `Idempotent-Replayed` came back, or with `raw`
        the tuple `(status, headers, bytes)`. Raises a `BranchyardError` for
        an error status."""
        url = self.url + path
        if query:
            pairs = {k: v for k, v in query.items() if v is not None}
            if pairs:
                url += "?" + urllib.parse.urlencode(pairs)
        data: Optional[bytes] = None
        request_headers = {
            "Authorization": f"Bearer {self.token}",
            "Accept": "application/json",
            "User-Agent": self.user_agent,
        }
        if body is not None:
            if isinstance(body, bytes):
                data = body
                request_headers["Content-Type"] = "application/octet-stream"
            else:
                data = json.dumps(body).encode("utf-8")
                request_headers["Content-Type"] = "application/json"
        if key is not None:
            request_headers["Idempotency-Key"] = key
        for name, value in (headers or {}).items():
            if value is None:
                request_headers.pop(name, None)
            else:
                request_headers[name] = value
        req = urllib.request.Request(url, data=data, method=method, headers=request_headers)
        try:
            with urllib.request.urlopen(req, timeout=timeout or self.timeout, context=self._context) as response:
                content = response.read()
                status = response.status
                response_headers = {k.lower(): v for k, v in response.headers.items()}
        except urllib.error.HTTPError as error:
            content = error.read()
            response_headers = {k.lower(): v for k, v in error.headers.items()}
            raise error_for(error.code, _json_or_text(content), response_headers) from None
        except (urllib.error.URLError, OSError, ssl.SSLError) as error:
            raise TransportError(f"{method} {url}: {error}") from None
        if raw:
            return status, response_headers, content
        parsed = _json_or_text(content) if content else {}
        if isinstance(parsed, dict) and response_headers.get("idempotent-replayed", "").lower() == "true":
            parsed["_replayed"] = True
        return parsed

    def get(self, path: str, **query: Any) -> Any:
        return self.request("GET", path, query=query or None)

    def post(self, path: str, body: Any = None, key: Optional[str] = None, **query: Any) -> Any:
        return self.request("POST", path, body if body is not None else {}, key=key, query=query or None)

    def delete(self, path: str) -> Any:
        return self.request("DELETE", path)

    def _operation(self, path: str, body: JSON, key: Optional[str]) -> "Operation":
        value = self.post(path, _drop_none(body), key=key or new_key())
        replayed = bool(value.pop("_replayed", False))
        op = Operation(value)
        op.replayed = replayed
        return op

    # -- the server --------------------------------------------------------

    def healthy(self) -> bool:
        """`GET /healthz`, without the token."""
        try:
            status, _, _ = self.request("GET", "/healthz", raw=True, headers={"Authorization": None})
        except BranchyardError:
            return False
        return status == 200

    def me(self) -> Record:
        """The caller's own principal: `name`, `tenant`, `scopes`, `repos`."""
        return Record(self.get("/v1/app/me"))

    def repos(self) -> list[Record]:
        """Served repositories, `{"name", "root"}` each."""
        return [Record(r) for r in self.get("/v1/repos").get("repos", [])]

    def harnesses(self) -> list[Record]:
        """Harness profiles as the server finds them: `harness`, `profile`,
        `default`, `available`, `qualification`."""
        return [Record(h) for h in self.get("/v1/harnesses").get("harnesses", [])]

    def repo(self, name: str) -> "Repo":
        return Repo(self, name)

    # -- operations --------------------------------------------------------

    def operation(self, id: str) -> Operation:
        return Operation(self.get(f"/v1/operations/{_q(id)}"))

    def operation_by_key(self, key: str) -> Operation:
        """The operation a request with idempotency `key` created, for a
        client that lost the response."""
        return Operation(self.get("/v1/operations", idempotency_key=key))

    def wait(
        self,
        operation: Union[Operation, str],
        *,
        timeout: Optional[float] = None,
        poll: float = 0.5,
        check: bool = False,
    ) -> Operation:
        """Poll until the operation finishes, or `timeout` seconds pass
        (`TimeoutError`). With `check`, a failed or interrupted operation
        raises `OperationFailedError`."""
        id = operation.id if isinstance(operation, Operation) else operation
        deadline = None if timeout is None else time.monotonic() + timeout
        op = operation if isinstance(operation, Operation) and operation.done else self.operation(id)
        while not op.done:
            if deadline is not None and time.monotonic() >= deadline:
                raise TimeoutError(f"operation {id} still {op.state} after {timeout}s")
            time.sleep(poll)
            op = self.operation(id)
        if check and op.failed:
            raise OperationFailedError(op)
        return op

    # -- triggers ----------------------------------------------------------

    def triggers(self) -> list[Record]:
        return [Record(t) for t in self.get("/v1/triggers").get("triggers", [])]

    def trigger(self, name_or_id: str) -> Record:
        return Record(self.get(f"/v1/triggers/{_q(name_or_id)}"))

    def create_trigger(self, spec: JSON) -> Record:
        """`POST /v1/triggers` with a `TriggerSpec` (`docs/triggers.md`):
        `name`, `repo`, `when`, `task`, and `deliver: {"branch": TEMPLATE,
        "busy": "queue" | "steer" | "skip"}` for a branch that lives on.
        Returns `{"trigger", "secret"?}`."""
        return Record(self.post("/v1/triggers", spec))

    def delete_trigger(self, name_or_id: str) -> None:
        self.delete(f"/v1/triggers/{_q(name_or_id)}")

    def enable_trigger(self, name_or_id: str, enabled: bool = True) -> Record:
        verb = "enable" if enabled else "disable"
        return Record(self.post(f"/v1/triggers/{_q(name_or_id)}/{verb}"))

    def test_trigger(
        self,
        name_or_id: str,
        event: Optional[JSON] = None,
        event_type: Optional[str] = None,
        run_precheck: bool = False,
    ) -> Record:
        body = _drop_none({"event": event, "event_type": event_type, "run_precheck": run_precheck})
        return Record(self.post(f"/v1/triggers/{_q(name_or_id)}/test", body))

    def trigger_runs(self, name_or_id: str, limit: Optional[int] = None) -> list[Record]:
        return [Record(r) for r in self.get(f"/v1/triggers/{_q(name_or_id)}/runs", limit=limit).get("runs", [])]


def _q(part: str) -> str:
    return urllib.parse.quote(part, safe="")


def _json_or_text(content: bytes) -> Any:
    try:
        return json.loads(content.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        return content.decode("utf-8", "replace")


# ---------------------------------------------------------------------------
# A repository


class Repo:
    """One served repository. Every method maps to one route of
    `docs/server.md#api-reference`; those that start work return an
    `Operation` to `Client.wait` on."""

    def __init__(self, client: Client, name: str):
        self.client = client
        self.name = name
        self._base = f"/v1/repos/{_q(name)}"

    def _branch(self, branch: str, suffix: str = "") -> str:
        return f"{self._base}/branches/{_q(branch)}{suffix}"

    # -- starting and continuing work --------------------------------------

    def task(
        self,
        prompt: str,
        *,
        harness: Optional[str] = None,
        harnesses: Optional[Sequence[str]] = None,
        name: Optional[str] = None,
        base: Optional[str] = None,
        budget: Optional[JSON] = None,
        budget_usd: Optional[float] = None,
        policy: Optional[JSON] = None,
        check: Optional[Sequence[str]] = None,
        isolated: Optional[bool] = None,
        connectors: Optional[Grants] = None,
        provision: Optional[JSON] = None,
        delegation: Optional[JSON] = None,
        allow_delegation: bool = False,
        provider: Optional[JSON] = None,
        seats: Optional[JSON] = None,
        priority: Optional[int] = None,
        plan: bool = False,
        goal: Optional[JSON] = None,
        key: Optional[str] = None,
        **extra: Any,
    ) -> Operation:
        """`POST .../tasks`: a branch running `prompt` (`docs/server.md#requests`).
        `connectors` is the branch's grant, in either form; a grant needs a
        private home, so `isolated` defaults to true when one is given.
        `budget_usd` is `budget.max_usd`. `delegation` is the envelope
        `{max_depth, max_children, harnesses}`, with which the harness may
        spawn children, each given at most this branch's grant."""
        body: JSON = {
            "prompt": prompt,
            "harness": harness,
            "harnesses": list(harnesses or ()),
            "name": name,
            "base": base,
            "budget": _budget(budget, budget_usd),
            "policy": policy,
            "check": list(check) if check is not None else None,
            "isolated": bool(connectors) if isolated is None else isolated,
            "provision": _provision(provision, connectors),
            "delegation": delegation,
            "allow_delegation": allow_delegation,
            "provider": provider,
            "seats": seats,
            "priority": priority,
            "plan": plan,
            "goal": goal,
        }
        body.update(extra)
        return self.client._operation(f"{self._base}/tasks", body, key)

    def send(
        self,
        branch: str,
        prompt: Optional[str] = None,
        *,
        retry: bool = False,
        budget: Optional[JSON] = None,
        budget_usd: Optional[float] = None,
        policy: Optional[JSON] = None,
        check: Optional[Sequence[str]] = None,
        connectors: Optional[Grants] = None,
        provision: Optional[JSON] = None,
        delegation: Optional[JSON] = None,
        allow_delegation: bool = False,
        priority: Optional[int] = None,
        key: Optional[str] = None,
        **extra: Any,
    ) -> Operation:
        """`POST .../send`: another turn on `branch`'s session. A send that
        names no `connectors` keeps the branch's grant; one that does is
        narrowed to it for a delegated child. `409 branch_busy`
        (`BranchBusyError`) while another operation holds the branch."""
        body: JSON = {
            "prompt": prompt,
            "retry": retry,
            "budget": _budget(budget, budget_usd),
            "policy": policy,
            "check": list(check) if check is not None else None,
            "provision": _provision(provision, connectors),
            "delegation": delegation,
            "allow_delegation": allow_delegation,
            "priority": priority,
        }
        body.update(extra)
        return self.client._operation(self._branch(branch, "/send"), body, key)

    def steer(self, branch: str, text: str) -> Steer:
        """`POST .../steer`: `text` joins `branch`'s running turn without
        interrupting it. Not an operation; `RunningError`-free, but refused
        (`steer_refused`) when no turn runs or the harness cannot."""
        return Steer(self.client.post(self._branch(branch, "/steer"), {"text": text}))

    def cancel(self, branch: str) -> list[str]:
        """Stop `branch`'s running turn and every turn delegated below it;
        the branches that were running."""
        return list(self.client.post(self._branch(branch, "/cancel"), {}).get("cancelled", []))

    def discard(self, branch: str, reason: Optional[str] = None) -> Inspection:
        return Inspection(self.client.post(self._branch(branch, "/discard"), _drop_none({"reason": reason})))

    def fork(
        self,
        branch: str,
        prompt: str,
        *,
        name: Optional[str] = None,
        fresh_session: bool = False,
        key: Optional[str] = None,
        **options: Any,
    ) -> Operation:
        """A new branch from `branch`'s candidate, continuing its session
        unless `fresh_session`; `options` as `task` takes them (`harness`,
        `budget`, `isolated`, `provision`, ...). The fork keeps its parent's
        grant unless `options["provision"]` gives another."""
        body: JSON = {"prompt": prompt, "name": name, "fresh_session": fresh_session}
        body.update(options)
        return self.client._operation(self._branch(branch, "/fork"), body, key)

    def reincarnate(
        self, branch: str, *, name: Optional[str] = None, key: Optional[str] = None, **options: Any
    ) -> Operation:
        """A new branch from `branch`'s candidate with a fresh session and a
        generated handoff brief: the way a long conversation is compacted."""
        body: JSON = {"name": name}
        body.update(options)
        return self.client._operation(self._branch(branch, "/reincarnate"), body, key)

    def merge(self, branch: str, target: Optional[str] = None, key: Optional[str] = None) -> Operation:
        return self.client._operation(self._branch(branch, "/merge"), {"target": target}, key)

    def spawn(
        self,
        parent: str,
        prompt: str,
        *,
        harness: Optional[str] = None,
        name: Optional[str] = None,
        base: Optional[str] = None,
        budget: Optional[JSON] = None,
        budget_usd: Optional[float] = None,
        policy: Optional[JSON] = None,
        check: Optional[Sequence[str]] = None,
        connectors: Optional[Grants] = None,
        max_depth: Optional[int] = None,
        max_children: Optional[int] = None,
        deny: Optional[Sequence[str]] = None,
        seat: Optional[str] = None,
        depends_on: Optional[Sequence[str]] = None,
        after: Optional[str] = None,
        bindings: Optional[Sequence[JSON]] = None,
        harnesses: Optional[Sequence[str]] = None,
        plan: bool = False,
        model: Optional[str] = None,
        priority: Optional[int] = None,
        key: Optional[str] = None,
        **extra: Any,
    ) -> Operation:
        """`POST .../spawn`: a child of `parent`, as `by spawn --parent`
        makes one; the server must allow delegation. `connectors` asks for
        the child's grant, which is narrowed to `parent`'s: the result's
        `inspection.grants` is what it got."""
        body: JSON = {
            "prompt": prompt,
            "harness": harness,
            "name": name,
            "base": base,
            "budget": _budget(budget, budget_usd),
            "policy": policy,
            "check": list(check) if check is not None else None,
            "connectors": grants_json(connectors) if connectors is not None else None,
            "max_depth": max_depth,
            "max_children": max_children,
            "deny": list(deny or ()),
            "seat": seat,
            "depends_on": list(depends_on or ()),
            "after": after,
            "bindings": list(bindings or ()),
            "harnesses": list(harnesses) if harnesses is not None else None,
            "plan": plan,
            "model": model,
            "priority": priority,
        }
        body.update(extra)
        return self.client._operation(self._branch(parent, "/spawn"), body, key)

    def integrate(self, branch: str, *, with_: Sequence[str] = (), key: Optional[str] = None) -> Operation:
        """Merge delegated `branch` (and `with_` siblings) into the branch
        that delegated it."""
        return self.client._operation(self._branch(branch, "/integrate"), {"with": list(with_)}, key)

    # -- looking -----------------------------------------------------------

    def branches(self) -> list[BranchInfo]:
        return [BranchInfo(b) for b in self.client.get(f"{self._base}/branches").get("branches", [])]

    def branch(self, name: str) -> BranchInfo:
        return BranchInfo(self.client.get(self._branch(name)))

    def exists(self, name: str) -> bool:
        try:
            self.branch(name)
        except NotFoundError as error:
            if error.code == "unknown_branch":
                return False
            raise
        return True

    def remove(self, name: str) -> None:
        self.client.delete(self._branch(name))

    def diff(self, name: str) -> str:
        return str(self.client.get(self._branch(name, "/diff")).get("diff", ""))

    def inspect(self, branch: str) -> Inspection:
        """`branch` as a delegating parent sees it, grant included."""
        return Inspection(self.client.get(self._branch(branch, "/inspection")))

    def children(self, branch: str) -> list[BranchInfo]:
        """Every branch `branch` delegated to, directly or below."""
        return [BranchInfo(b) for b in self.client.get(self._branch(branch, "/children")).get("descendants", [])]

    def graph(self, branch: str) -> Record:
        return Record(self.client.get(self._branch(branch, "/graph")))

    def apply_graph(self, branch: str, expected_revision: int, edits: Sequence[JSON], **options: Any) -> Record:
        body: JSON = {"expected_revision": expected_revision, "edits": list(edits)}
        body.update(options)
        return Record(self.client.post(self._branch(branch, "/graph"), body))

    def operations(self, branch: Optional[str] = None) -> list[Operation]:
        """Queued and running operations, or only those on `branch`."""
        return [Operation(o) for o in self.client.get(f"{self._base}/operations", branch=branch).get("operations", [])]

    def events(self, branch: str, cursor: int = 0) -> BranchEvents:
        """`branch`'s recorded events after the first `cursor`; pass the
        result's `cursor` back for the next ones."""
        return BranchEvents(self.client.get(self._branch(branch, "/events"), cursor=cursor))

    def event_page(self, branch: str, cursor: Optional[int] = None, limit: Optional[int] = None) -> Record:
        """`EventPage`: up to `limit` events after `cursor`, or the most
        recent without one; `total` is how many the branch has."""
        return Record(self.client.get(self._branch(branch, "/event-page"), cursor=cursor, limit=limit))

    def said(self, branch: str, cursor: int = 0) -> tuple[str, int]:
        """What the harness said on `branch` after event `cursor`: its
        message deltas joined, and the cursor after them."""
        page = self.events(branch, cursor)
        text = "".join(_delta_text(e) for e in page.events)
        return text, int(page.get("cursor", cursor))

    def wait_for(self, branches: Sequence[str], *, any: bool = False, timeout: Optional[float] = None) -> Record:
        """Block until `branches` settle (or `any` one does), as `by wait`;
        the server caps each ask at 30 s, so this asks again until `timeout`.
        `Waited`: `{"settled": [Inspection], "pending": [branch],
        "timed_out"?}`."""
        deadline = None if timeout is None else time.monotonic() + timeout
        while True:
            remaining = None if deadline is None else max(0.0, deadline - time.monotonic())
            ask = 30.0 if remaining is None else min(30.0, remaining)
            waited = Record(
                self.client.request(
                    "POST",
                    f"{self._base}/wait",
                    _drop_none({"branches": list(branches), "any": any, "timeout_seconds": ask}),
                    timeout=ask + self.client.timeout,
                )
            )
            if not waited.get("timed_out") or (remaining is not None and remaining <= 0):
                return waited

    def stream(
        self,
        cursor: Optional[int] = None,
        *,
        branches: Optional[Sequence[str]] = None,
        read_timeout: float = 60.0,
        max_failures: int = 10,
    ) -> "EventStream":
        """The repository's activity after feed position `cursor` (from now
        without one), as `FeedEntry`s, reconnecting with the last position
        after a failure. `branches` keeps only those branches' entries."""
        return EventStream(self, cursor, branches=branches, read_timeout=read_timeout, max_failures=max_failures)

    # -- the inbox, as a branch --------------------------------------------

    def inbox(self, branch: str) -> list[Message]:
        return [Message(m) for m in self.client.get(self._branch(branch, "/inbox")).get("messages", [])]

    def ask(self, branch: str, text: str, wait_seconds: Optional[float] = None) -> Record:
        return Record(
            self.client.post(self._branch(branch, "/ask"), _drop_none({"text": text, "wait_seconds": wait_seconds}))
        )

    def report(self, branch: str, text: str) -> Message:
        return Message(self.client.post(self._branch(branch, "/report"), {"text": text}))

    def escalate(self, branch: str, text: str) -> Message:
        return Message(self.client.post(self._branch(branch, "/escalate"), {"text": text}))

    def answer(self, branch: str, message_id: int, text: str) -> Message:
        """Answer one of `branch`'s descendants' messages, as `branch`."""
        return Message(self.client.post(self._branch(branch, "/answer"), {"message_id": message_id, "text": text}))

    # -- plans -------------------------------------------------------------

    def plan(self, branch: str) -> Record:
        return Record(self.client.get(self._branch(branch, "/plan")))

    def approve_plan(
        self, branch: str, edited: Optional[str] = None, key: Optional[str] = None, **options: Any
    ) -> Operation:
        body: JSON = {"edited": edited}
        body.update(options)
        return self.client._operation(self._branch(branch, "/plan/approve"), body, key)

    def reject_plan(self, branch: str, reason: str, replan: bool = False, key: Optional[str] = None) -> Operation:
        return self.client._operation(self._branch(branch, "/plan/reject"), {"reason": reason, "replan": replan}, key)

    # -- approvals and effects ---------------------------------------------

    def approvals(self, all: bool = False) -> list[Approval]:
        """The approvals waiting (every one with `all`): a branch's turn
        asking to run a tool or perform a connector operation its grant
        says to confirm (`docs/effects.md#approvals`)."""
        query = {"all": "true"} if all else {}
        return [Approval(a) for a in self.client.get(f"{self._base}/approvals", **query).get("approvals", [])]

    def allow(self, approval_id: str, reason: Optional[str] = None, surface: Optional[str] = None) -> Approval:
        """Answer an approval yes, as the caller (`run` scope)."""
        return Approval(
            self.client.post(
                f"{self._base}/approvals/{_q(approval_id)}/allow", _drop_none({"reason": reason, "surface": surface})
            )
        )

    def deny(self, approval_id: str, reason: Optional[str] = None, surface: Optional[str] = None) -> Approval:
        return Approval(
            self.client.post(
                f"{self._base}/approvals/{_q(approval_id)}/deny", _drop_none({"reason": reason, "surface": surface})
            )
        )

    def effects(self, branch: Optional[str] = None) -> list[Effect]:
        """The effect ledger (`docs/effects.md`): every connector call with
        effects, or `branch`'s."""
        return [Effect(e) for e in self.client.get(f"{self._base}/effects", branch=branch).get("effects", [])]

    def effect(self, effect_id: str) -> Record:
        """One entry and its events: `{"entry", "events"}`."""
        return Record(self.client.get(f"{self._base}/effects/{_q(effect_id)}"))

    def promote_effect(self, effect_id: str) -> Effect:
        return Effect(self.client.post(f"{self._base}/effects/{_q(effect_id)}/promote", {}))

    # -- artifacts and scratch areas ---------------------------------------

    def publish_artifact(
        self, branch: str, data: bytes, name: str, kind: Optional[str] = None, key: Optional[str] = None
    ) -> Record:
        query = _drop_none({"name": name, "kind": kind})
        return Record(
            self.client.request("POST", self._branch(branch, "/artifacts"), data, key=key or new_key(), query=query)
        )

    def artifacts(self, branch: str) -> list[Record]:
        return [Record(a) for a in self.client.get(self._branch(branch, "/artifacts")).get("artifacts", [])]

    def read_artifact(self, branch: str, artifact_id: str) -> tuple[Record, bytes]:
        ref = Record(self.client.get(self._branch(branch, f"/artifacts/{_q(artifact_id)}")))
        _, _, content = self.client.request(
            "GET", self._branch(branch, f"/artifacts/{_q(artifact_id)}/content"), raw=True
        )
        return ref, content

    def share_artifact(self, branch: str, artifact_id: str, to: str) -> None:
        self.client.post(self._branch(branch, f"/artifacts/{_q(artifact_id)}/share"), {"to": to}, key=new_key())

    def create_scratch(self, branch: str, name: str) -> Record:
        return Record(self.client.post(self._branch(branch, "/scratch"), {"name": name}, key=new_key()))

    def scratch_areas(self, branch: str) -> list[Record]:
        return [Record(a) for a in self.client.get(self._branch(branch, "/scratch")).get("areas", [])]

    def share_scratch(self, branch: str, name: str, to: str) -> None:
        self.client.post(self._branch(branch, f"/scratch/{_q(name)}/share"), {"to": to}, key=new_key())

    # -- surfaces ----------------------------------------------------------

    def surface(self, prefix: str, **defaults: Any) -> "Surface":
        """A `Surface`: one resident branch per conversation, named
        `<prefix>-<key>`, created with `defaults` (as `task` takes them)."""
        return Surface(self, prefix, **defaults)


def _budget(budget: Optional[JSON], budget_usd: Optional[float]) -> Optional[JSON]:
    if budget_usd is None:
        return budget
    merged = dict(budget or {})
    merged["max_usd"] = budget_usd
    return merged


def _provision(provision: Optional[JSON], connectors: Optional[Grants]) -> Optional[JSON]:
    if connectors is None:
        return provision
    merged = dict(provision or {})
    merged["connectors"] = grants_json(connectors)
    return merged


def _delta_text(event: JSON) -> str:
    activity = event.get("activity") if isinstance(event, dict) else None
    harness = activity.get("harness") if isinstance(activity, dict) else None
    if isinstance(harness, dict) and harness.get("type") == "message_delta":
        return str(harness.get("text", ""))
    return ""


# ---------------------------------------------------------------------------
# The event stream


class _Idle(Exception):
    """A read on the stream saw nothing for `read_timeout` seconds."""


class EventStream:
    """`GET .../events/stream` as an iterator of `FeedEntry`s
    (`docs/server.md#event-stream`). `cursor` is the last position seen
    and `head` the feed's end when the stream opened; a failure reconnects
    from `cursor` with backoff, up to `max_failures` in a row, then raises
    the last `TransportError`. The server comments every 15 s, so a read
    quiet for `read_timeout` seconds is a dead connection: the stream
    reconnects at once (not a failure) after calling `on_idle`, if set,
    which may `close()` it. `for entry in stream` is the loop."""

    def __init__(
        self,
        repo: Repo,
        cursor: Optional[int],
        *,
        branches: Optional[Sequence[str]],
        read_timeout: float,
        max_failures: int,
    ):
        self.repo = repo
        self.cursor = cursor
        self.head: Optional[int] = None
        self.branches = set(branches) if branches is not None else None
        self.read_timeout = read_timeout
        self.max_failures = max_failures
        self.on_idle: Optional[Callable[[], None]] = None
        self._response: Any = None
        self._closed = False

    def __iter__(self) -> Iterator[FeedEntry]:
        failures = 0
        while not self._closed:
            try:
                self._open()
                failures = 0
                yield from self._entries()
                if self._closed:
                    return
                # The server closed the stream (a shutdown): reconnect.
                raise TransportError("the event stream ended")
            except _Idle:
                if self.on_idle is not None:
                    self.on_idle()
            except TransportError as error:
                failures += 1
                if self._closed:
                    return
                if failures > self.max_failures:
                    raise error
                time.sleep(min(30.0, 0.5 * (2 ** (failures - 1))))
            finally:
                self._drop()

    def close(self) -> None:
        self._closed = True
        self._drop()

    def _drop(self) -> None:
        response, self._response = self._response, None
        if response is not None:
            with contextlib.suppress(OSError):
                response.close()

    def _open(self) -> None:
        client = self.repo.client
        url = f"{client.url}{self.repo._base}/events/stream"
        headers = {
            "Authorization": f"Bearer {client.token}",
            "Accept": "text/event-stream",
            "User-Agent": client.user_agent,
        }
        if self.cursor is not None:
            headers["Last-Event-ID"] = str(self.cursor)
        req = urllib.request.Request(url, headers=headers, method="GET")
        try:
            self._response = urllib.request.urlopen(req, timeout=self.read_timeout, context=client._context)
        except urllib.error.HTTPError as error:
            content = error.read()
            raise error_for(
                error.code, _json_or_text(content), {k.lower(): v for k, v in error.headers.items()}
            ) from None
        except (urllib.error.URLError, OSError, ssl.SSLError) as error:
            raise TransportError(f"GET {url}: {error}") from None

    def _entries(self) -> Iterator[FeedEntry]:
        event_name = ""
        data: list[str] = []
        last_id: Optional[str] = None
        while True:
            response = self._response
            if response is None:
                return  # closed by the consumer between two entries
            try:
                raw = response.readline()
            except socket.timeout:
                if self._closed:
                    return
                raise _Idle() from None
            except (OSError, ssl.SSLError, ValueError) as error:  # ValueError: read on a closed response
                if self._closed:
                    return
                raise TransportError(f"event stream: {error}") from None
            if not raw:
                return
            line = raw.decode("utf-8", "replace").rstrip("\r\n")
            if line == "":
                if data:
                    entry = self._dispatch(event_name, "\n".join(data), last_id)
                    if entry is not None:
                        yield entry
                event_name, data, last_id = "", [], None
                continue
            if line.startswith(":"):
                continue
            field, _, value = line.partition(":")
            value = value[1:] if value.startswith(" ") else value
            if field == "event":
                event_name = value
            elif field == "data":
                data.append(value)
            elif field == "id":
                last_id = value

    def _dispatch(self, event_name: str, data: str, last_id: Optional[str]) -> Optional[FeedEntry]:
        try:
            payload = json.loads(data)
        except json.JSONDecodeError:
            return None
        if event_name == "open":
            self.cursor = int(payload.get("cursor", self.cursor or 0))
            self.head = payload.get("head")
            return None
        if event_name != "activity" or not isinstance(payload, dict):
            return None
        seq = payload.get("seq")
        if seq is None and last_id is not None:
            seq = int(last_id)
        self.cursor = int(seq) if seq is not None else self.cursor
        if self.branches is not None and payload.get("branch") not in self.branches:
            return None
        return FeedEntry(payload)


# ---------------------------------------------------------------------------
# Surfaces: one branch per conversation


class Surface:
    """A third-party surface (a Slack workspace, a Discord server, a web
    app) whose conversations each live on one branch that is created on
    the first message and continued by every later one, the way a chat
    assistant keeps a session. `defaults` are `Repo.task`'s options for the
    first message (`harness`, `budget_usd`, `connectors`, `delegation`,
    `allow_delegation`, `base`, ...). The grant given here is every
    conversation's ceiling: a child a conversation's harness delegates to
    gets at most this."""

    def __init__(self, repo: Repo, prefix: str, **defaults: Any):
        self.repo = repo
        self.prefix = prefix
        self.defaults = defaults

    def conversation(self, *key: str) -> "Conversation":
        """The conversation `key` names (a channel id, a thread, a user),
        on branch `branch_name(prefix, *key)`."""
        return Conversation(self, branch_name(self.prefix, *key))

    def conversations(self) -> list[BranchInfo]:
        """Every branch of this surface, by its prefix."""
        return [b for b in self.repo.branches() if str(b.get("name", "")).startswith(self.prefix + "-")]


class Conversation:
    """One conversation's branch. `say` is the one call a surface needs."""

    def __init__(self, surface: Surface, branch: str):
        self.surface = surface
        self.repo = surface.repo
        self.branch = branch

    def exists(self) -> bool:
        return self.repo.exists(self.branch)

    def inspect(self) -> Inspection:
        return self.repo.inspect(self.branch)

    def history(self, limit: int = 50) -> list[JSON]:
        """The most recent `limit` recorded events."""
        return list(self.repo.event_page(self.branch, limit=limit).get("events") or ())

    def cancel(self) -> list[str]:
        return self.repo.cancel(self.branch)

    def steer(self, text: str) -> Steer:
        return self.repo.steer(self.branch, text)

    def say(
        self,
        text: str,
        *,
        busy: str = "queue",
        key: Optional[str] = None,
        timeout: Optional[float] = None,
        **options: Any,
    ) -> "Reply":
        """Deliver `text`: the first message creates the branch with the
        surface's defaults, every later one continues it. `busy` says what
        to do while the branch runs a turn, as a trigger's `deliver.busy`
        (`docs/triggers.md`): `queue` waits for the running operation and
        sends after it (up to `timeout` seconds, `TimeoutError` past it);
        `steer` adds the text to the running turn and the `Reply` is that
        turn's; `skip` raises `BranchBusyError`. `options` go to the send
        (or the first task)."""
        if busy not in ("queue", "steer", "skip"):
            raise ValueError(f"busy {busy!r} is not queue, steer or skip")
        key = key or new_key()
        deadline = None if timeout is None else time.monotonic() + timeout
        if not self.exists():
            try:
                defaults = dict(self.surface.defaults)
                defaults.update(options)
                op = self.repo.task(text, name=self.branch, key=key, **defaults)
                return Reply(self.repo, self.branch, op)
            except ConflictError as error:
                if error.code != "branch_exists":
                    raise
                # Created between the look and the task: continue it.
        while True:
            try:
                op = self.repo.send(self.branch, text, key=key, **options)
                return Reply(self.repo, self.branch, op)
            except BranchBusyError as error:
                if busy == "skip":
                    raise
                holder = error.holder
                if busy == "steer":
                    try:
                        steer = self.repo.steer(self.branch, text)
                    except BranchyardError as refused:
                        if refused.code != "steer_refused":
                            raise
                    else:
                        running = self.repo.client.operation(holder) if holder else None
                        return Reply(self.repo, self.branch, running, steer=steer)
                    # Refused mid-turn input falls back to queueing.
                if holder:
                    remaining = None if deadline is None else max(0.0, deadline - time.monotonic())
                    if remaining is not None and remaining <= 0:
                        raise TimeoutError(f"{self.branch} still busy after {timeout}s") from None
                    with contextlib.suppress(NotFoundError):
                        self.repo.client.wait(holder, timeout=remaining)
                else:
                    if deadline is not None and time.monotonic() >= deadline:
                        raise TimeoutError(f"{self.branch} still busy after {timeout}s") from None
                    time.sleep(1.0)


class Reply:
    """What a conversation says back to one message: the harness's text on
    the branch for the operation that carried the message. `wait()` blocks
    until the turn ends; `deltas()` yields the text as it arrives (for a
    surface that streams); `operation` is the operation, `None` when the
    message was steered into a turn whose operation is unknown."""

    def __init__(self, repo: Repo, branch: str, operation: Optional[Operation], steer: Optional[Steer] = None):
        self.repo = repo
        self.branch = branch
        self.operation = operation
        self.steer = steer
        self._text: Optional[str] = None

    @property
    def steered(self) -> bool:
        return self.steer is not None

    def wait(self, timeout: Optional[float] = None, poll: float = 0.5) -> "Reply":
        """Block until the operation finishes; `OperationFailedError` when
        it failed, `TimeoutError` past `timeout`. Then `text` is ready."""
        if self.operation is None:
            raise ValueError("no operation to wait for: the message was steered into a turn without one")
        self.operation = self.repo.client.wait(self.operation, timeout=timeout, poll=poll, check=True)
        return self

    @property
    def text(self) -> str:
        """The harness's message text for the operation, read once it has
        finished (`wait` first); the empty string before."""
        if self._text is None:
            if self.operation is None or not self.operation.done:
                return ""
            self._text = "".join(self.deltas())
        return self._text

    @property
    def branch_info(self) -> Optional[BranchInfo]:
        """The branch as the finished operation left it (`status` says
        `ready`, `failed`, `budget_exceeded`, ...)."""
        if self.operation is None:
            return None
        for branch in self.operation.result_branches:
            if branch.get("name") == self.branch:
                return branch
        return None

    def deltas(self, read_timeout: Optional[float] = None) -> Iterator[str]:
        """The message text as the harness produces it, from the operation's
        start to its end: the repository's feed after the operation's
        `cursor` up to its `end_cursor`, this branch's `message_delta`s
        only. On a finished operation it is a bounded read; on a running
        one it ends when the operation does, looked up again whenever the
        branch's turn ends or the feed is quiet for `read_timeout` seconds
        (5 by default while running)."""
        if self.operation is None:
            return
        op = self.operation
        start = int(op.get("cursor") or 0)
        seen = start
        if op.done and int(op.get("end_cursor") or start) <= start:
            return  # Nothing was recorded for it: refused before a turn.
        stream = self.repo.stream(start, read_timeout=read_timeout or (60.0 if op.done else 5.0))
        idles_done = 0

        def refresh() -> None:
            nonlocal op
            if not op.done:
                op = self.repo.client.operation(op.id)

        def idle() -> None:
            # Quiet for `read_timeout`: look again, and once the operation
            # has finished, give the feed one more connection to deliver
            # what it recorded last, then stop.
            nonlocal idles_done
            refresh()
            if op.done:
                end = op.get("end_cursor")
                if end is None or seen >= int(end) or idles_done >= 1:
                    stream.close()
                idles_done += 1

        stream.on_idle = idle
        try:
            for entry in stream:
                seq = int(entry.get("seq") or 0)
                end = op.get("end_cursor")
                if end is not None and seq > int(end):
                    break
                seen = seq
                if entry.get("branch") == self.branch:
                    text = entry.text
                    if text:
                        yield text
                    if entry.turn_ended or entry.status is not None:
                        refresh()
                        end = op.get("end_cursor")
                if end is not None and seq >= int(end):
                    break
                if op.done and stream.head is not None and seq >= stream.head:
                    break  # The feed ended before `end_cursor`: positions may skip.
        finally:
            stream.close()
        if op.done:
            self.operation = op
