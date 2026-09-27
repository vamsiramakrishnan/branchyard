"""Delegate work to child branches from Python, by running `by ... --json`.

Inside a harness that Branchyard runs with delegation, every call acts as
that harness's branch and only on its descendants: `by` finds the branch
from the BRANCHYARD_DELEGATION token the engine set, and the engine checks
it. Outside a harness, `by` acts with your authority on the repository.

Standard library only. `by` is BRANCHYARD_BY when set, else `by` on PATH.
Every failure raises a BranchyardError subclass carrying the error's kind,
as `by --json` reported it:

    import branchyard

    child = branchyard.spawn("Make the parser test deterministic",
                             harness="codex", budget_usd=0.5)
    done = branchyard.wait(child.name)
    if done.status["state"] == "ready":
        branchyard.integrate(child.name)

Not guaranteed: anything `by` does not. This module adds no authority,
retries nothing, and waits only when asked.
"""

import dataclasses
import json
import os
import subprocess
import time
from typing import Any, Dict, List, Optional

__all__ = [
    "BranchyardError",
    "DeniedError",
    "RunningError",
    "NotFoundError",
    "Spawned",
    "Inspection",
    "EventPage",
    "Sent",
    "Merged",
    "Cancelled",
    "Children",
    "spawn",
    "inspect",
    "events",
    "send",
    "integrate",
    "cancel",
    "children",
    "wait",
]


class BranchyardError(Exception):
    """`by` failed. `kind` is the stable error kind, such as "denied"."""

    def __init__(self, kind: str, message: str):
        super().__init__(message)
        self.kind = kind
        self.message = message


class DeniedError(BranchyardError):
    """Refused by the envelope, the budget or the authority check."""


class RunningError(BranchyardError):
    """The branch is running a turn and must be idle for this."""


class NotFoundError(BranchyardError):
    """No such branch."""


_KINDS = {"denied": DeniedError, "running": RunningError, "unknown_branch": NotFoundError}


@dataclasses.dataclass
class Spawned:
    name: str
    git_branch: str
    harness: str
    profile: str
    base: str
    depth: int
    status: Dict[str, Any]
    budget: Dict[str, Any]
    # The rig seat the child fills; None when it was spawned without one.
    seat: Optional[str] = None


@dataclasses.dataclass
class Inspection:
    name: str
    status: Dict[str, Any]
    harness: str
    profile: str
    parent: Optional[str]
    children: List[str]
    depth: int
    turns: int
    candidate: Optional[Dict[str, Any]]
    cost_usd: Optional[float]
    subtree_cost_usd: float
    max_usd: Optional[float]
    remaining_usd: Optional[float]
    envelope: Optional[Dict[str, Any]]
    last_message: str
    # In a rig: the seat the branch fills and the seats it may spawn.
    seat: Optional[str] = None
    seats: Optional[List[str]] = None

    @property
    def running(self) -> bool:
        return self.status.get("state") == "running"


@dataclasses.dataclass
class EventPage:
    branch: str
    events: List[Dict[str, Any]]
    next_cursor: int
    total: int


@dataclasses.dataclass
class Sent:
    name: str
    status: Dict[str, Any]


@dataclasses.dataclass
class Merged:
    branch: str
    target: str
    previous: str
    commit: str


@dataclasses.dataclass
class Cancelled:
    cancelled: List[str]


@dataclasses.dataclass
class Children:
    branch: str
    descendants: List[Dict[str, Any]]


def _by() -> str:
    return os.environ.get("BRANCHYARD_BY") or "by"


def _run(args: List[str]) -> Any:
    try:
        done = subprocess.run(
            [_by(), args[0], "--json", *args[1:]],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError as error:
        raise BranchyardError("unavailable", f"could not run {_by()}: {error}") from error
    try:
        value = json.loads(done.stdout) if done.stdout.strip() else None
    except json.JSONDecodeError:
        value = None
    if done.returncode != 0 or value is None:
        error = value.get("error") if isinstance(value, dict) else None
        if isinstance(error, dict):
            kind = str(error.get("kind", "error"))
            raise _KINDS.get(kind, BranchyardError)(kind, str(error.get("message", "")))
        detail = done.stderr.strip() or f"exit status {done.returncode}"
        raise BranchyardError("error", f"by {args[0]}: {detail}")
    return value


def _make(cls, value: Dict[str, Any]):
    names = {field.name for field in dataclasses.fields(cls)}
    return cls(**{key: value.get(key) for key in names})


def spawn(
    prompt: str,
    harness: Optional[str] = None,
    name: Optional[str] = None,
    base: Optional[str] = None,
    budget_usd: Optional[float] = None,
    max_turns: Optional[int] = None,
    max_minutes: Optional[float] = None,
    check: Optional[str] = None,
    max_depth: Optional[int] = None,
    deny: Optional[List[str]] = None,
    seat: Optional[str] = None,
) -> Spawned:
    """Create a child branch and start it; returns once it has started.

    In a rig, `seat` names the seat to fill: it sets the child's harness,
    limits, check and instructions, and the other arguments may only narrow
    them. `inspect().seats` lists the seats you may spawn.
    """
    options = {
        "--seat": seat,
        "--harness": harness,
        "--name": name,
        "--base": base,
        "--budget-usd": budget_usd,
        "--max-turns": max_turns,
        "--max-minutes": max_minutes,
        "--check": check,
        "--max-depth": max_depth,
        "--deny": ",".join(deny) if deny else None,
    }
    flags: List[str] = []
    for flag, value in options.items():
        if value is not None:
            flags += [flag, str(value)]
    return _make(Spawned, _run(["spawn", *flags, "--", prompt]))


def inspect(branch: Optional[str] = None) -> Inspection:
    """A branch's state: your own when `branch` is None, or a descendant's."""
    return _make(Inspection, _run(["inspect"] + ([branch] if branch else [])))


def events(branch: Optional[str] = None, cursor: Optional[int] = None,
           limit: Optional[int] = None) -> EventPage:
    """Recorded events; pass `next_cursor` back as `cursor` to continue."""
    args = ["events"] + ([branch] if branch else [])
    if cursor is not None:
        args += ["--cursor", str(cursor)]
    if limit is not None:
        args += ["--limit", str(limit)]
    return _make(EventPage, _run(args))


def send(branch: str, prompt: str) -> Sent:
    """Start a descendant's next turn with `prompt`."""
    return _make(Sent, _run(["send", branch, "--", prompt]))


def integrate(branch: str) -> Merged:
    """Merge a finished descendant into your own branch after its check passes."""
    return _make(Merged, _run(["integrate", branch]))


def cancel(branch: str) -> Cancelled:
    """Stop a descendant's turn and every turn running below it."""
    return _make(Cancelled, _run(["cancel", branch]))


def children() -> Children:
    """Your branch's descendants."""
    return _make(Children, _run(["children"]))


def wait(branch: str, timeout: Optional[float] = None, poll: float = 1.0) -> Inspection:
    """Inspect `branch` until it is not running; RunningError after `timeout` seconds."""
    deadline = None if timeout is None else time.monotonic() + timeout
    while True:
        state = inspect(branch)
        if not state.running:
            return state
        if deadline is not None and time.monotonic() >= deadline:
            raise RunningError("running", f"{branch} is still running after {timeout}s")
        time.sleep(poll)
