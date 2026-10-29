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

    # Local mode: a `by serve` of this process's own, on the repository
    # here, stopped when the client is closed (`docs/sdk.md#local-mode`).
    with Client.local(".") as client:
        app = client.repo("app")
        op = app.task("Make the flaky parser test deterministic", name="flaky-parser", budget_usd=2)
        op = client.wait(op, check=True)   # raises if the request failed or its branch did
        print(op.result_branches[0].state)  # "ready"

    # Or a server that is running (`by serve --repo app=.`; the token is in
    # `.branchyard/server/token`): the same calls.
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

`Local` is this machine's `by` for what a server does not do: `by up`
brings the configured backends up (harnesses installed, the gateway
started, and what only a person can do listed), `by doctor` and `by
harnesses` report; each as the JSON `by --json` prints.

Not guaranteed: anything the server does not. This module adds no
authority; what a token may do, the server decides.
"""

import base64
import contextlib
import dataclasses
import json
import os
import re
import select
import shutil
import socket
import ssl
import subprocess
import tempfile
import threading
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
    "BranchFailedError",
    "BranchInfo",
    "BranchyardError",
    "CheckFailedError",
    "CheckNotStartedError",
    "CheckReport",
    "CheckTimedOutError",
    "Client",
    "ConflictError",
    "Conversation",
    "Delegator",
    "DeniedError",
    "Effect",
    "EventStream",
    "FeedEntry",
    "Grant",
    "IdempotencyKeyReusedError",
    "Inspection",
    "InvalidRequestError",
    "Local",
    "LocalError",
    "Message",
    "NotAllowedError",
    "NotFoundError",
    "NotRunningError",
    "Operation",
    "OperationFailedError",
    "Plan",
    "QuotaExceededError",
    "Record",
    "Reply",
    "Repo",
    "Report",
    "Reported",
    "Rewound",
    "RunningError",
    "ServerError",
    "StaleRevisionError",
    "Status",
    "StatusReport",
    "Steer",
    "SteerRefusedError",
    "Step",
    "Surface",
    "TerminalOutput",
    "TransportError",
    "UnauthorizedError",
    "UnsupportedError",
    "Waited",
    "branch_name",
    "connect",
    "error_for_code",
    "new_key",
]

JSON = dict[str, Any]
Grants = Sequence[Union["Grant", str, JSON]]

# The longest one wait request asks the server for, in seconds: its own cap
# on `POST .../wait` and `GET /v1/operations/{id}?wait=`, past which a wait
# asks again.
_WAIT_CAP = 30.0

# ---------------------------------------------------------------------------
# Errors


class BranchyardError(Exception):
    """A refusal or failure, as the server or `by --json` reported it:
    `code` is stable (`docs/server.md#errors`), and `kind` is the same
    value under the name `by --json` gives it, so one `except` and one
    attribute read serve both. `message` is for people, `detail` the
    code's extra data (`{}` when it has none), `status` the HTTP status (0
    from `by`). `sdk/python/branchyard.py` raises these same classes."""

    # The failed operation, when the error is one's: `OperationFailedError`,
    # and a `Delegator`'s errors over HTTP.
    operation: Optional["Operation"] = None

    def __init__(self, code: str, message: str, detail: Optional[JSON] = None, status: int = 0):
        super().__init__(message)
        self.code = code
        self.message = message
        self.detail = detail or {}
        self.status = status

    @property
    def kind(self) -> str:
        """`code`, by the name `by --json` gives it."""
        return self.code

    @kind.setter
    def kind(self, value: str) -> None:
        self.code = value

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
    `unknown_trigger`, `unknown_checkpoint`, `unknown_knowledge`,
    `unknown_message` or `not_found`: no such thing, or another tenant's."""


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
    `cursor_out_of_range`, `unsupported_media_type`, `body_too_large`.
    `field` names the request field that broke a rule when the server says
    which (`Repo.report_status`: `state`, `summary` or `link`)."""

    @property
    def field(self) -> Optional[str]:
        """The request field that broke a rule when the server names one, else None."""
        value = self.detail.get("field")
        return value if isinstance(value, str) else None


class BranchBusyError(BranchyardError):
    """`branch_busy`: another operation holds the branch; `holder` is its
    id. Wait for it (`Client.wait`), steer the running turn instead
    (`Repo.steer`), or skip."""

    @property
    def holder(self) -> Optional[str]:
        """The id of the operation that holds the branch, when the server says; else None."""
        value = self.detail.get("holder")
        return value if isinstance(value, str) else None


class RunningError(BranchyardError):
    """`running`: a turn runs on the branch in another engine (a local
    `by`), or a scratch lock is held; also a wait that timed out with the
    branches still running (`branchyard.wait`, `Delegator.wait`)."""


class NotRunningError(BranchyardError):
    """`not_running`: the branch runs no turn, and steering needs one."""


class SteerRefusedError(BranchyardError):
    """`steer_refused`: the running turn did not take steered input; the
    message says why."""


class UnsupportedError(BranchyardError):
    """`unsupported` or `harness_unavailable`: the harness cannot do this
    (take input mid-turn, choose a model), or cannot run here."""


class ConflictError(BranchyardError):
    """An engine refusal: `branch_exists`, `no_candidate`, `target_moved`,
    `conflict`, `dirty_target`, `already_merged`, `fenced`,
    `detached_head`, `trigger_exists`, `no_plan`, `stale_revision`,
    `checkpoint_unavailable`, `service_held`, `invalid_candidate`."""


class StaleRevisionError(ConflictError):
    """`stale_revision`: a graph proposal was made against a revision that
    moved on; read the graph again."""


class CheckFailedError(BranchyardError):
    """`check_failed`, and the base of `CheckTimedOutError` and
    `CheckNotStartedError`: a merge or integration's check, so nothing was
    integrated; `detail.output_tail` or `detail.checks`.

    When siblings left out of the integration share the check and could
    change the result, `integrate_together` names the branches to
    integrate at once and `unsettled` those of them still to wait for;
    both are empty otherwise, and `own_work` names the branches integrated,
    on whose own work it failed. `checks` names every check of the
    branches integrated, in the order they ran, as dicts `{check,
    branches, outcome}`: `outcome` is `passed`, `failed`, `timed_out`,
    `not_started` or `not_run` (a check before it failed)."""

    @property
    def checks(self) -> list[JSON]:
        """Every check of the branches integrated, in the order they ran: dicts `{check, branches, outcome}`."""
        return list(self.detail.get("checks", []))

    @property
    def integrate_together(self) -> list[str]:
        """The branches to integrate at once, when siblings sharing the check could change the result."""
        return list(self.detail.get("integrate_together", []))

    @property
    def unsettled(self) -> list[str]:
        """Those of `integrate_together` that have still to settle."""
        return list(self.detail.get("unsettled", []))

    @property
    def own_work(self) -> list[str]:
        """The branches integrated, on whose own work the check failed."""
        return list(self.detail.get("own_work", []))


class CheckTimedOutError(CheckFailedError):
    """`check_timed_out`: the check ran past its timeout on the merge; its
    outcome in `checks` is `timed_out`."""


class CheckNotStartedError(CheckFailedError):
    """`check_not_started`: the check could not be started on the merge;
    its outcome in `checks` is `not_started`."""


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


class LocalError(BranchyardError):
    """`local`: `by` on this machine could not be run, did not start its
    server, or printed no JSON (`Client.local`, `Local`). The message
    carries what it said."""

    def __init__(self, message: str):
        super().__init__("local", message)


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


# The states a branch ends in when the work it was asked for did not get done.
_FAILED_STATES = ("failed", "budget_exceeded", "interrupted")


class BranchFailedError(OperationFailedError):
    """An operation succeeded, but a branch it made ended `failed`,
    `budget_exceeded` or `interrupted`: the harness's turn did not do the
    work. Raised by `Client.wait(..., check=True)` and `Reply.wait`, which
    would otherwise return normally for a run whose turn never started.

    `code` is the branch's state, `branch` its name, `branch_info` its
    `BranchInfo`, `branch_status` its `Status` (`status` is the HTTP status
    of every `BranchyardError`, 0 here) and `state` that status's state;
    `reason` is why (`failed`)
    and `limit` which limit (`budget_exceeded`), None when the status names
    none. `operation` is the finished operation. `branches` holds every
    result branch that ended this way, the first being `branch_info` (a map
    can have several)."""

    def __init__(self, operation: "Operation", failed: Optional[Sequence["BranchInfo"]] = None):
        branches = list(failed if failed is not None else operation.failed_branches)
        if not branches:
            raise ValueError("BranchFailedError needs a result branch that ended failed")
        first = branches[0]
        status = first.status
        state = status.state
        why = status.get("reason") or status.get("limit")
        name = str(first.get("name", ""))
        message = f"branch {name} ended {state}" + (f": {why}" if why else "")
        if len(branches) > 1:
            message += f" (and {len(branches) - 1} more: {', '.join(str(b.get('name', '')) for b in branches[1:])})"
        message += f". {_branch_fix(state, name)}"
        BranchyardError.__init__(
            self,
            state,
            message,
            {"branch": name, "status": status.raw, "branches": [str(b.get("name", "")) for b in branches]},
        )
        self.operation = operation
        self.branch: str = name
        self.branch_info: BranchInfo = first
        self.branch_status: Status = status
        self.branches: list[BranchInfo] = branches
        self.state: str = state
        self.reason: Optional[str] = status.get("reason") if isinstance(status.get("reason"), str) else None
        self.limit: Optional[str] = status.get("limit") if isinstance(status.get("limit"), str) else None


def _branch_fix(state: str, name: str) -> str:
    """The `to fix:` clause for a branch that ended `state`."""
    if state == "budget_exceeded":
        return f'to fix: raise the limit and give it another turn, app.send("{name}", "continue", budget_usd=...)'
    if state == "interrupted":
        return f'to fix: give it another turn, app.send("{name}", "continue")'
    return f'to fix: read why with `by log {name}` or app.events("{name}"), then app.send("{name}", "try again")'


_BY_CODE: dict[str, Callable[..., BranchyardError]] = {
    "unauthorized": UnauthorizedError,
    "not_found": NotFoundError,
    "method_not_allowed": NotFoundError,
    "unknown_repo": NotFoundError,
    "unknown_branch": NotFoundError,
    "unknown_operation": NotFoundError,
    "unknown_trigger": NotFoundError,
    "unknown_checkpoint": NotFoundError,
    "unknown_knowledge": NotFoundError,
    "unknown_message": NotFoundError,
    "unknown_preview_asset": NotFoundError,
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
    "not_running": NotRunningError,
    "steer_refused": SteerRefusedError,
    "unsupported": UnsupportedError,
    "harness_unavailable": UnsupportedError,
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
    "stale_revision": StaleRevisionError,
    "checkpoint_unavailable": ConflictError,
    "service_held": ConflictError,
    "invalid_candidate": ConflictError,
    "check_failed": CheckFailedError,
    "check_timed_out": CheckTimedOutError,
    "check_not_started": CheckNotStartedError,
    "quota_exceeded": QuotaExceededError,
    "rate_limited": QuotaExceededError,
    "idempotency_key_reused": IdempotencyKeyReusedError,
}

# Server-side failures by the kinds `by --json` gives them; the server's
# codes for them (`git_error`, `io_error`, ...) are 5xx.
_BY_FAILURES = frozenset({"git", "io", "state", "harness"})


def error_for(status: int, body: Any, headers: Optional[dict[str, str]] = None) -> BranchyardError:
    """The exception for an error response: by its `code`, else by its
    status (5xx `ServerError`, the rest `BranchyardError`)."""
    error = body.get("error") if isinstance(body, dict) else None
    if not isinstance(error, dict) or "code" not in error:
        return TransportError(f"HTTP {status} without an API error body: {str(body)[:200]}")
    code = str(error["code"])
    retry_after = None
    value = (headers or {}).get("retry-after")
    if value and value.strip().isdigit():
        retry_after = float(value)
    return error_for_code(code, str(error.get("message", code)), error.get("detail"), status, retry_after)


def error_for_code(
    code: str, message: str, detail: Any = None, status: int = 0, retry_after: Optional[float] = None
) -> BranchyardError:
    """The exception for an error `code`, the server's or the `kind` `by
    --json` prints, else by `status` (5xx `ServerError`, the rest
    `BranchyardError`). Both Python modules raise through it."""
    detail = detail if isinstance(detail, dict) else None
    message = _with_fix(code, message, detail)
    cls = _BY_CODE.get(code)
    if cls is QuotaExceededError:
        return QuotaExceededError(code, message, detail, status, retry_after)
    if cls is None:
        cls = ServerError if status >= 500 or code in _BY_FAILURES else BranchyardError
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
    (`op.state`, `branch.status`).

    **A key the answer does not carry reads as `None`**, not as an error:
    a newer server's fields never break an older client, and an older
    server's absent fields are simply `None`. The price is that a typo is
    `None` too (`op.stat`): the fields of a record are in its class's
    docstring, in `docs/server.md`, and in `dir(record)`, which lists the
    keys it holds with the names of its properties. `raw` is the plain
    dict. `Operation`, `BranchInfo`, `Inspection`, `FeedEntry`, `Message`
    and `CheckReport` declare their fields as class annotations, which an
    editor that reads the source (Pyright, Pylance) completes and checks;
    the other records read as `Any`. mypy treats the installed module as
    untyped: a single-file module cannot carry the `py.typed` marker, and
    making this a package changes how Branchyard writes it into a
    delegating turn (`docs/sdk.md#what-the-client-does-not-do`)."""

    def __getattr__(self, name: str) -> Any:
        if name.startswith("__"):
            raise AttributeError(name)
        return self.get(name)

    def __dir__(self) -> list[str]:
        """The record's keys beside its properties and methods, so a REPL or an editor completes `op.sta` to `state`."""
        keys = [key for key in self if isinstance(key, str) and key.isidentifier()]
        return sorted(set(super().__dir__()) | set(keys))

    @property
    def raw(self) -> JSON:
        """The plain dict (a copy), for code that wants a dict and nothing else, such as `json.dumps`."""
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

    name: str
    git_branch: str
    harness: str
    profile: str
    parent: Optional[str]
    children: list[str]
    depth: int
    base: str
    candidate: Optional[JSON]  # {commit, files_changed, insertions, deletions}; None before it has changes
    turns: int
    cost_usd: Optional[float]  # None when the harness reports none
    created_at: int
    stalled: bool

    @property
    def status(self) -> Status:
        """The branch's `Status` (an empty one when the answer carried none)."""
        return Status(self.get("status") or {})

    @property
    def state(self) -> str:
        """The name of the status's state, such as `ready`; the empty string when there is none."""
        return str(self.status.get("state", ""))

    @property
    def settled(self) -> bool:
        """Not running, waiting, blocked, waiting on children or awaiting
        plan approval."""
        return self.state not in ("running", "waiting", "blocked", "waiting_on_children", "awaiting_plan_approval")


class StatusReport(Record):
    """A branch's own account of what it is doing, from its `report_status`
    tool (`docs/delegation.md#self-reported-status`): `state` (`working`,
    `idle`, `blocked`, `complete` or `failure`), `summary`, `link` (None
    when none was given) and `at_ms`; `ttl_seconds` and `expires_at_ms`
    when it was given a time-to-live; and `stale_because` (`ended`,
    `later_turn` or `expired`) once it no longer says what the branch is
    doing. A claim beside the protocol's `status`, which may disagree with
    it."""

    @property
    def stale(self) -> bool:
        """Whether the report had gone stale when it was read."""
        return bool(self.get("stale"))


class Reported(Record):
    """What `Repo.report_status` returns: `report` (a `StatusReport`, as
    recorded) and `escalation`, the `Message` sent to the parent for a
    `blocked` or `failure` report, or None for the other states, for a
    branch with no parent, and for a report that repeats the one before it
    exactly (the parent was told then)."""

    @property
    def report(self) -> StatusReport:
        """The recorded `StatusReport`."""
        return StatusReport(self.get("report") or {})

    @property
    def escalation(self) -> Optional["Message"]:
        """The `Message` sent to the parent for a `blocked` or `failure` report, else None."""
        value = self.get("escalation")
        return Message(value) if isinstance(value, dict) else None


class Rewound(Record):
    """What `Repo.rewind` returns, as `by rewind --json` prints it:
    `branch`, `from` (the checkpoint it was at, None when it had none),
    `to`, `commit` (the checkpoint's, now the branch's candidate) and
    `session`, how its next turn continues: `{"mode": "native", "session"}`,
    `{"mode": "summary", "turns", "reason"}` or `{"mode": "fresh",
    "reason"}` (`docs/checkpoints.md`). `from` is a keyword, so read it as
    `rewound["from"]`."""

    @property
    def session_mode(self) -> Optional[str]:
        """How the branch's next turn continues: `native`, `summary` or `fresh`; None when not stated."""
        session = self.get("session")
        return session.get("mode") if isinstance(session, dict) else None


class CheckReport(Record):
    """A branch's check run on its current work, as `by check` prints it:
    `branch`, `check` (its argv, None when it has none: nothing ran),
    `inherited_from`, `work` (a commit), `target` (the parent's git branch
    it was merged into), `checked` (the commit the check ran on),
    `outcome` (`passed`, `failed`, `timed_out`, `not_started`, or `not_run`
    when there is no check or `conflicts` names files), `conflicts` and
    `output_tail`. From `Operation.check_report`."""

    branch: str
    check: Optional[list[str]]
    outcome: str
    conflicts: list[str]
    output_tail: Optional[str]

    @property
    def passed(self) -> bool:
        """Whether integrating the work as it is would get past its check:
        the check passed, or there is none and nothing conflicted."""
        if self.get("check") is not None:
            return self.get("outcome") == "passed"
        return not self.get("conflicts")


class Inspection(Record):
    """`Inspection`, as `by inspect --json`: `name`, `status`, `harness`,
    `parent`, `children`, `depth`, `turns`, `cost_usd`, `subtree_cost_usd`,
    `max_usd`, `remaining_usd`, `tree_remaining_usd`, `tree_capped_by`,
    `envelope`, `allowed_harnesses`,
    `last_message`, `check`, `model`, `grants`, `reported`, ...
    (`docs/delegation.md`)."""

    name: str
    harness: str
    parent: Optional[str]
    children: list[str]
    depth: int
    turns: int
    cost_usd: Optional[float]

    @property
    def status(self) -> Status:
        """The branch's `Status` (an empty one when the answer carried none)."""
        return Status(self.get("status") or {})

    @property
    def state(self) -> str:
        """The name of the status's state, such as `ready`; the empty string when there is none."""
        return str(self.status.get("state", ""))

    @property
    def grants(self) -> list[Grant]:
        """Its connector grant as stored: what its turns' tokens carry, and
        the most a child of its can be given. Empty without a grant."""
        return [Grant.from_json(g) for g in self.get("grants") or ()]

    @property
    def reported(self) -> Optional[StatusReport]:
        """The branch's own latest account of what it is doing, or None
        until it states one. Not cleared by a new turn: `at_ms` says how old
        it is."""
        value = self.get("reported")
        return StatusReport(value) if isinstance(value, dict) else None

    @property
    def result_contract(self) -> Optional[JSON]:
        """What the branch's parent asked of its answer, as the spawn gave
        it: `{"schema": {...}, "required": bool}`; None when none was
        asked."""
        value = self.get("result_contract")
        return value if isinstance(value, dict) else None

    @property
    def result(self) -> Any:
        """The latest result the branch reported with `report`, a JSON
        value that matched `result_contract` when it has one; None until it
        reports one (`docs/delegation.md#result-contract`)."""
        return self.get("result")


class Steer(Record):
    """`Steer`: `id`, `branch`, `by`, `text`, `requested_at_ms`, `state`
    (`{"state": "accepted" | "written" | "pending"}`), `boundary`."""


class Message(Record):
    """`Message`: `id`, `from`/`to` branches, `kind`, `text`, `at_ms`, ... (`from` is a keyword: `message["from"]`)."""

    id: int
    to: str
    kind: str
    text: str
    at_ms: int

    @property
    def handoff(self) -> Optional[Record]:
        """The frozen source version, result and selected artifact metadata, when present."""
        value = self.get("handoff")
        return Record(value) if isinstance(value, dict) else None

    @property
    def next_turn(self) -> bool:
        """Whether delivery waits for a later turn without steering the active one."""
        return bool(self.get("next_turn", False))


class Approval(Record):
    """`ApprovalAsk`: `id`, `branch`, `turn`, `subject`, `about`
    (`{"kind": "tool" | "operation" | "promote", ...}`), `effect`,
    `request`, `created_ms`, `deadline_ms`, `answer`."""

    @property
    def pending(self) -> bool:
        """Whether nobody has answered it yet."""
        return self.get("answer") is None


class Effect(Record):
    """`EffectEntry`: `id`, `task`, `branch`, `turn`, `subject`, `connector`,
    `operation`, `account`, `class`, `state`, `undo`, `approval`, ..."""


class FeedEntry(Record):
    """One entry of a repository's feed: `seq`, `branch`, `at_ms`,
    `activity` (an `Activity`, externally tagged: `{"harness": {...}}`,
    `{"prompt": "..."}`, `{"status": {...}}`, ...)."""

    seq: int
    branch: str
    at_ms: int
    activity: JSON

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
        """Whether the activity is a harness turn ending."""
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
        """The recorded events of the page, as dicts."""
        return list(self.get("events") or ())


class Operation(Record):
    """An operation (`docs/server.md#operations`): `id`, `repo`, `kind`,
    `state` (`queued`, `running`, `succeeded`, `failed`, `interrupted`),
    `branches`, `cursor` and `end_cursor` (feed positions), `result`
    (`branches`, `descendants`, `merged`, `inspection`, `check_report`), `error`,
    `waiting`, `priority`. `replayed` is true when the server answered a
    repeated idempotency key with the original operation."""

    id: str
    repo: str
    kind: str
    state: str
    branches: list[str]
    cursor: int
    end_cursor: Optional[int]  # None until it has finished
    result: Optional[JSON]  # None until it has succeeded
    error: Optional[JSON]  # {code, message, detail?} once it failed
    priority: int
    workspace_id: Optional[str]
    session_id: Optional[str]
    replayed: bool = False

    @property
    def done(self) -> bool:
        """Whether it has finished, however it ended: `succeeded`, `failed` or `interrupted`."""
        return self.get("state") in ("succeeded", "failed", "interrupted")

    @property
    def failed(self) -> bool:
        """Whether the operation itself ended `failed` or `interrupted`. A succeeded operation can still have a failed
        branch: see `failed_branches`."""
        return self.get("state") in ("failed", "interrupted")

    @property
    def branch(self) -> Optional[str]:
        """The one branch it works on, when it is one."""
        branches = self.get("branches") or []
        return str(branches[0]) if len(branches) == 1 else None

    @property
    def result_branches(self) -> list[BranchInfo]:
        """The `BranchInfo` of each branch the operation's result holds, as the operation left them."""
        result = self.get("result") or {}
        return [BranchInfo(b) for b in result.get("branches") or ()]

    @property
    def failed_branches(self) -> list[BranchInfo]:
        """The result branches that ended `failed`, `budget_exceeded` or
        `interrupted`: empty for an operation that did its work. An
        operation can be `succeeded` while its branch is not: the
        operation is the request, the branch is the harness's turn."""
        return [b for b in self.result_branches if b.state in _FAILED_STATES]

    @property
    def inspection(self) -> Optional[Inspection]:
        """The result's `Inspection` (a spawn's child, for one), else None."""
        result = self.get("result") or {}
        value = result.get("inspection")
        return Inspection(value) if isinstance(value, dict) else None

    @property
    def check_report(self) -> Optional[CheckReport]:
        """A check operation's report (`Repo.check`), once it succeeded. A
        check that failed is a succeeded operation whose report is not
        `passed`; the operation fails only when the check could not be
        run."""
        result = self.get("result") or {}
        value = result.get("check_report")
        return CheckReport(value) if isinstance(value, dict) else None


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


class TerminalOutput(Record):
    """A terminal replay page. `cursor` counts bytes, `truncated` reports
    eviction, and `closed` marks an ended terminal. Handles are process-local."""

    cursor: int
    data: str
    encoding: str
    truncated: bool
    closed: bool
    error: Optional[str]

    @property
    def data_bytes(self) -> bytes:
        """Exact terminal bytes, decoded with the standard base64 codec."""
        if self.get("encoding") != "base64":
            raise ValueError("unsupported terminal output encoding")
        return base64.b64decode(self.get("data", ""), validate=True)


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
        # Set by `Client.local`: the server this client started, its data
        # directory and its log; `close` stops it and removes them.
        self.server: Optional[subprocess.Popen] = None
        self.server_dir: Optional[str] = None
        self.server_log: Optional[str] = None
        self.local_name: Optional[str] = None

    @classmethod
    def local(
        cls,
        root: str = ".",
        *,
        name: str = "app",
        by: Optional[str] = None,
        listen: str = "127.0.0.1:0",
        args: Sequence[str] = (),
        start_timeout: float = 60.0,
        timeout: float = 30.0,
    ) -> "Client":
        """Local mode: a `by serve` of this process's own, serving the
        repository at `root` as `name` on a free loopback port (`listen`),
        with a fresh token and a private data directory, and a client on
        it. `by` is the executable (default: `BRANCHYARD_BY`, else `by` on
        `PATH`); `args` are more `by serve` flags (`--allow-delegation`,
        `--harness-command H=CMD`, whose program path is absolute: the
        server runs in the repository). The server reads the repository's
        configuration as every `by` command does and keeps its branches in
        the repository's `.branchyard/`; only its operation registry is in
        the private directory, which goes when the client is closed.
        `close()` stops the server; use `with Client.local(...) as client`.
        Raises `LocalError` when `by` cannot run or the server does not
        start, with what it said."""
        program = _by_command(by)
        root = os.path.abspath(root)
        data_dir = tempfile.mkdtemp(prefix="branchyard-local-")
        log_path = os.path.join(data_dir, "server.log")
        command = [
            program,
            "serve",
            "--listen",
            listen,
            "--repo",
            f"{name}={root}",
            "--data-dir",
            data_dir,
            "--quiet",
            *args,
        ]
        try:
            with open(log_path, "ab") as log:
                process = subprocess.Popen(
                    command, cwd=root, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=log
                )
        except OSError as error:
            shutil.rmtree(data_dir, ignore_errors=True)
            raise LocalError(_cannot_run(program, error)) from None
        line = _first_line(process, start_timeout)
        if not line.startswith("listening on "):
            _stop(process)
            said = _tail(log_path) or line.strip() or "nothing"
            shutil.rmtree(data_dir, ignore_errors=True)
            raise LocalError(f"`{program} serve` did not start; it said: {said}")
        _drain(process)
        try:
            client = cls(
                line[len("listening on ") :].strip(), token_file=os.path.join(data_dir, "token"), timeout=timeout
            )
        except (OSError, ValueError) as error:
            _stop(process)
            shutil.rmtree(data_dir, ignore_errors=True)
            raise LocalError(f"`{program} serve` started but gave no token: {error}") from None
        client.server = process
        client.server_dir = data_dir
        client.server_log = log_path
        client.local_name = name
        return client

    def close(self) -> None:
        """Stop the server `Client.local` started and remove its directory;
        nothing to do for a client made with a URL."""
        process, self.server = self.server, None
        if process is not None:
            _stop(process)
        directory, self.server_dir = self.server_dir, None
        if directory is not None:
            shutil.rmtree(directory, ignore_errors=True)
        self.server_log = None

    def __enter__(self) -> "Client":
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

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
                url += "?" + urllib.parse.urlencode(pairs, doseq=True)
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
            raise TransportError(_unreachable(f"{method} {url}: {error}", error)) from None
        if raw:
            return status, response_headers, content
        parsed = _json_or_text(content) if content else {}
        if isinstance(parsed, dict) and response_headers.get("idempotent-replayed", "").lower() == "true":
            parsed["_replayed"] = True
        return parsed

    def get(self, path: str, **query: Any) -> Any:
        """`GET path` with `query` parameters; the parsed JSON. The escape hatch for a route with no method, as
        `request`."""
        return self.request("GET", path, query=query or None)

    def post(self, path: str, body: Any = None, key: Optional[str] = None, **query: Any) -> Any:
        """`POST path` with `body` as JSON and `key` as the idempotency key; the parsed JSON. See `request`."""
        return self.request("POST", path, body if body is not None else {}, key=key, query=query or None)

    def delete(self, path: str) -> Any:
        """`DELETE path`; the parsed JSON, empty for most."""
        return self.request("DELETE", path)

    def _operation(self, path: str, body: JSON, key: Optional[str], *, preserve_empty: bool = False) -> "Operation":
        selected = (
            {name: value for name, value in body.items() if value is not None} if preserve_empty else _drop_none(body)
        )
        value = self.post(path, selected, key=key or new_key())
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
        """The served repository `name`. No request is made: an unknown name fails at the first call, `unknown_repo`."""
        return Repo(self, name)

    # -- projects and workspaces -------------------------------------------

    def projects(self) -> list[Record]:
        """Projects available to the authenticated caller."""
        return [Record(value) for value in self.get("/v1/projects").get("projects", [])]

    def project(self, project: str) -> Record:
        """One project, including its source and result acceptance policy."""
        return Record(self.get(f"/v1/projects/{_q(project)}"))

    def create_project(
        self, name: str, *, source: Optional[JSON] = None, result_mode: str = "artifacts", key: Optional[str] = None
    ) -> Record:
        """Create a knowledge project by default. A repository source is
        `{"kind":"repository","repo":"registered-name"}`; no host path is
        accepted. Result modes are artifacts, code or mixed."""
        return Record(
            self.post(
                "/v1/projects",
                {
                    "name": name,
                    "source": source if source is not None else {"kind": "knowledge"},
                    "result_mode": result_mode,
                },
                key=key or new_key(),
            )
        )

    def workspaces(self, project: Optional[str] = None) -> list[Record]:
        """Authorized workspace summaries, optionally within one project."""
        return [Record(value) for value in self.get("/v1/workspaces", project=project).get("workspaces", [])]

    def workspace(self, workspace: str) -> Record:
        """Workspace detail, sessions, immutable sources and accepted outputs."""
        return Record(self.get(self._workspace(workspace)))

    def create_workspace(self, project: str, title: str, *, key: Optional[str] = None) -> Operation:
        """Reserve a workspace. The operation's workspace_id identifies it;
        wait for initialization before starting sessions or uploading sources."""
        return self._operation("/v1/workspaces", {"project_id": project, "title": title}, key)

    def workspace_sessions(self, workspace: str) -> list[Record]:
        """Named native attempts with their current result and provenance."""
        return [Record(value) for value in self.get(self._workspace(workspace, "/sessions")).get("sessions", [])]

    def start_workspace_session(
        self,
        workspace: str,
        name: str,
        prompt: str,
        *,
        harness: str,
        model: Optional[str] = None,
        effort: Optional[str] = None,
        policy: Optional[JSON] = None,
        provision: Optional[JSON] = None,
        result_contract: Optional[JSON] = None,
        access: str = "writer",
        sources: Optional[Sequence[str]] = None,
        artifacts: Optional[Sequence[str]] = None,
        candidate: Optional[str] = None,
        key: Optional[str] = None,
    ) -> Operation:
        """Queue an isolated writer or reviewer using the existing harness.
        Omitted policy retains the server's deny default. Sources and review
        artifacts are immutable IDs; candidate is an exact workspace commit.
        Provisioning names operator-known secrets and connector grants."""
        return self._operation(
            self._workspace(workspace, "/sessions"),
            {
                "name": name,
                "prompt": prompt,
                "harness": harness,
                "model": model,
                "effort": effort,
                "policy": policy,
                "provision": provision,
                "result_contract": result_contract,
                "access": access,
                "sources": list(sources) if sources is not None else None,
                "artifacts": list(artifacts) if artifacts is not None else None,
                "candidate": candidate,
            },
            key,
            preserve_empty=True,
        )

    def workspace_messages(self, workspace: str, session: str, after: Optional[str] = None) -> Record:
        """Native messages and next_cursor; pass that cursor on the next read."""
        return Record(self.get(self._session(workspace, session, "/messages"), after=after))

    def send_workspace_message(
        self, workspace: str, session: str, prompt: str, *, action: Optional[str] = None, key: Optional[str] = None
    ) -> Operation:
        """Queue send, steer or explicit resume. Omitted action means send."""
        return self._operation(
            self._session(workspace, session, "/messages"), {"prompt": prompt, "action": action}, key
        )

    def cancel_workspace_session(self, workspace: str, session: str, *, key: Optional[str] = None) -> Operation:
        """Queue cancellation of one native session."""
        return self._operation(self._session(workspace, session, "/cancel"), {}, key)

    def workspace_action(self, workspace: str, action: str, *, revision: int, key: Optional[str] = None) -> Operation:
        """Queue a revision-checked lifecycle intent, such as archive or restore.
        Unsupported provider capabilities remain explicit server errors."""
        return self._operation(self._workspace(workspace, f"/actions/{_q(action)}"), {"revision": revision}, key)

    def finalize_workspace(
        self, workspace: str, artifacts: Sequence[str], *, revision: int, key: Optional[str] = None
    ) -> Record:
        """Accept current immutable deliverables at revision. Code integration
        remains independent; no Git diff is required for knowledge outputs."""
        return Record(
            self.post(
                self._workspace(workspace, "/finalize"),
                {"revision": revision, "artifacts": list(artifacts)},
                key=key or new_key(),
            )
        )

    def workspace_members(self, workspace: str) -> list[Record]:
        """Current members, bound to verified subjects and workspace roles."""
        return [Record(value) for value in self.get(self._workspace(workspace, "/members")).get("members", [])]

    def workspace_member(self, workspace: str, subject: str) -> Record:
        """One membership by its stable verified subject."""
        return Record(self.get(self._workspace(workspace, f"/members/{_q(subject)}")))

    def set_workspace_member(
        self, workspace: str, subject: str, role: str, *, revision: int, key: Optional[str] = None
    ) -> Record:
        """Set a revision-checked owner/editor/reviewer/viewer role. The server
        requires existing tenant/source grants and an authorized owner."""
        return Record(
            self.post(
                self._workspace(workspace, f"/members/{_q(subject)}"),
                {"revision": revision, "role": role},
                key=key or new_key(),
            )
        )

    def remove_workspace_member(
        self, workspace: str, subject: str, *, revision: int, key: Optional[str] = None
    ) -> Record:
        """Remove membership at revision. The final owner cannot be removed."""
        return Record(
            self.request(
                "DELETE",
                self._workspace(workspace, f"/members/{_q(subject)}"),
                {"revision": revision},
                key=key or new_key(),
            )
        )

    def workspace_comments(self, workspace: str) -> list[Record]:
        """Attributed comments in the workspace."""
        return [Record(value) for value in self.get(self._workspace(workspace, "/comments")).get("comments", [])]

    def add_workspace_comment(self, workspace: str, text: str, *, revision: int, key: Optional[str] = None) -> Record:
        """Add a revision-checked comment; author identity comes from the token."""
        return Record(
            self.post(
                self._workspace(workspace, "/comments"), {"revision": revision, "text": text}, key=key or new_key()
            )
        )

    def workspace_presence(self, workspace: str) -> list[Record]:
        """Unexpired advisory presence. Presence is never a writer lock."""
        return [Record(value) for value in self.get(self._workspace(workspace, "/presence")).get("presence", [])]

    def touch_workspace_presence(self, workspace: str, *, ttl_seconds: int = 60, key: Optional[str] = None) -> Record:
        """Refresh advisory presence for 1..300 seconds without changing revision."""
        return Record(
            self.post(self._workspace(workspace, "/presence"), {"ttl_seconds": ttl_seconds}, key=key or new_key())
        )

    def workspace_sources(self, workspace: str) -> list[Record]:
        """Immutable original source metadata; bytes are downloaded separately."""
        return [Record(value) for value in self.get(self._workspace(workspace, "/sources")).get("sources", [])]

    def upload_workspace_source(
        self,
        workspace: str,
        data: bytes,
        title: str,
        *,
        revision: int,
        media_type: str = "application/octet-stream",
        key: Optional[str] = None,
    ) -> Record:
        """Upload original bytes at revision, retaining the server's media type
        and title header contract. This does not read any client filesystem path."""
        return Record(
            self.request(
                "POST",
                self._workspace(workspace, "/sources"),
                data,
                key=key or new_key(),
                headers={
                    "X-Source-Title": title,
                    "X-Workspace-Revision": str(revision),
                    "Content-Type": media_type,
                },
            )
        )

    def read_workspace_source(self, workspace: str, source: str) -> bytes:
        """Download the authenticated original source bytes."""
        return self.request("GET", self._workspace(workspace, f"/sources/{_q(source)}/content"), raw=True)[2]

    def workspace_artifacts(self, workspace: str) -> list[Record]:
        """Immutable artifacts produced by this workspace's native attempts."""
        return [Record(value) for value in self.get(self._workspace(workspace, "/artifacts")).get("artifacts", [])]

    def read_workspace_artifact(self, workspace: str, artifact: str) -> bytes:
        """Download authenticated artifact bytes without requiring a branch path."""
        return self.request("GET", self._workspace(workspace, f"/artifacts/{_q(artifact)}/content"), raw=True)[2]

    def preview_workspace_source(
        self,
        workspace: str,
        source: str,
        *,
        view: Optional[str] = None,
        page: Optional[int] = None,
        sheet: Optional[str] = None,
        offset: Optional[int] = None,
        limit: Optional[int] = None,
    ) -> Record:
        """Read a document preview of an immutable workspace source."""
        return Record(
            self.get(
                self._workspace(workspace, f"/sources/{_q(source)}/preview"),
                view=view,
                page=page,
                sheet=sheet,
                offset=offset,
                limit=limit,
            )
        )

    def read_workspace_source_preview_asset(
        self,
        workspace: str,
        source: str,
        asset: str,
        *,
        view: Optional[str] = None,
        page: Optional[int] = None,
        sheet: Optional[str] = None,
        offset: Optional[int] = None,
        limit: Optional[int] = None,
    ) -> bytes:
        """Read a source preview image using the same options as its manifest."""
        return self.request(
            "GET",
            self._workspace(workspace, f"/sources/{_q(source)}/preview/assets/{_q(asset)}"),
            query={"view": view, "page": page, "sheet": sheet, "offset": offset, "limit": limit},
            raw=True,
        )[2]

    def preview_workspace_artifact(
        self,
        workspace: str,
        artifact: str,
        *,
        view: Optional[str] = None,
        page: Optional[int] = None,
        sheet: Optional[str] = None,
        offset: Optional[int] = None,
        limit: Optional[int] = None,
    ) -> Record:
        """Read a document preview of an immutable workspace output."""
        return Record(
            self.get(
                self._workspace(workspace, f"/artifacts/{_q(artifact)}/preview"),
                view=view,
                page=page,
                sheet=sheet,
                offset=offset,
                limit=limit,
            )
        )

    def read_workspace_artifact_preview_asset(
        self,
        workspace: str,
        artifact: str,
        asset: str,
        *,
        view: Optional[str] = None,
        page: Optional[int] = None,
        sheet: Optional[str] = None,
        offset: Optional[int] = None,
        limit: Optional[int] = None,
    ) -> bytes:
        """Read an output preview image using the same options as its manifest."""
        return self.request(
            "GET",
            self._workspace(workspace, f"/artifacts/{_q(artifact)}/preview/assets/{_q(asset)}"),
            query={"view": view, "page": page, "sheet": sheet, "offset": offset, "limit": limit},
            raw=True,
        )[2]

    # -- workspace execution and forge -------------------------------------

    def workspace_runtime(self, workspace: str, session: str) -> Record:
        """Actual provider state, owner-host readiness and terminal policy."""
        return Record(self.get(self._session(workspace, session, "/runtime")))

    def control_workspace_runtime(
        self, workspace: str, session: str, action: str, *, key: Optional[str] = None
    ) -> Operation:
        """Queue pause, suspend, resume or destroy of the retained allocation.
        This never starts or replays a model turn."""
        return self._operation(self._session(workspace, session, f"/runtime/actions/{_q(action)}"), {}, key)

    def open_workspace_terminal(
        self,
        workspace: str,
        session: str,
        *,
        rows: Optional[int] = None,
        columns: Optional[int] = None,
        key: Optional[str] = None,
    ) -> Record:
        """Open a fixed operator-approved PTY on the owning provider. It requires
        an idle branch and editor/run authority. Reuse key for an opening retry;
        handles and their bounded replay do not survive restart or eviction."""
        return Record(
            self.post(
                self._session(workspace, session, "/terminals"),
                _drop_none({"rows": rows, "columns": columns}),
                key=key or new_key(),
            )
        )

    def workspace_terminal_output(
        self, workspace: str, session: str, terminal: str, *, after: int = 0, limit: Optional[int] = None
    ) -> TerminalOutput:
        """Read bounded replay after a byte cursor. `data_bytes` is the exact
        decoded output. A lost handle stays a server error; no shell is restarted."""
        return TerminalOutput(
            self.get(self._terminal(workspace, session, terminal, "/output"), after=after, limit=limit)
        )

    def write_workspace_terminal(
        self, workspace: str, session: str, terminal: str, data: bytes, *, sequence: int
    ) -> Record:
        """Write at most 16 KiB using a sequence starting at one. Only the last
        identical sequence can be retried safely; unknown writes are not replayed."""
        return Record(
            self.post(
                self._terminal(workspace, session, terminal, "/input"),
                {"sequence": sequence, "data": base64.b64encode(data).decode("ascii")},
            )
        )

    def resize_workspace_terminal(self, workspace: str, session: str, terminal: str, rows: int, columns: int) -> Record:
        """Resize the actual PTY; rows and columns must be 1..1000."""
        return Record(
            self.post(self._terminal(workspace, session, terminal, "/resize"), {"rows": rows, "columns": columns})
        )

    def close_workspace_terminal(self, workspace: str, session: str, terminal: str) -> Record:
        """Idempotently close the PTY and release its branch writer lease."""
        return Record(self.post(self._terminal(workspace, session, terminal, "/close"), {}))

    def workspace_pull_request(self, workspace: str, session: str) -> Record:
        """Current candidate, pull request and forge/check observations."""
        return Record(self.get(self._session(workspace, session, "/pr")))

    def publish_workspace_pull_request(
        self,
        workspace: str,
        session: str,
        *,
        title: Optional[str] = None,
        base: Optional[str] = None,
        draft: bool = False,
        key: Optional[str] = None,
    ) -> Operation:
        """Queue publication of the actual candidate. Requires editor membership,
        merge scope and operator forge policy; credentials stay with the worker."""
        return self._operation(
            self._session(workspace, session, "/pr"),
            {"title": title, "base": base, "draft": draft},
            key,
            preserve_empty=True,
        )

    def refresh_workspace_pull_request(self, workspace: str, session: str, *, key: Optional[str] = None) -> Operation:
        """Queue fresh forge/check observations without publishing another PR."""
        return self._operation(self._session(workspace, session, "/pr/refresh"), {}, key)

    @staticmethod
    def _workspace(workspace: str, suffix: str = "") -> str:
        return f"/v1/workspaces/{_q(workspace)}{suffix}"

    @staticmethod
    def _session(workspace: str, session: str, suffix: str = "") -> str:
        return Client._workspace(workspace, f"/sessions/{_q(session)}{suffix}")

    @staticmethod
    def _terminal(workspace: str, session: str, terminal: str, suffix: str = "") -> str:
        return Client._session(workspace, session, f"/terminals/{_q(terminal)}{suffix}")

    # -- operations --------------------------------------------------------

    def operation(self, id: str) -> Operation:
        """Operation `id` as it is now, without waiting for it: `wait` blocks."""
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
        check: bool = False,
    ) -> Operation:
        """Block until the operation finishes, or `timeout` seconds pass
        (`TimeoutError`), with `GET /v1/operations/{id}?wait=` long polls:
        the server answers as it finishes, or at its 30 s cap, and this asks
        again. With `check`, a failed or interrupted operation raises
        `OperationFailedError`, and so does a succeeded one whose result
        has a branch that ended `failed`, `budget_exceeded` or
        `interrupted`: `BranchFailedError`, a subclass, naming the branch
        and its status. Without `check`, read `op.failed_branches`."""
        id = operation.id if isinstance(operation, Operation) else operation
        deadline = None if timeout is None else time.monotonic() + timeout
        op = operation if isinstance(operation, Operation) and operation.done else None
        while op is None or not op.done:
            remaining = None if deadline is None else max(0.0, deadline - time.monotonic())
            if op is not None and remaining is not None and remaining <= 0:
                raise TimeoutError(f"operation {id} still {op.state} after {timeout}s")
            ask = _WAIT_CAP if remaining is None else min(_WAIT_CAP, remaining)
            op = Operation(
                self.request(
                    "GET", f"/v1/operations/{_q(id)}", query={"wait": f"{ask:.3f}"}, timeout=ask + self.timeout
                )
            )
        if check and op.failed:
            raise OperationFailedError(op)
        if check and op.failed_branches:
            raise BranchFailedError(op)
        return op

    # -- triggers ----------------------------------------------------------

    def triggers(self) -> list[Record]:
        """The server's triggers (`docs/triggers.md`), as the server lists them."""
        return [Record(t) for t in self.get("/v1/triggers").get("triggers", [])]

    def trigger(self, name_or_id: str) -> Record:
        """One trigger by name or id."""
        return Record(self.get(f"/v1/triggers/{_q(name_or_id)}"))

    def create_trigger(self, spec: JSON) -> Record:
        """`POST /v1/triggers` with a `TriggerSpec` (`docs/triggers.md`):
        `name`, `repo`, `when`, `task`, and `deliver: {"branch": TEMPLATE,
        "busy": "queue" | "steer" | "skip", "lifecycle": {"idle_minutes": N,
        "dormant_days": N, "delete_days": N}}` for a branch that lives on
        (each lifecycle threshold optional, rising when several are given).
        Returns `{"trigger", "secret"?}`."""
        return Record(self.post("/v1/triggers", spec))

    def delete_trigger(self, name_or_id: str) -> None:
        """Delete a trigger by name or id; `NotFoundError` when there is none."""
        self.delete(f"/v1/triggers/{_q(name_or_id)}")

    def enable_trigger(self, name_or_id: str, enabled: bool = True) -> Record:
        """Enable a trigger, or with `enabled=False` disable it; returns it."""
        verb = "enable" if enabled else "disable"
        return Record(self.post(f"/v1/triggers/{_q(name_or_id)}/{verb}"))

    def test_trigger(
        self,
        name_or_id: str,
        event: Optional[JSON] = None,
        event_type: Optional[str] = None,
        run_precheck: bool = False,
    ) -> Record:
        """Evaluate a trigger against `event` (or a sample `event_type` event) without creating anything;
        `run_precheck` also runs its precheck."""
        body = _drop_none({"event": event, "event_type": event_type, "run_precheck": run_precheck})
        return Record(self.post(f"/v1/triggers/{_q(name_or_id)}/test", body))

    def trigger_runs(self, name_or_id: str, limit: Optional[int] = None) -> list[Record]:
        """The trigger's recent runs, newest first, at most `limit`."""
        return [Record(r) for r in self.get(f"/v1/triggers/{_q(name_or_id)}/runs", limit=limit).get("runs", [])]

    def set_trigger_secret(self, name_or_id: str, secret: Optional[str] = None, key: Optional[str] = None) -> Record:
        """`POST /v1/triggers/{t}/secret`: set the webhook secret, or
        generate one when `secret` is None. Returns `{"secret"?}`: the
        generated secret, shown once; nothing for one you gave."""
        body = _drop_none({"secret": secret})
        return Record(self.post(f"/v1/triggers/{_q(name_or_id)}/secret", body, key=key or new_key()))

    # -- the fleet ---------------------------------------------------------

    def well_known(self) -> Record:
        """`GET /.well-known/branchyard`, without the token: what this
        server is (`service`, `version`, `api`, `jwks_uri` when it has
        connectors, `services_uri`), the served `repos`, the `scopes` a
        token may carry, and the live `services`' `id`, `kind`,
        `capabilities` and `health`, not their endpoints or owners."""
        return Record(self.request("GET", "/.well-known/branchyard", headers={"Authorization": None}))

    def inventory(self) -> list[Record]:
        """`GET /v1/inventory`: the live workers serving the caller's
        repositories, the answering server's first (`this`), each with
        `id`, `host`, `labels`, `repos`, `seen_ms_ago` and `inventory`, the
        harnesses its machine has (None when it advertises nothing)."""
        return [Record(w) for w in self.get("/v1/inventory").get("workers", [])]

    def services(self, kind: Optional[str] = None) -> list[Record]:
        """`GET /v1/services`: the fleet's service records, workers among
        them, of `kind` when given, with endpoints, owners, health and
        leases (`docs/registry.md`). The `read` scope."""
        return [Record(s) for s in self.get("/v1/services", kind=kind).get("services", [])]

    def register_service(
        self,
        kind: str,
        *,
        id: Optional[str] = None,
        capabilities: Optional[JSON] = None,
        endpoints: Optional[Sequence[JSON]] = None,
        health: Optional[str] = None,
        weight: Optional[int] = None,
        ttl_seconds: Optional[int] = None,
    ) -> Record:
        """`POST /v1/services`: register a service of `kind` in the fleet's
        registry, or renew one this caller registered (the same `id`; a
        fresh one when absent). `capabilities` maps names to a flag, a
        number, a text or a list of texts; `health` is `healthy` (the
        default), `degraded`, `starting` or `unhealthy`; the lease is
        `ttl_seconds` (30 by default, at most a day). The `admin` scope. A
        live record another principal holds is `ConflictError`
        (`service_held`). Returns the stored record."""
        body = _drop_none(
            {
                "id": id,
                "kind": kind,
                "capabilities": capabilities,
                "endpoints": list(endpoints or ()),
                "health": health,
                "weight": weight,
                "ttl_seconds": ttl_seconds,
            }
        )
        return Record(self.post("/v1/services", body, key=new_key()))

    def deregister_service(self, id: str) -> Record:
        """`DELETE /v1/services/{id}`: deregister a service this caller
        registered; the record as it was left. `ConflictError`
        (`service_held`) when it is not live or another principal holds
        it, `NotFoundError` when there is none. The `admin` scope."""
        return Record(self.delete(f"/v1/services/{_q(id)}"))

    def reclaim_services(self) -> list[Record]:
        """`POST /v1/services/gc`: expire and reclaim now the services whose
        lease ran out, as the server does on its recovery interval; those
        it expired. The `admin` scope."""
        return [Record(s) for s in self.post("/v1/services/gc", {}, key=new_key()).get("services", [])]

    # -- Web Push ----------------------------------------------------------

    def push_info(self) -> Record:
        """`GET /v1/app/push` (`docs/companion.md`): whether the server
        sends Web Push notifications (`enabled`), the VAPID `public_key` a
        subscription is made with, the push `services` an endpoint may
        name, this token's `subscriptions` (their endpoints) and the
        `kinds` a notification can be about. `NotFoundError` when the
        server runs no companion."""
        return Record(self.get("/v1/app/push"))

    def subscribe_push(self, endpoint: str, p256dh: str, auth: str, kinds: Sequence[str] = ()) -> Record:
        """`POST /v1/app/push/subscriptions`: notify a push subscription
        (a browser's `PushSubscription.toJSON()`: its `endpoint` and its
        `keys.p256dh` and `keys.auth`, base64url) for this token, of
        `kinds` (every kind when empty). `NotAllowedError`
        (`push_not_enabled`) on a server that sends none. Returns
        `{"subscriptions": N}`, this token's count now."""
        body: JSON = {"endpoint": endpoint, "keys": {"p256dh": p256dh, "auth": auth}}
        if kinds:
            body["kinds"] = list(kinds)
        return Record(self.post("/v1/app/push/subscriptions", body))

    def unsubscribe_push(self, endpoint: str) -> Record:
        """`DELETE /v1/app/push/subscriptions`: stop notifying `endpoint`,
        if this token subscribed it. `{"subscriptions": N}`."""
        return Record(self.request("DELETE", "/v1/app/push/subscriptions", {"endpoint": endpoint}))

    def test_push(self) -> Record:
        """`POST /v1/app/push/test`: a notification to this token's own
        subscriptions: `subscriptions`, `delivered` (those a push service
        accepted) and `failures`, one line each."""
        return Record(self.post("/v1/app/push/test", {}))


def _q(part: str) -> str:
    return urllib.parse.quote(part, safe="")


def _unreachable(said: str, error: BaseException) -> str:
    """`said`, with the fix appended when nothing listens at the address
    (the server is not running, or the URL names another port)."""
    if isinstance(getattr(error, "reason", error), ConnectionRefusedError):
        return (
            f"{said}. Nothing is listening there. to fix: start a server with `by serve --repo app=.`, "
            'check the URL, or use Client.local(".") for a server of your own'
        )
    return said


def _with_fix(code: str, message: str, detail: Optional[JSON]) -> str:
    """`message` with the remedy the client knows for `code` appended, unless
    the server's own text already carries one."""
    if code == "unknown_repo" and "to fix" not in message.lower():
        served = (detail or {}).get("served")
        names = f"This server serves: {', '.join(map(str, served))}. " if isinstance(served, list) and served else ""
        return (
            f"{message.rstrip('. ')}. {names}to fix: name a served repository (`client.repos()` lists them); "
            "`by serve --repo NAME=.` serves one"
        )
    return message


def _json_or_text(content: bytes) -> Any:
    try:
        return json.loads(content.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        return content.decode("utf-8", "replace")


# ---------------------------------------------------------------------------
# Local mode: a server of this process's own, and this machine's `by`


def _by_command(by: Optional[str]) -> str:
    """The `by` to run: `by` as given, else `BRANCHYARD_BY`, else `by` on
    `PATH`."""
    return by or os.environ.get("BRANCHYARD_BY") or "by"


def _cannot_run(program: str, error: OSError) -> str:
    """Why `program` (a `by`) could not be started, ending with the fix
    where it is known."""
    said = f"could not run {program}: {error}"
    if isinstance(error, FileNotFoundError) and os.sep not in program:
        return (
            f"{said}. `by` is not on PATH (or set BRANCHYARD_BY). to fix: install released binaries with "
            "`curl -fsSL https://github.com/vamsiramakrishnan/branchyard/releases/latest/download/install.sh "
            "| sh -s -- --no-setup`, or build from a checkout with "
            "`cargo install --locked --path crates/branchyard-cli`"
        )
    if isinstance(error, FileNotFoundError):
        return f"{said}. to fix: point BRANCHYARD_BY (or `by=`) at the `by` executable, or put `by` on PATH"
    if isinstance(error, PermissionError):
        return f"{said}. to fix: make it executable (`chmod +x {program}`), or point BRANCHYARD_BY at another `by`"
    return said


def _first_line(process: subprocess.Popen, timeout: float) -> str:
    """The first line `process` prints on its stdout, or what it printed
    before it exited or `timeout` ran out."""
    stdout = process.stdout
    if stdout is None:
        return ""
    deadline = time.monotonic() + timeout
    buffer = bytearray()
    while b"\n" not in buffer:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        ready, _, _ = select.select([stdout], [], [], min(remaining, 0.5))
        if not ready:
            if process.poll() is not None:
                break
            continue
        chunk = os.read(stdout.fileno(), 4096)
        if not chunk:
            break
        buffer += chunk
    return buffer.split(b"\n", 1)[0].decode("utf-8", "replace")


def _drain(process: subprocess.Popen) -> None:
    """Read and drop whatever else `process` prints, so a full pipe never
    stops it."""
    stdout = process.stdout
    if stdout is None:
        return

    def run() -> None:
        with contextlib.suppress(OSError, ValueError):
            while stdout.read(4096):
                pass

    threading.Thread(target=run, name="branchyard-local-stdout", daemon=True).start()


def _stop(process: subprocess.Popen) -> None:
    """Stop `process`: a signal, then force after ten seconds."""
    if process.poll() is None:
        with contextlib.suppress(OSError):
            process.terminate()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            with contextlib.suppress(OSError):
                process.kill()
            with contextlib.suppress(OSError):
                process.wait(timeout=10)
    if process.stdout is not None:
        with contextlib.suppress(OSError):
            process.stdout.close()


def _tail(path: str, lines: int = 5) -> str:
    """The last `lines` lines of the file at `path`, as one line."""
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            text = f.read()
    except OSError:
        return ""
    return " / ".join(line.strip() for line in text.strip().splitlines()[-lines:] if line.strip())


class Step(Record):
    """One step of `by up` (`docs/up.md`): `name` (`harness.codex`,
    `secret.OPENAI_API_KEY`, `connectors`, ...), `state` (`ready`,
    `planned`, `needs_person` or `blocked`), `detail`; `todo` for a step
    that is not ready; `action` (`{"kind": "install", "plan": ...}` or
    `{"kind": "start_gateway"}`) for one `up` does, or may do with your
    yes."""

    @property
    def ready(self) -> bool:
        """Whether the step needs nothing more."""
        return self.get("state") == "ready"


class Plan(Record):
    """What `by up` found and did: `steps`, `state` (the worst of them) and
    `ready`. Nothing in it is a secret's value."""

    @property
    def steps(self) -> list[Step]:
        """Every step, in report order."""
        return [Step(s) for s in self.get("steps") or []]

    @property
    def ready(self) -> bool:
        """Whether everything is in place."""
        return bool(self.get("ready"))

    @property
    def todo(self) -> list[Step]:
        """The steps that are not ready, in report order."""
        return [s for s in self.steps if not s.ready]

    def step(self, name: str) -> Optional[Step]:
        """The step named `name` (`harness.codex`, `connectors`, ...), else None."""
        return next((s for s in self.steps if s.get("name") == name), None)


class Report(Record):
    """`by doctor --json` (`docs/doctor.md`): `checks` (`name`, `status`,
    `detail`, `fix`) and `status`, the worst of them."""

    @property
    def checks(self) -> list[Record]:
        """Every check, in report order."""
        return [Record(c) for c in self.get("checks") or []]

    @property
    def failed(self) -> list[Record]:
        """The checks that failed."""
        return [c for c in self.checks if c.get("status") == "fail"]

    def check(self, name: str) -> Optional[Record]:
        """The check named `name` (`repository.git`, ...), else None."""
        return next((c for c in self.checks if c.get("name") == name), None)


class Local:
    """This machine's `by`, run in the repository at `root` with `--json`,
    for what a server does not do (`docs/sdk.md#local-mode`): `up` brings
    the configured backends up, `doctor` and `harnesses` report. `by` is
    the executable (default: `BRANCHYARD_BY`, else `by` on `PATH`);
    `timeout` bounds each run, an install included. Every method raises
    `LocalError` when `by` cannot run or prints no JSON, and the
    `BranchyardError` of `by`'s own `{"error": {"kind", "message"}}`
    answer, such as a configuration file that does not load (`config`)."""

    def __init__(self, root: str = ".", *, by: Optional[str] = None, timeout: float = 900.0):
        self.root = os.path.abspath(root)
        self.by = _by_command(by)
        self.timeout = timeout

    def run(self, *args: str) -> tuple[int, Any]:
        """`by <args> --json` in the repository: its exit status and the
        JSON it printed. The escape hatch for a command without a method."""
        command = [self.by, *args, "--json"]
        try:
            done = subprocess.run(
                command,
                cwd=self.root,
                stdin=subprocess.DEVNULL,
                capture_output=True,
                timeout=self.timeout,
                check=False,
            )
        except OSError as error:
            raise LocalError(_cannot_run(self.by, error)) from None
        except subprocess.TimeoutExpired:
            raise LocalError(
                f"`{' '.join(command)}` did not finish in {self.timeout:g}s. "
                "to fix: give `Local` a longer timeout=, or run that command yourself to see where it waits"
            ) from None
        try:
            value = json.loads(done.stdout.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
            said = done.stderr.decode("utf-8", "replace").strip() or "nothing"
            raise LocalError(f"`{' '.join(command)}` exited {done.returncode} without JSON; it said: {said}") from None
        if isinstance(value, dict) and set(value) == {"error"} and isinstance(value["error"], dict):
            refusal = value["error"]
            raise error_for_code(
                str(refusal.get("kind") or "error"), str(refusal.get("message") or ""), refusal.get("detail")
            )
        return done.returncode, value

    def up(self, *, plan: bool = False, yes: bool = False) -> Plan:
        """`by up`: install what the `[harnesses]` policy allows, start the
        connector gateway, and list what only you can do; `plan` changes
        nothing and says what would be done; `yes` confirms the installs
        the policy would ask about. `Plan.ready` says whether everything
        is in place."""
        args = ["up"]
        if plan:
            args.append("--plan")
        if yes:
            args.append("--yes")
        _, value = self.run(*args)
        return Plan(value)

    def doctor(self) -> Report:
        """`by doctor`: is this machine and repository ready, check by
        check."""
        _, value = self.run("doctor")
        return Report(value)

    def harnesses(self, *, refresh: bool = False) -> Record:
        """`by harnesses`: this machine's inventory (`harnesses`, each with
        `id`, `path`, `version`, `login`; `tools`), from the last minute's
        detection unless `refresh`."""
        args = ["harnesses"]
        if refresh:
            args.append("--refresh")
        _, value = self.run(*args)
        return Record(value)


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
        body = _task_body(
            prompt,
            harness=harness,
            harnesses=harnesses,
            name=name,
            base=base,
            budget=budget,
            budget_usd=budget_usd,
            policy=policy,
            check=check,
            isolated=isolated,
            connectors=connectors,
            provision=provision,
            delegation=delegation,
            allow_delegation=allow_delegation,
            provider=provider,
            seats=seats,
            priority=priority,
            plan=plan,
            goal=goal,
            **extra,
        )
        return self.client._operation(f"{self._base}/tasks", body, key)

    # -- maps and task records ---------------------------------------------

    def map(
        self,
        prompt: str,
        items: Union[Sequence[Any], dict[str, Any]],
        *,
        name: Optional[str] = None,
        schema: Optional[JSON] = None,
        concurrency: Optional[int] = None,
        retries: Optional[int] = None,
        total_usd: Optional[float] = None,
        reduce: Optional[str] = None,
        remove_done: bool = False,
        retry_failed: bool = False,
        key: Optional[str] = None,
        **task: Any,
    ) -> Operation:
        """`POST .../maps`: a wide map (`docs/map.md`), one branch per item
        running the `prompt` template, as one operation of kind `map`; like
        `by map`. `items` is a list of values, each with an `id` field (a
        string or a whole number), or a dict from id to value: a rerun of
        the map `name` (default: a slug of the prompt) skips the items it
        has done by their ids. `schema` is the JSON Schema every answer must
        match; `task` is every branch's options as `task` takes them
        (`harness`, `budget_usd`, `connectors`, ...), without `name`,
        `harnesses`, `seats`, `plan` or `goal`. A value without an id is
        refused here (`ValueError`): the default id is a BLAKE3 digest,
        which the standard library cannot compute."""
        body: JSON = {
            "name": name,
            "items": _map_items(items),
            "schema": schema,
            "concurrency": concurrency,
            "retries": retries,
            "total_usd": total_usd,
            "reduce": reduce,
            "remove_done": remove_done,
            "retry_failed": retry_failed,
            "task": _drop_none(_task_body(prompt, **task)),
        }
        return self.client._operation(f"{self._base}/maps", body, key)

    def resume_map(self, name: str, *, retry_failed: bool = False, key: Optional[str] = None) -> Operation:
        """`POST .../maps/{name}/resume`: run a recorded map again with the
        request that started it, its done items skipped and, with
        `retry_failed`, its failed ones run again. A map started outside
        this server's API is refused (`InvalidRequestError`)."""
        return self.client._operation(f"{self._base}/maps/{_q(name)}/resume", {"retry_failed": retry_failed}, key)

    def maps(self) -> list[Record]:
        """The recorded maps (`MapSummary`): `name`, `prompt`, `total`,
        `done`, `failed`, `pending`, `running`, `spent_usd`, ..."""
        return [Record(m) for m in self.client.get(f"{self._base}/maps").get("maps", [])]

    def map_report(self, name: str) -> Record:
        """A recorded map's progress and rows (`MapReport`): the counts,
        `columns`, `rows` (each item's latest), `reduce`; like `by map
        show`."""
        return Record(self.client.get(f"{self._base}/maps/{_q(name)}"))

    def remove_map(self, name: str) -> None:
        """Forget a map's record; its branches stay."""
        self.client.delete(f"{self._base}/maps/{_q(name)}")

    def task_records(self) -> list[Record]:
        """The repository's tasks, oldest first, each with its attempts
        (`TaskView`, `docs/task-repos.md`); like `by task ls`."""
        return [Record(t) for t in self.client.get(f"{self._base}/task-records").get("tasks", [])]

    def task_record(self, task: str) -> Record:
        """The task `task` names: its ID, a prefix of at least four
        characters, or one of its attempts; like `by task show`."""
        return Record(self.client.get(f"{self._base}/task-records/{_q(task)}"))

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
        session_patch: Optional[JSON] = None,
        delegation: Optional[JSON] = None,
        allow_delegation: bool = False,
        priority: Optional[int] = None,
        key: Optional[str] = None,
        **extra: Any,
    ) -> Operation:
        """`POST .../send`: another turn on `branch`'s session. A send that
        names no `connectors` keeps the branch's grant; one that does is
        narrowed to it for a delegated child. `409 branch_busy`
        (`BranchBusyError`) while another operation holds the branch.
        `session_patch={"model": ..., "effort": ...}` changes only those
        settings and preserves stored provisioning. It cannot accompany
        `provision` or `connectors`, which remain replacement requests."""
        patch = _session_patch(session_patch, provision is not None or connectors is not None)
        body: JSON = {
            "prompt": prompt,
            "retry": retry,
            "budget": _budget(budget, budget_usd),
            "policy": policy,
            "check": list(check) if check is not None else None,
            "provision": _provision(provision, connectors),
            "session_patch": patch,
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
        """Set `branch` aside on purpose, with `reason`: it runs no more turns and is never integrated; its record is
        kept. Returns its `Inspection`."""
        return Inspection(self.client.post(self._branch(branch, "/discard"), _drop_none({"reason": reason})))

    def fork(
        self,
        branch: str,
        prompt: str,
        *,
        name: Optional[str] = None,
        fresh_session: bool = False,
        at: Optional[int] = None,
        key: Optional[str] = None,
        **options: Any,
    ) -> Operation:
        """A new branch from `branch`'s candidate, continuing its session
        unless `fresh_session`; `options` as `task` takes them (`harness`,
        `budget`, `isolated`, `provision`, ...). The fork keeps its parent's
        grant unless `options["provision"]` gives another.

        With `at`, the new branch starts from `branch`'s checkpoint `at`
        instead (0 is its base), like `by fork --at N`: its session forks
        natively only if it ended there, else a fresh one starts with a
        summary (`docs/checkpoints.md`). Not with `fresh_session`
        (`InvalidRequestError`); a checkpoint the branch does not have is
        refused before anything is queued (`NotFoundError`,
        `unknown_checkpoint`)."""
        body: JSON = {"prompt": prompt, "name": name, "fresh_session": fresh_session, "at": at}
        body.update(options)
        return self.client._operation(self._branch(branch, "/fork"), body, key)

    def rewind(self, branch: str, to: int) -> Rewound:
        """Reset settled `branch`, its worktree and its candidate to its
        checkpoint `to` (0 is its base), like `by rewind --to N`; not an
        operation, so it returns when done. Refused while a turn runs
        (`RunningError`), while an operation holds the branch
        (`BranchBusyError`), for a merged branch or over worktree changes no
        checkpoint has (`DeniedError`), for a checkpoint it does not have
        (`NotFoundError`, `unknown_checkpoint`) and for one whose ref no
        longer names its commit (`ConflictError`, `checkpoint_unavailable`).
        Later checkpoints stay, so rewinding to one of them undoes this."""
        return Rewound(self.client.post(self._branch(branch, "/rewind"), {"to": to}, key=new_key()))

    def reincarnate(
        self, branch: str, *, name: Optional[str] = None, key: Optional[str] = None, **options: Any
    ) -> Operation:
        """A new branch from `branch`'s candidate with a fresh session and a
        generated handoff brief: the way a long conversation is compacted."""
        body: JSON = {"name": name}
        body.update(options)
        return self.client._operation(self._branch(branch, "/reincarnate"), body, key)

    def merge(self, branch: str, target: Optional[str] = None, key: Optional[str] = None) -> Operation:
        """`POST .../merge`: merge `branch` into `target` (default: the branch checked out in the served repository), as
        `by merge`. An `Operation`."""
        return self.client._operation(self._branch(branch, "/merge"), {"target": target}, key)

    def check(self, branch: str, key: Optional[str] = None) -> Operation:
        """`POST .../check`: run `branch`'s check on its current work, the
        way integrating it would, and integrate nothing; as `by check`. The
        check can run as long as an integration's (30 minutes), so this is
        an operation: `client.wait(op, check=True).check_report` says how it
        ended. A check that fails is a succeeded operation whose report is
        not `passed`. It holds no branch lock and runs no turn. Needs the
        `run` scope (`DeniedError` without it)."""
        return self.client._operation(self._branch(branch, "/check"), {}, key)

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
        bindings: Union[Sequence[JSON], dict[str, str], None] = None,
        harnesses: Optional[Sequence[str]] = None,
        plan: bool = False,
        model: Optional[str] = None,
        priority: Optional[int] = None,
        result: Optional[JSON] = None,
        why: Optional[str] = None,
        owns: Optional[Sequence[str]] = None,
        land: bool = False,
        review: Union[str, JSON, None] = None,
        key: Optional[str] = None,
        **extra: Any,
    ) -> Operation:
        """`POST .../spawn`: a child of `parent`, as `by spawn --parent`
        makes one; the server must allow delegation. `connectors` asks for
        the child's grant, which is narrowed to `parent`'s: the result's
        `inspection.grants` is what it got. `result` is the child's result
        contract, `{"schema": {...}, "required": bool}`: the child reports a
        JSON value matching the schema, and `inspection.result` reads it
        back; a schema keyword outside the contract's subset is refused.
        A required deliverable may add capture={manifest, max_files, max_bytes}
        to collect bounded regular files after provider transfer. The receipt
        and artifacts remain separate from the native result value.
        `bindings` is a list of `{"scratch", "access"}` or a dict from
        scratch area to `read_only` or `exclusive_write`. `owns` are the
        paths the child owns, `land` leaves it to the engine to land, and
        `review` is a harness ID or `{"harness", "model", "rubric",
        "budget_usd"}` for the reviewer that reads its work first (needs
        `land`; `docs/delegation.md`)."""
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
            "bindings": _binding_list(bindings),
            "harnesses": list(harnesses) if harnesses is not None else None,
            "plan": plan,
            "model": model,
            "priority": priority,
            "result": result,
            "why": why,
            "owns": list(owns or ()),
            "land": land,
            "review": _reviewer(review),
        }
        body.update(extra)
        return self.client._operation(self._branch(parent, "/spawn"), body, key)

    def integrate(self, branch: str, *, with_: Sequence[str] = (), key: Optional[str] = None) -> Operation:
        """Merge delegated `branch` (and `with_` siblings) into the branch
        that delegated it."""
        return self.client._operation(self._branch(branch, "/integrate"), {"with": list(with_)}, key)

    # -- looking -----------------------------------------------------------

    def branches(self) -> list[BranchInfo]:
        """Every branch of the repository, as `BranchInfo`s."""
        return [BranchInfo(b) for b in self.client.get(f"{self._base}/branches").get("branches", [])]

    def branch(self, name: str) -> BranchInfo:
        """One branch as `BranchInfo`; `NotFoundError` (`unknown_branch`) when there is none."""
        return BranchInfo(self.client.get(self._branch(name)))

    def exists(self, name: str) -> bool:
        """Whether branch `name` exists; any other failure raises."""
        try:
            self.branch(name)
        except NotFoundError as error:
            if error.code == "unknown_branch":
                return False
            raise
        return True

    def remove(self, name: str) -> None:
        """Remove branch `name`, its worktree and its record, as `by rm`; a name used by a merged or discarded branch is
        free again."""
        self.client.delete(self._branch(name))

    def diff(self, name: str) -> str:
        """The unified diff of `name`'s candidate against its base, as text; empty for a branch with no changes."""
        return str(self.client.get(self._branch(name, "/diff")).get("diff", ""))

    def inspect(self, branch: str) -> Inspection:
        """`branch` as a delegating parent sees it, grant included."""
        return Inspection(self.client.get(self._branch(branch, "/inspection")))

    def children(self, branch: str) -> list[BranchInfo]:
        """Every branch `branch` delegated to, directly or below."""
        return [BranchInfo(b) for b in self.client.get(self._branch(branch, "/children")).get("descendants", [])]

    def graph(self, branch: str) -> Record:
        """`branch`'s task graph: its children, their prerequisites and the revision a proposal must be made against
        (`docs/graph.md`)."""
        return Record(self.client.get(self._branch(branch, "/graph")))

    def apply_graph(self, branch: str, expected_revision: int, edits: Sequence[JSON], **options: Any) -> Record:
        """Apply a graph proposal to `branch`'s children against `expected_revision`; `StaleRevisionError` when it moved
        on. `options` are the request's others (`policy`, ...)."""
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
            ask = _WAIT_CAP if remaining is None else min(_WAIT_CAP, remaining)
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
        """`branch`'s own inbox, as `Message`s."""
        return [Message(m) for m in self.client.get(self._branch(branch, "/inbox")).get("messages", [])]

    def handoff(
        self,
        source: str,
        to: str,
        artifacts: Sequence[str] = (),
        note: str = "",
        expected: Optional[JSON] = None,
    ) -> Message:
        """Queue a frozen result and selected artifact grants without starting a recipient turn."""
        body = _drop_none({"from": source, "to": to, "artifacts": list(artifacts), "note": note, "expected": expected})
        return Message(self.client.post(self._branch(source, "/handoff"), body, key=new_key()))

    def ask(self, branch: str, text: str, wait_seconds: Optional[float] = None) -> Record:
        """Ask `branch`'s parent a question, as `branch`. With `wait_seconds` (capped at 120 on the server) it waits for
        the answer, `answer` being None when none came."""
        return Record(
            self.client.post(self._branch(branch, "/ask"), _drop_none({"text": text, "wait_seconds": wait_seconds}))
        )

    def report(self, branch: str, text: str, result: Optional[Any] = None) -> Message:
        """Report to `branch`'s parent. `result` is `branch`'s answer, a JSON
        value checked against its result contract (a mismatch is refused,
        naming the path and the rule) and read back as `inspect(branch)`'s
        `result`."""
        body: JSON = {"text": text}
        if result is not None:  # `{}`, `[]` and `False` are results too
            body["result"] = result
        return Message(self.client.post(self._branch(branch, "/report"), body))

    def escalate(self, branch: str, text: str) -> Message:
        """Escalate `text` as `branch` to its parent, or further up if its rig seat allows."""
        return Message(self.client.post(self._branch(branch, "/escalate"), {"text": text}))

    def report_status(
        self,
        branch: str,
        state: str,
        summary: str,
        link: Optional[str] = None,
        ttl_seconds: Optional[float] = None,
    ) -> Reported:
        """State `branch`'s own status beside the protocol's, as the person
        or surface acting for it (`by report-status --as`): `state` is
        `working`, `idle`, `blocked`, `complete` or `failure`, `summary`
        one line of at most 280 characters, `link` an optional http(s) URL,
        `ttl_seconds` an optional time-to-live (whole seconds, 1 to 604800)
        after which the report reads as stale. A `blocked` or `failure`
        report is also escalated to its parent. A field that breaks a rule
        raises `InvalidRequestError` whose `field` names it. A branch's own
        harness reports through its broker, not this route."""
        body = _drop_none({"state": state, "summary": summary, "link": link, "ttl_seconds": ttl_seconds})
        return Reported(self.client.post(self._branch(branch, "/report-status"), body))

    def answer(self, branch: str, message_id: int, text: str) -> Message:
        """Answer one of `branch`'s descendants' messages, as `branch`."""
        return Message(self.client.post(self._branch(branch, "/answer"), {"message_id": message_id, "text": text}))

    # -- plans -------------------------------------------------------------

    def plan(self, branch: str) -> Record:
        """`branch`'s plan and its phase (`docs/plans-and-goals.md`)."""
        return Record(self.client.get(self._branch(branch, "/plan")))

    def approve_plan(
        self, branch: str, edited: Optional[str] = None, key: Optional[str] = None, **options: Any
    ) -> Operation:
        """Approve `branch`'s plan, optionally replaced by `edited`; the branch then runs it. An `Operation`."""
        body: JSON = {"edited": edited}
        body.update(options)
        return self.client._operation(self._branch(branch, "/plan/approve"), body, key)

    def reject_plan(self, branch: str, reason: str, replan: bool = False, key: Optional[str] = None) -> Operation:
        """Reject `branch`'s plan with `reason`; with `replan` the branch plans again. An `Operation`."""
        return self.client._operation(self._branch(branch, "/plan/reject"), {"reason": reason, "replan": replan}, key)

    # -- knowledge ---------------------------------------------------------

    def _knowledge(self, rest: str = "") -> str:
        return f"{self._base}/knowledge{rest}"

    def knowledge(self, status: Optional[str] = None) -> list[Record]:
        """The repository's knowledge entries (`docs/knowledge.md`), or
        those of `status` (`proposed`, `adopted` or `rejected`); like `by
        knowledge list`. Each `KnowledgeEntry` has `id`, `scope`
        (`{"path"?, "kind"?}`), `text`, `source`, `status`, `created_ms`,
        and `adopted_by`, `decided_ms` and `note` once decided."""
        return [Record(e) for e in self.client.get(self._knowledge(), status=status).get("entries", [])]

    def knowledge_entry(self, id: int) -> Record:
        """One entry; `NotFoundError` (`unknown_knowledge`) when there is
        none."""
        return Record(self.client.get(self._knowledge(f"/{id}")))

    def add_knowledge(
        self, text: str, *, path: Optional[str] = None, kind: Optional[str] = None, propose: bool = False
    ) -> Record:
        """Add an entry as the caller, scoped to files matching the glob
        `path` and to tasks of `kind` when given (the whole repository
        otherwise): adopted at once, or only proposed with `propose`; like
        `by knowledge add`. The `run` scope."""
        body = _drop_none({"text": text, "scope": _drop_none({"path": path, "kind": kind}), "propose": propose})
        return Record(self.client.post(self._knowledge(), body, key=new_key()))

    def adopt_knowledge(self, id: int) -> Record:
        """Adopt a proposed entry as the caller; from the next turn on, it
        is given to the harnesses whose branches it matches."""
        return Record(self.client.post(self._knowledge(f"/{id}/adopt"), {}, key=new_key()))

    def reject_knowledge(self, id: int, reason: Optional[str] = None) -> Record:
        """Reject a proposed knowledge entry, with `reason`."""
        return Record(self.client.post(self._knowledge(f"/{id}/reject"), _drop_none({"reason": reason}), key=new_key()))

    def edit_knowledge(
        self, id: int, *, text: Optional[str] = None, path: Optional[str] = None, kind: Optional[str] = None
    ) -> Record:
        """Change an entry's `text`, `path` or `kind`, each when given; `""`
        clears `path` or `kind`."""
        body = {k: v for k, v in {"text": text, "path": path, "kind": kind}.items() if v is not None}
        return Record(self.client.post(self._knowledge(f"/{id}/edit"), body, key=new_key()))

    def remove_knowledge(self, id: int) -> Record:
        """Remove an entry; the entry as it was."""
        return Record(self.client.delete(self._knowledge(f"/{id}")))

    def export_knowledge(self) -> Record:
        """The adopted entries as an `AGENTS.md`-style file: `markdown`,
        and `entries`, their ids."""
        return Record(self.client.get(self._knowledge("/export")))

    def distill(self, branch: str) -> Record:
        """Propose entries from `branch` with the deterministic extractor
        (the corrections sent into it, review comments), adopting none;
        like `by knowledge distill`. A harness distiller runs only where
        `by knowledge distill --harness` does. `Distilled`: `branch`,
        `proposed` (the new entries), `duplicates`, `by`, `trigger`."""
        return Record(self.client.post(self._branch(branch, "/distill"), {}, key=new_key()))

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
        """Answer an approval no, as the caller (`run` scope)."""
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
        """Perform a staged effect now, as the caller; returns its ledger entry."""
        return Effect(self.client.post(f"{self._base}/effects/{_q(effect_id)}/promote", {}))

    def reconcile_effects(self) -> Record:
        """Reconcile the ledger now, as the server does on its own interval
        (`docs/effects.md`); like `by effects reconcile`. `Reconciled`:
        `settled`, `[id, state]` pairs, and `unknown`, `[id, why]` pairs.
        The `run` scope."""
        return Record(self.client.post(f"{self._base}/effects/reconcile", {}, key=new_key()))

    def undo_plan(self, branch: str, to: int = 0) -> Record:
        """What undoing `branch` to turn `to` (0: every turn) would do
        upstream, changing nothing; like `by undo --plan`. `UndoPlan`:
        `branch`, `to`, the effects after it as `reversible`, `compensable`,
        `irreversible` and `unknown`, the `staged` calls, `at_ms`."""
        return Record(self.client.get(self._branch(branch, "/undo"), to=to))

    def undo(self, branch: str, to: int = 0, only: Sequence[str] = ()) -> Record:
        """Undo, as the caller, the upstream effects of `branch`'s turns
        after `to`: the effects `only` names, or every reversible effect and
        staged call when it is empty. Files are not rewound (`rewind` or a
        local `by undo` does that). `UndoReport`: `plan` and `outcomes`."""
        body: JSON = {"to": to}
        if only:
            body["only"] = list(only)
        return Record(self.client.post(self._branch(branch, "/undo"), body, key=new_key()))

    # -- artifacts and scratch areas ---------------------------------------

    def publish_artifact(
        self,
        branch: str,
        data: bytes,
        name: str,
        media_type: Optional[str] = None,
        key: Optional[str] = None,
        *,
        labels: Optional[dict[str, str]] = None,
        kind: Optional[str] = None,
    ) -> Record:
        """Publish `data` as a new immutable artifact of `branch` called `name`, with `media_type` (default
        `application/octet-stream`) and `labels`. Its ancestors and descendants can read it; a sibling after
        `share_artifact`. `kind` is the old name of `media_type`: the server never took a `kind`, so before this it was
        dropped."""
        query: dict[str, Any] = {
            "name": name,
            "media_type": media_type or kind,
            "label": [f"{k}={v}" for k, v in (labels or {}).items()] or None,
        }
        return Record(
            self.client.request("POST", self._branch(branch, "/artifacts"), data, key=key or new_key(), query=query)
        )

    def artifacts(self, branch: str) -> list[Record]:
        """Every artifact `branch` may read."""
        return [Record(a) for a in self.client.get(self._branch(branch, "/artifacts")).get("artifacts", [])]

    def read_artifact(self, branch: str, artifact_id: str) -> tuple[Record, bytes]:
        """An artifact's record and its content, `(ref, bytes)`."""
        ref = Record(self.client.get(self._branch(branch, f"/artifacts/{_q(artifact_id)}")))
        _, _, content = self.client.request(
            "GET", self._branch(branch, f"/artifacts/{_q(artifact_id)}/content"), raw=True
        )
        return ref, content

    def preview_artifact(
        self,
        branch: str,
        artifact_id: str,
        *,
        view: Optional[str] = None,
        page: Optional[int] = None,
        sheet: Optional[str] = None,
        offset: Optional[int] = None,
        limit: Optional[int] = None,
    ) -> Record:
        """Read an artifact's document preview. `view` is auto, table or pages;
        pages start at one and table offsets at zero. Omitted options use the
        server defaults. Conversion readiness remains in the returned record."""
        return Record(
            self.client.get(
                self._branch(branch, f"/artifacts/{_q(artifact_id)}/preview"),
                view=view,
                page=page,
                sheet=sheet,
                offset=offset,
                limit=limit,
            )
        )

    def read_preview_asset(
        self,
        branch: str,
        artifact_id: str,
        asset: str,
        *,
        view: Optional[str] = None,
        page: Optional[int] = None,
        sheet: Optional[str] = None,
        offset: Optional[int] = None,
        limit: Optional[int] = None,
    ) -> bytes:
        """Read authenticated image bytes named by a preview response. Pass the
        same options as `preview_artifact`; assets are bound to those options.
        The asset is a name, not a URL, and never changes the server origin."""
        _, _, content = self.client.request(
            "GET",
            self._branch(branch, f"/artifacts/{_q(artifact_id)}/preview/assets/{_q(asset)}"),
            query={"view": view, "page": page, "sheet": sheet, "offset": offset, "limit": limit},
            raw=True,
        )
        return content

    def share_artifact(self, branch: str, artifact_id: str, to: str) -> None:
        """Share an artifact `branch` may read with branch `to`."""
        self.client.post(self._branch(branch, f"/artifacts/{_q(artifact_id)}/share"), {"to": to}, key=new_key())

    def create_scratch(self, branch: str, name: str) -> Record:
        """Create scratch area `name`, owned by `branch` and visible to its ancestors and descendants."""
        return Record(self.client.post(self._branch(branch, "/scratch"), {"name": name}, key=new_key()))

    def scratch_areas(self, branch: str) -> list[Record]:
        """Every scratch area `branch` may reach."""
        return [Record(a) for a in self.client.get(self._branch(branch, "/scratch")).get("areas", [])]

    def share_scratch(self, branch: str, name: str, to: str) -> None:
        """Share `branch`'s scratch area `name` with branch `to`."""
        self.client.post(self._branch(branch, f"/scratch/{_q(name)}/share"), {"to": to}, key=new_key())

    def lock_scratch(self, branch: str, name: str, key: Optional[str] = None) -> Record:
        """Acquire scratch area `name`'s writer lock for `branch`, as a
        person acting for it; like `by scratch lock --branch`. Re-entrant
        for its holder; `RunningError` while another branch's turn holds
        it. `ScratchLock`: `name`, `holder_branch`, `acquired_at`."""
        path = self._branch(branch, f"/scratch/{_q(name)}/lock")
        return Record(self.client.post(path, {}, key=key or new_key()))

    def unlock_scratch(self, branch: str, name: str, key: Optional[str] = None) -> None:
        """Release scratch area `name`'s lock if `branch` holds it; like
        `by scratch unlock --branch`. Nothing to do otherwise."""
        self.client.post(self._branch(branch, f"/scratch/{_q(name)}/unlock"), {}, key=key or new_key())

    def scratch_lock_state(self, name: str) -> Optional[Record]:
        """Scratch area `name`'s writer lock, or None when none is held,
        whether or not its turn still runs. Not scoped to a branch."""
        lock = self.client.get(f"{self._base}/scratch/{_q(name)}/lock").get("lock")
        return Record(lock) if isinstance(lock, dict) else None

    # -- surfaces ----------------------------------------------------------

    def surface(self, prefix: str, **defaults: Any) -> "Surface":
        """A `Surface`: one resident branch per conversation, named
        `<prefix>-<key>`, created with `defaults` (as `task` takes them)."""
        return Surface(self, prefix, **defaults)


def _task_body(
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
    **extra: Any,
) -> JSON:
    """A `TaskRequest`, as `Repo.task` and each item of `Repo.map` send it."""
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
    return body


# The longest explicit map item id, as the server's `item_id` takes one.
_ITEM_ID_MAX = 100


def _map_items(items: Union[Sequence[Any], dict[str, Any]]) -> list[JSON]:
    """`MapItem`s, `{"id", "value"}`: a dict's keys as the ids, else each
    value's own `id` field, read as `branchyard::item_id` reads it."""
    if isinstance(items, dict):
        return [{"id": str(id), "value": value} for id, value in items.items()]
    out: list[JSON] = []
    for value in items:
        given = value.get("id") if isinstance(value, dict) else None
        if isinstance(given, str) and given.strip() and len(given.strip()) <= _ITEM_ID_MAX:
            id = given.strip()
        elif isinstance(given, int) and not isinstance(given, bool):
            id = str(given)
        elif given is None:
            raise ValueError(
                f"map item {value!r} has no id: give each an `id` field, or the items as a dict from id to value"
            )
        else:
            raise ValueError(
                f"map item {value!r}: its id must be a non-empty string of at most {_ITEM_ID_MAX} "
                "characters or a whole number"
            )
        out.append({"id": id, "value": value})
    return out


def _budget(budget: Optional[JSON], budget_usd: Optional[float]) -> Optional[JSON]:
    if budget_usd is None:
        return budget
    merged = dict(budget or {})
    merged["max_usd"] = budget_usd
    return merged


_ACCESS = {"read_only": "read_only", "ro": "read_only", "exclusive_write": "exclusive_write", "rw": "exclusive_write"}


def _binding_list(bindings: Union[Sequence[JSON], dict[str, str], None]) -> list[JSON]:
    """A spawn's scratch bindings as the server takes them, `{"scratch",
    "access"}` each, from that list or from `branchyard.spawn`'s dict of
    scratch area to access (`read_only`, `exclusive_write`, or `ro`, `rw`)."""
    if isinstance(bindings, dict):
        return [
            {"scratch": str(name), "access": _ACCESS.get(str(access), str(access))} for name, access in bindings.items()
        ]
    return list(bindings or ())


def _binding_map(bindings: Union[Sequence[JSON], dict[str, str], None]) -> Optional[dict[str, str]]:
    """The same bindings as `branchyard.spawn` takes them, a dict from
    scratch area to access; None when there are none."""
    if isinstance(bindings, dict):
        return dict(bindings) or None
    return {str(b["scratch"]): str(b["access"]) for b in bindings or ()} or None


def _reviewer(review: Union[str, JSON, None]) -> Optional[JSON]:
    """A reviewer as the server takes it, from a harness ID or a dict of
    `harness`, `model`, `rubric` and `budget_usd`: what `branchyard.spawn`
    accepts, so one call means the same inside and outside."""
    if review is None:
        return None
    if isinstance(review, str):
        return {"harness": review}
    if isinstance(review, dict) and "harness" in review and set(review) <= {"harness", "model", "rubric", "budget_usd"}:
        return dict(review)
    raise ValueError('review must be a harness ID or {"harness": ..., "model": ..., "rubric": ..., "budget_usd": ...}')


def _provision(provision: Optional[JSON], connectors: Optional[Grants]) -> Optional[JSON]:
    if connectors is None:
        return provision
    merged = dict(provision or {})
    merged["connectors"] = grants_json(connectors)
    return merged


def _session_patch(patch: Optional[JSON], replacing: bool = False) -> Optional[JSON]:
    """Validate operator session edits without treating empty grants as omissions."""
    if patch is None:
        return None
    if replacing:
        raise ValueError("session_patch and provision/connectors are mutually exclusive")
    if not isinstance(patch, dict) or not patch or not set(patch) <= {"model", "effort"}:
        raise ValueError("session_patch needs only model or effort")
    clean = {key: value for key, value in patch.items() if value is not None}
    if not clean:
        raise ValueError("session_patch needs a model or effort")
    if "model" in clean and (not isinstance(clean["model"], str) or not clean["model"].strip()):
        raise ValueError("session_patch model must not be blank")
    if "effort" in clean and (
        not isinstance(clean["effort"], str) or clean["effort"] not in {"low", "medium", "high", "xhigh"}
    ):
        raise ValueError("session_patch effort must be low, medium, high or xhigh")
    return clean


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
    which may `close()` it. `for entry in stream` is the loop; `with
    repo.stream() as stream:` closes it however the block ends."""

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

    def __enter__(self) -> "EventStream":
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

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
        """Stop and close the connection; safe to call twice, and from `on_idle`."""
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
            raise TransportError(_unreachable(f"GET {url}: {error}", error)) from None

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
        """Whether the conversation's branch exists yet: it is made by the first message."""
        return self.repo.exists(self.branch)

    def inspect(self) -> Inspection:
        """The conversation's branch as an `Inspection`: `turns`, `cost_usd`, `remaining_usd`, its grant."""
        return self.repo.inspect(self.branch)

    def history(self, limit: int = 50) -> list[JSON]:
        """The most recent `limit` recorded events."""
        return list(self.repo.event_page(self.branch, limit=limit).get("events") or ())

    def cancel(self) -> list[str]:
        """Stop the turn running on the conversation's branch; the names cancelled."""
        return self.repo.cancel(self.branch)

    def steer(self, text: str) -> Steer:
        """Join `text` to the running turn as steered input, without interrupting it (`say(busy="steer")` falls back
        when that is refused)."""
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
        """Whether the message was steered into a running turn instead of starting one."""
        return self.steer is not None

    def wait(self, timeout: Optional[float] = None) -> "Reply":
        """Block until the operation finishes (`Client.wait`'s long poll);
        `OperationFailedError` when it failed, `BranchFailedError` (a
        subclass) when its turn ended with the branch `failed`,
        `budget_exceeded` or `interrupted`, `TimeoutError` past `timeout`.
        Then `text` is ready, also after either error: what the harness
        said before it stopped."""
        if self.operation is None:
            raise ValueError("no operation to wait for: the message was steered into a turn without one")
        try:
            self.operation = self.repo.client.wait(self.operation, timeout=timeout, check=True)
        except OperationFailedError as error:
            self.operation = error.operation
            raise
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


# ---------------------------------------------------------------------------
# One API inside and outside a harness


class Waited(Record):
    """What `Delegator.wait_any` and `wait_all` return, as `by wait --json`
    and `POST .../wait` give it: `settled` (`Inspection`s), `pending` (the
    names still running) and `timed_out`."""

    @property
    def settled(self) -> list[Inspection]:
        """The `Inspection` of each branch that settled."""
        return [Inspection(i) for i in self.get("settled") or ()]

    @property
    def pending(self) -> list[str]:
        """The names of the branches still running."""
        return list(self.get("pending") or ())


def connect(
    target: Union[Client, Repo, None] = None, *, repo: Optional[str] = None, branch: Optional[str] = None
) -> "Delegator":
    """The delegation calls (`spawn`, `send`, `steer`, `integrate`, `check`,
    `wait`, `wait_any`, `wait_all`, `inspect`, `events`, `report`,
    `report_status`) as one object, so the same code runs inside a harness
    turn and outside it (`docs/sdk.md#one-api-inside-and-outside-a-harness`).

    Given a `Client` or a `Repo`, it speaks HTTP to that server, acting as
    `branch` (the branch it spawns under and reports as). Given neither:
    inside a delegating turn (`BRANCHYARD_DELEGATION` set) it runs
    `sdk/python/branchyard.py`'s functions, acting as the turn's branch;
    else, with `BRANCHYARD_REMOTE` set, it makes a `Client` from it,
    `BRANCHYARD_TOKEN_FILE` and `BRANCHYARD_CA_FILE`, as `by --remote`
    does; else it runs `branchyard.py` with your authority on the
    repository here. `repo` (default `BRANCHYARD_REPO`, else the server's
    only one) picks a `Client`'s repository; `branch` defaults to
    `BRANCHYARD_BRANCH`."""
    branch = branch or os.environ.get("BRANCHYARD_BRANCH") or None
    if target is None and not os.environ.get("BRANCHYARD_DELEGATION"):
        remote = os.environ.get("BRANCHYARD_REMOTE")
        if remote:
            target = Client(
                remote,
                token_file=os.environ.get("BRANCHYARD_TOKEN_FILE") or None,
                ca_file=os.environ.get("BRANCHYARD_CA_FILE") or None,
            )
    if isinstance(target, Client):
        name = repo or os.environ.get("BRANCHYARD_REPO")
        if not name:
            served = [str(r.name) for r in target.repos()]
            if len(served) != 1:
                raise InvalidRequestError(
                    "invalid_request", f"the server serves {', '.join(served) or 'nothing'}; pass repo=NAME"
                )
            name = served[0]
        target = target.repo(name)
    return Delegator(target, branch)


class Delegator:
    """`connect()`'s object: `branchyard.py`'s delegation calls, with its
    argument names and order, run through `by --json` inside a harness
    (`repo` is None) or over HTTP on `repo`, acting as `branch`.

    Both return the same types: `Record`s holding the whole JSON answer
    (`BranchInfo`, `Inspection`, `Steer`, `Message`, `Reported`,
    `CheckReport`, `Waited`), which lose no field where `branchyard.py`'s
    dataclasses keep only those they name; and both raise the same
    exceptions, by code. Over HTTP each call that is an operation is
    waited for, and one that fails raises the error for its code (with
    `operation` set), as the same call inside would. The server's spawn and
    send operations last until the turn they start settles, so outside,
    `spawn` and `send` return a settled branch where inside they return as
    its turn starts."""

    def __init__(self, repo: Optional[Repo] = None, branch: Optional[str] = None):
        self.repo = repo
        self.branch = branch

    @property
    def inside(self) -> bool:
        """Whether the calls run through `by --json` (`branchyard.py`)."""
        return self.repo is None

    # -- the two backends --------------------------------------------------

    def _by(self, function: str, *args: Any, **options: Any) -> Any:
        import branchyard

        getattr(branchyard, function)(*args, **options)
        return branchyard._answer.value

    def _http(self) -> Repo:
        assert self.repo is not None
        return self.repo

    def _acting(self, call: str) -> str:
        if not self.branch:
            raise InvalidRequestError(
                "invalid_request", f"{call} over HTTP acts as a branch: connect(..., branch=NAME)"
            )
        return self.branch

    def _done(self, op: Operation) -> Operation:
        op = self._http().client.wait(op)
        if op.failed:
            error = op.error or {}
            raised = error_for_code(
                str(error.get("code", op.state)),
                str(error.get("message", f"operation {op.id} {op.state}")),
                error.get("detail"),
            )
            raised.operation = op
            raise raised
        return op

    # -- delegating --------------------------------------------------------

    def spawn(
        self,
        prompt: str,
        harness: Optional[str] = None,
        name: Optional[str] = None,
        base: Optional[str] = None,
        budget_usd: Optional[float] = None,
        max_turns: Optional[int] = None,
        max_minutes: Optional[float] = None,
        check: Union[str, Sequence[str], None] = None,
        max_depth: Optional[int] = None,
        max_children: Optional[int] = None,
        harnesses: Optional[Sequence[str]] = None,
        deny: Optional[Sequence[str]] = None,
        seat: Optional[str] = None,
        depends_on: Optional[Sequence[str]] = None,
        after: Optional[str] = None,
        bindings: Union[Sequence[JSON], dict[str, str], None] = None,
        connectors: Optional[Grants] = None,
        plan: bool = False,
        model: Optional[str] = None,
        result: Optional[JSON] = None,
        why: Optional[str] = None,
        owns: Optional[Sequence[str]] = None,
        land: bool = False,
        review: Union[str, JSON, None] = None,
        **options: Any,
    ) -> BranchInfo:
        """A child of this branch running `prompt`; its `BranchInfo`. The
        arguments are `branchyard.spawn`'s, by name and in order, and mean
        the same on both sides (`docs/sdk.md#one-api-inside-and-outside-a-harness`):
        `check` is a command line or its argv, `max_minutes` the turn's
        time limit, `connectors` grants in any form `Grant` takes,
        `bindings` a dict from scratch area to `read_only` or
        `exclusive_write`, and `review` a harness ID or `{"harness",
        "model", "rubric", "budget_usd"}`. `options` go to the backend's
        own spawn as they are (`Repo.spawn`'s over HTTP: `policy`,
        `priority`, ...), so they are not portable."""
        if self.inside:
            return BranchInfo(
                self._by(
                    "spawn",
                    prompt,
                    harness=harness,
                    name=name,
                    base=base,
                    budget_usd=budget_usd,
                    max_turns=max_turns,
                    max_minutes=max_minutes,
                    check=check if check is None or isinstance(check, str) else _shell_line(check),
                    max_depth=max_depth,
                    max_children=max_children,
                    harnesses=list(harnesses) if harnesses is not None else None,
                    deny=list(deny) if deny is not None else None,
                    seat=seat,
                    depends_on=list(depends_on) if depends_on else None,
                    after=after,
                    bindings=_binding_map(bindings),
                    connectors=[str(Grant.from_json(g)) for g in connectors] if connectors is not None else None,
                    plan=plan,
                    model=model,
                    result=result,
                    why=why,
                    owns=list(owns) if owns is not None else None,
                    land=land,
                    review=review,
                    **options,
                )
            )
        budget: JSON = {}
        if max_turns is not None:
            budget["max_turns"] = max_turns
        if max_minutes is not None:
            budget["max_seconds"] = max_minutes * 60
        op = self._http().spawn(
            self._acting("spawn"),
            prompt,
            harness=harness,
            name=name,
            base=base,
            budget=budget or None,
            budget_usd=budget_usd,
            check=_shell_words(check) if isinstance(check, str) else check,
            connectors=connectors,
            max_depth=max_depth,
            max_children=max_children,
            deny=deny,
            seat=seat,
            depends_on=depends_on,
            after=after,
            bindings=bindings,
            harnesses=harnesses,
            plan=plan,
            model=model,
            result=result,
            why=why,
            owns=owns,
            land=land,
            review=review,
            **options,
        )
        return self._done(op).result_branches[0]

    def send(
        self, branch: str, prompt: Optional[str] = None, retry: bool = False, *, session_patch: Optional[JSON] = None
    ) -> BranchInfo:
        """`branch`'s next turn with `prompt`, or with `retry` its cut-off
        one again; its `BranchInfo` (inside, `name` and `status` only)."""
        if self.inside:
            if session_patch is not None:
                raise DeniedError("denied", "session_patch is an operator send option; a child keeps its provisioning")
            return BranchInfo(self._by("send", branch, prompt, retry=retry))
        if retry and prompt is not None:
            raise DeniedError("denied", "retry submits the cut-off turn's own prompt; give no prompt")
        if not retry and prompt is None:
            raise DeniedError("denied", "send needs a prompt, or retry=True")
        return self._done(self._http().send(branch, prompt, retry=retry, session_patch=session_patch)).result_branches[
            0
        ]

    def steer(self, branch: str, text: str) -> Steer:
        """`text` into `branch`'s running turn, without interrupting it."""
        if self.inside:
            return Steer(self._by("steer", branch, text))
        return self._http().steer(branch, text)

    def integrate(self, *branches: str) -> Record:
        """Merge finished descendants into their parent after their check:
        a `Merged` for one, a `MergedAll` (`branches`, `checks`,
        `check_results`) for several. A failed check raises
        `CheckFailedError` (`CheckTimedOutError`, `CheckNotStartedError`)."""
        if not branches:
            raise DeniedError("denied", "name at least one branch to integrate")
        if self.inside:
            return Record(self._by("integrate", *branches))
        op = self._done(self._http().integrate(branches[0], with_=branches[1:]))
        result = op.get("result") or {}
        return Record(result.get("merged" if len(branches) == 1 else "merged_all") or {})

    def check(self, branch: Optional[str] = None) -> CheckReport:
        """Run a branch's check on its current work and integrate nothing:
        this branch's when `branch` is None."""
        if self.inside:
            return CheckReport(self._by("check", branch))
        op = self._done(self._http().check(branch or self._acting("check")))
        return op.check_report or CheckReport({})

    # -- waiting and looking -----------------------------------------------

    def _wait(self, branches: Sequence[str], any: bool, timeout: Optional[float]) -> Waited:
        if self.inside:
            # `branchyard.py` raises the RunningError of a timeout itself.
            return Waited(self._by("wait_any" if any else "wait_all", *branches, timeout=timeout))
        waited = Waited(self._http().wait_for(branches, any=any, timeout=timeout))
        if waited.get("timed_out"):
            raise RunningError("running", f"still running after {timeout}s: {', '.join(waited.pending)}")
        return waited

    def wait(self, branch: str, timeout: Optional[float] = None) -> Inspection:
        """Block until `branch` settles; its `Inspection`. `RunningError`
        after `timeout` seconds."""
        return self._wait([branch], False, timeout).settled[0]

    def wait_any(self, *branches: str, timeout: Optional[float] = None) -> Waited:
        """Block until the first of `branches` settles."""
        return self._wait(branches, True, timeout)

    def wait_all(self, *branches: str, timeout: Optional[float] = None) -> Waited:
        """Block until all of `branches` have settled."""
        return self._wait(branches, False, timeout)

    def inspect(self, branch: Optional[str] = None) -> Inspection:
        """A branch's state: this branch's when `branch` is None."""
        if self.inside:
            return Inspection(self._by("inspect", branch))
        return self._http().inspect(branch or self._acting("inspect"))

    def events(self, branch: Optional[str] = None, cursor: Optional[int] = None, limit: Optional[int] = None) -> Record:
        """An `EventPage` of a branch's recorded events (`branch`, `events`,
        `next_cursor`, `total`); pass `next_cursor` back as `cursor`."""
        if self.inside:
            return Record(self._by("events", branch, cursor, limit))
        return self._http().event_page(branch or self._acting("events"), cursor, limit)

    def handoff(
        self,
        source: str,
        to: str,
        artifacts: Optional[Sequence[str]] = None,
        note: str = "",
        expected: Optional[JSON] = None,
    ) -> Message:
        """Relay a coworker's settled result; the recipient retains its native execution."""
        if self.inside:
            return Message(self._by("handoff", source, to, list(artifacts or ()), note, expected))
        return self._http().handoff(source, to, artifacts or (), note, expected)

    # -- reporting, as this branch -----------------------------------------

    def report(self, text: str, result: Optional[Any] = None) -> Message:
        """Report to this branch's parent, with `result` as its answer."""
        if self.inside:
            return Message(self._by("report", text, result))
        return self._http().report(self._acting("report"), text, result)

    def report_status(
        self, state: str, summary: str, link: Optional[str] = None, ttl_seconds: Optional[int] = None
    ) -> Reported:
        """State this branch's own status beside the protocol's."""
        if self.inside:
            return Reported(self._by("report_status", state, summary, link, ttl_seconds))
        return self._http().report_status(self._acting("report_status"), state, summary, link, ttl_seconds)


def _shell_line(argv: Sequence[str]) -> str:
    import shlex

    return shlex.join(argv)


def _shell_words(line: str) -> list[str]:
    import shlex

    return shlex.split(line)
