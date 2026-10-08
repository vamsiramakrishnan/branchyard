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
    if done.status.state == "ready":       # or done.status["state"]
        branchyard.integrate(child.name)

A harness may also end its turn while its children run: Branchyard starts
its next turn once they have all settled, with a summary of what each did.

Not guaranteed: anything `by` does not. This module adds no authority,
retries nothing, and waits only when asked.
"""

import dataclasses
import json
import os
import subprocess
from typing import Any, Dict, List, Optional

__all__ = [
    "BranchyardError",
    "DeniedError",
    "RunningError",
    "NotRunningError",
    "SteerRefusedError",
    "CheckFailedError",
    "CheckTimedOutError",
    "CheckNotStartedError",
    "NotFoundError",
    "StaleRevisionError",
    "Status",
    "Spawned",
    "Inspection",
    "EventPage",
    "Sent",
    "Steer",
    "Merged",
    "MergedAll",
    "CheckReport",
    "Waited",
    "Cancelled",
    "Children",
    "ArtifactRef",
    "ScratchArea",
    "ScratchLock",
    "Message",
    "Asked",
    "Inbox",
    "Graph",
    "GraphApplied",
    "spawn",
    "inspect",
    "events",
    "send",
    "steer",
    "integrate",
    "check",
    "cancel",
    "discard",
    "children",
    "graph",
    "apply_graph",
    "wait",
    "wait_any",
    "wait_all",
    "publish",
    "list_artifacts",
    "get_artifact",
    "share_artifact",
    "create_scratch",
    "list_scratch",
    "share_scratch",
    "lock_scratch",
    "unlock_scratch",
    "ask",
    "report",
    "escalate",
    "answer",
    "inbox",
    "approve_plan",
    "reject_plan",
]


class BranchyardError(Exception):
    """`by` failed. `kind` is the stable error kind, such as "denied", and
    `detail` what the error carries beyond its message (None when nothing),
    as `by --json` reported them."""

    def __init__(self, kind: str, message: str, detail: Optional[Dict[str, Any]] = None):
        super().__init__(message)
        self.kind = kind
        self.message = message
        self.detail = detail


class DeniedError(BranchyardError):
    """Refused by the envelope, the budget or the authority check."""


class RunningError(BranchyardError):
    """The branch is running a turn and must be idle for this."""


class NotRunningError(BranchyardError):
    """The branch is not running a turn, and steering needs one."""


class SteerRefusedError(BranchyardError):
    """The running turn did not take steered input; the message says why."""


class CheckFailedError(BranchyardError):
    """A branch's check failed on its merge; nothing was integrated.

    When siblings left out of the integration share the check and could
    change the result (a parent's whole-suite check may pass only with all
    of them, and a branch integrated failed its `by check` alone too),
    `integrate_together` names the branches to integrate at once,
    `integrate(*e.integrate_together)`, and `unsettled` those of them still
    to wait for. Both are empty otherwise: the check failed on its own, and
    when siblings share it, `own_work` names the branches integrated, on
    whose own work it failed.

    `checks` names every check of the branches integrated, in the order
    they ran, as dicts `{check, branches, outcome}`: `outcome` is "passed",
    "failed", "timed_out", "not_started" or "not_run" (a check before it
    failed).
    """

    @property
    def checks(self) -> List[Dict[str, Any]]:
        return _checks(self.detail)

    @property
    def integrate_together(self) -> List[str]:
        return list((self.detail or {}).get("integrate_together", []))

    @property
    def unsettled(self) -> List[str]:
        return list((self.detail or {}).get("unsettled", []))

    @property
    def own_work(self) -> List[str]:
        return list((self.detail or {}).get("own_work", []))


class CheckTimedOutError(BranchyardError):
    """A branch's check ran past its timeout on the merge; nothing was
    integrated. `checks` is as CheckFailedError's, the timed-out one's
    outcome "timed_out" (empty for a single branch's merge)."""

    @property
    def checks(self) -> List[Dict[str, Any]]:
        return _checks(self.detail)


class CheckNotStartedError(BranchyardError):
    """A branch's check could not be started on the merge; nothing was
    integrated. `checks` is as CheckFailedError's, the one that could not
    start "not_started" (empty for a single branch's merge)."""

    @property
    def checks(self) -> List[Dict[str, Any]]:
        return _checks(self.detail)


def _checks(detail: Optional[Dict[str, Any]]) -> List[Dict[str, Any]]:
    # An integration's checks, as a check error's `detail` carries them.
    return list((detail or {}).get("checks", []))


class NotFoundError(BranchyardError):
    """No such branch."""


class StaleRevisionError(BranchyardError):
    """A graph proposal was made against a revision that moved on; read `graph()` again."""


class Status(dict):
    """A branch's status as `by --json` gives it, such as
    {"state": "budget_exceeded", "limit": "max_usd"}: still a dict, so
    `status["state"]` works, whose keys also read as attributes:
    `status.state`, `status.reason`, `status.limit`, `status.target`,
    `status.commit` (None when the state has no such field). It compares
    equal to its state's name: `status == "ready"`.
    """

    _FIELDS = ("state", "reason", "limit", "target", "commit")

    def __getattr__(self, name: str) -> Any:
        try:
            return self[name]
        except KeyError:
            if name in Status._FIELDS:
                return None
            raise AttributeError(name) from None

    def __eq__(self, other: object) -> bool:
        if isinstance(other, str):
            return self.get("state") == other
        return dict.__eq__(self, other)

    def __ne__(self, other: object) -> bool:
        return not self.__eq__(other)

    __hash__ = None  # type: ignore[assignment]

    def __str__(self) -> str:
        return str(self.get("state", ""))


_KINDS = {
    "denied": DeniedError,
    "running": RunningError,
    "not_running": NotRunningError,
    "steer_refused": SteerRefusedError,
    "unknown_branch": NotFoundError,
    "check_failed": CheckFailedError,
    "check_timed_out": CheckTimedOutError,
    "check_not_started": CheckNotStartedError,
    "stale_revision": StaleRevisionError,
}


@dataclasses.dataclass
class Spawned:
    name: str
    git_branch: str
    harness: str
    profile: str
    base: str
    depth: int
    status: Status
    budget: Dict[str, Any]
    # The rig seat the child fills; None when it was spawned without one.
    seat: Optional[str] = None
    # The siblings it waits for; it is {"state": "waiting"} until they settle.
    depends_on: Optional[List[str]] = None
    # The check its merge must pass when it is integrated, and whether it
    # is its parent's: siblings sharing a whole-suite check are integrated
    # together, integrate(a, b).
    check: Optional[List[str]] = None
    check_inherited: Optional[bool] = None
    # The model its harness runs; None for the harness's default.
    model: Optional[str] = None


@dataclasses.dataclass
class Inspection:
    name: str
    status: Status
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
    # Its own graph's revision (None while 0), what it waits for, and the
    # scratch areas it is bound to; see docs/graph.md.
    graph_revision: Optional[int] = None
    depends_on: Optional[List[Dict[str, Any]]] = None
    bindings: Optional[List[Dict[str, Any]]] = None
    stalled: Optional[bool] = None
    # What its live children (running, waiting, blocked, awaiting plan
    # approval) hold of its budget, how many they are, and what its settled
    # and removed children spent; see docs/delegation.md "Budgets".
    reserved_usd: Optional[float] = None
    reserving_children: Optional[int] = None
    settled_children_usd: Optional[float] = None
    # What its children may run, as harness= names it: the envelope's list,
    # or its own profile when that list is empty ("its own only").
    allowed_harnesses: Optional[List[str]] = None
    # The check its merge must pass, whether it is its parent's, and the
    # siblings sharing it, which are integrated together with it.
    check: Optional[List[str]] = None
    check_inherited: Optional[bool] = None
    check_shared_with: Optional[List[str]] = None
    # The model its harness runs; None for the harness's default.
    model: Optional[str] = None

    @property
    def state(self) -> Optional[str]:
        """The status's state, such as "ready": `status.state`."""
        return self.status.get("state")

    @property
    def running(self) -> bool:
        return self.status.get("state") == "running"

    @property
    def waiting(self) -> bool:
        """Created with prerequisites that have not all settled yet."""
        return self.status.get("state") == "waiting"

    @property
    def settled(self) -> bool:
        """Not running, waiting for prerequisites or waiting on its children."""
        return self.status.get("state") not in ("running", "waiting", "waiting_on_children")


@dataclasses.dataclass
class EventPage:
    branch: str
    events: List[Dict[str, Any]]
    next_cursor: int
    total: int


@dataclasses.dataclass
class Sent:
    name: str
    status: Status


@dataclasses.dataclass
class Steer:
    id: int
    branch: str
    by: str
    text: str
    requested_at_ms: int
    # {"state": "pending" | "written" | "accepted"}; refusals raise.
    # "accepted": it joined the running turn, which delivers it at
    # `boundary`; "written": the harness has not confirmed it yet.
    state: Status
    boundary: Optional[str] = None


@dataclasses.dataclass
class Merged:
    branch: str
    target: str
    previous: str
    commit: str
    # The target already contained the candidate: recorded as merged through
    # `via`, the commit that brought it in.
    already: Optional[bool] = None
    via: Optional[str] = None


@dataclasses.dataclass
class MergedAll:
    target: str
    previous: str
    commit: str
    # Merges stack: each one's `previous` is the merge before it.
    branches: List[Merged]
    # The distinct checks that ran once on the result, as argv lists.
    checks: Optional[List[List[str]]] = None
    # The same, as dicts {check, branches, outcome}: whose check each is.
    check_results: Optional[List[Dict[str, Any]]] = None


@dataclasses.dataclass
class CheckReport:
    branch: str
    # The check as an argv list (its own, or inherited from
    # `inherited_from`); None when it has none, and nothing ran.
    check: Optional[List[str]]
    # Its current work as a commit, committed nowhere.
    work: str
    # "passed", "failed", "timed_out", "not_started", or "not_run" when
    # there is no check or the work conflicts with `target`.
    outcome: str
    inherited_from: Optional[str] = None
    # The parent's git branch the work was merged into, as integrate would.
    target: Optional[str] = None
    # The commit the check ran on.
    checked: Optional[str] = None
    conflicts: Optional[List[str]] = None
    output_tail: str = ""

    @property
    def passed(self) -> bool:
        """Whether integrating the work as it is would get past its check."""
        if self.check is None:
            return not self.conflicts
        return self.outcome == "passed"


@dataclasses.dataclass
class Waited:
    settled: List[Inspection]
    pending: List[str]
    timed_out: Optional[bool] = None


@dataclasses.dataclass
class Cancelled:
    cancelled: List[str]
    # The branch had already stopped, so nothing changed; `note` says what
    # it is and what to do instead. A cancel is never refused for that.
    already: Optional[bool] = None
    note: Optional[str] = None


@dataclasses.dataclass
class Children:
    branch: str
    descendants: List[Dict[str, Any]]


@dataclasses.dataclass
class Graph:
    """A branch's children, the dependencies among them, and its revision."""

    branch: str
    revision: int
    children: List[Dict[str, Any]]
    dependencies: List[Dict[str, Any]]


@dataclasses.dataclass
class GraphApplied:
    branch: str
    revision: int
    spawned: List[Spawned]
    dependencies: List[Dict[str, Any]]


@dataclasses.dataclass
class ArtifactRef:
    """An immutable published artifact's provenance; see `docs/storage.md`."""

    id: str
    digest: str
    size: int
    name: str
    media_type: str
    publisher_branch: str
    turn: int
    created_at: int
    labels: Dict[str, str]


@dataclasses.dataclass
class ScratchArea:
    name: str
    owner_branch: str
    created_at: int


@dataclasses.dataclass
class ScratchLock:
    name: str
    holder_branch: str
    acquired_at: int


@dataclasses.dataclass
class Message:
    id: int
    from_: str
    to: str
    kind: str
    text: str
    in_reply_to: Optional[int]
    at_ms: int
    delivered: bool


def _message(value: Dict[str, Any]) -> Message:
    # "from" is a Python keyword; the wire field is renamed on the way in.
    return Message(
        id=value["id"],
        from_=value["from"],
        to=value["to"],
        kind=value["kind"],
        text=value["text"],
        in_reply_to=value.get("in_reply_to"),
        at_ms=value["at_ms"],
        delivered=value.get("delivered", False),
    )


@dataclasses.dataclass
class Asked:
    message: Message
    # The reply, once one arrived within `wait`; `None` without a wait, or
    # if it passed with no answer yet.
    answer: Optional[Message]


@dataclasses.dataclass
class Inbox:
    branch: str
    messages: List[Message]
    # Ids of messages steered into your running turn: delivered, but not
    # read until your next step, so unread=True still lists them.
    steered_this_turn: Optional[List[int]] = None


def _by() -> str:
    return os.environ.get("BRANCHYARD_BY") or "by"


def _run(args: List[str], result_on_failure: bool = False, stdin: Optional[str] = None) -> Any:
    # `stdin`, when given, is what `by` reads on its standard input, such as
    # a prompt too long for a command line (`--prompt-file -`).
    try:
        done = subprocess.run(
            [_by(), args[0], "--json", *args[1:]],
            input=stdin,
            stdin=subprocess.DEVNULL if stdin is None else None,
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
    if result_on_failure and isinstance(value, dict) and "error" not in value:
        return value
    if done.returncode != 0 or value is None:
        error = value.get("error") if isinstance(value, dict) else None
        if isinstance(error, dict):
            kind = str(error.get("kind", "error"))
            detail = error.get("detail")
            raise _KINDS.get(kind, BranchyardError)(
                kind, str(error.get("message", "")), detail if isinstance(detail, dict) else None
            )
        detail = done.stderr.strip() or f"exit status {done.returncode}"
        raise BranchyardError("error", f"by {args[0]}: {detail}")
    return value


def _make(cls, value: Dict[str, Any]):
    names = {field.name for field in dataclasses.fields(cls)}
    made = {key: value.get(key) for key in names}
    # A status (and a steer's state) reads as attributes too.
    for key in ("status", "state"):
        found = made.get(key)
        if isinstance(found, dict):
            made[key] = Status(found)
    return cls(**made)


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
    max_children: Optional[int] = None,
    harnesses: Optional[List[str]] = None,
    deny: Optional[List[str]] = None,
    seat: Optional[str] = None,
    depends_on: Optional[List[str]] = None,
    after: Optional[str] = None,
    bindings: Optional[Dict[str, str]] = None,
    connectors: Optional[List[str]] = None,
    plan: bool = False,
    model: Optional[str] = None,
) -> Spawned:
    """Create a child branch and start it; returns once it has started.

    In a rig, `seat` names the seat to fill: it sets the child's harness,
    limits, check and instructions, and the other arguments may only narrow
    them. `inspect().seats` lists the seats you may spawn.

    With `depends_on` (other children of yours), the child is created
    waiting and starts once each has settled, or, with
    `after="integrated"`, once you integrated each. `bindings` maps scratch
    area names to "read_only" or "exclusive_write". See docs/graph.md.

    `connectors` are the child's connector grants, as `--connector` takes
    them ("github:read", "github:write:issues.*"); they are narrowed to
    yours, and one you hold nothing of is refused. Unset, the child has
    your grant (or its seat's). See docs/connectors.md.

    `max_children` and `harnesses` narrow what the child may delegate;
    `plan=True` has it plan first, read-only, and escalate its plan to you
    (approve_plan or reject_plan).

    `model` is the model the child's harness runs, or a size alias
    ("small", "medium", "large", "extra-large") where it defines one: a
    cheaper one suits mechanical work. Unset, it is the seat's or yours. A
    harness whose driver cannot choose a model refuses it.
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
        "--max-children": max_children,
        "--harnesses": ",".join(harnesses) if harnesses else None,
        "--deny": ",".join(deny) if deny else None,
        "--depends-on": ",".join(depends_on) if depends_on else None,
        "--after": after,
        "--model": model,
    }
    flags: List[str] = []
    for flag, value in options.items():
        if value is not None:
            flags += [flag, str(value)]
    for grant in connectors or []:
        flags += ["--connector", grant]
    for scratch, access in (bindings or {}).items():
        flags += ["--bind", f"{scratch}:{access}"]
    if plan:
        flags.append("--plan")
    # On stdin, so a prompt of any length fits: a command-line argument is
    # limited to 128 KiB on Linux.
    return _make(Spawned, _run(["spawn", *flags, "--prompt-file", "-"], stdin=prompt))


def graph(branch: Optional[str] = None) -> Graph:
    """Your graph (or a descendant's): children, dependencies, revision."""
    return _make(Graph, _run(["graph", "show"] + ([branch] if branch else [])))


def apply_graph(edits: List[Dict[str, Any]], expected_revision: int) -> GraphApplied:
    """Apply a graph proposal to your children, all or nothing.

    `edits` are dicts tagged by "kind": {"kind": "spawn", "prompt": ...,
    "name": ..., "depends_on": [...], ...}, {"kind": "add_dependency",
    "dependent": ..., "prerequisite": ...} or {"kind": "remove_dependency",
    ...}. `expected_revision` is `graph().revision`; if the graph moved on,
    nothing changes and StaleRevisionError is raised.
    """
    value = _run(["graph", "apply", "--edits", json.dumps(edits),
                  "--expected-revision", str(expected_revision)])
    applied = _make(GraphApplied, value)
    applied.spawned = [_make(Spawned, s) for s in value.get("spawned", [])]
    return applied


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


def send(branch: str, prompt: Optional[str] = None, retry: bool = False) -> Sent:
    """Start a descendant's next turn with `prompt`.

    With `retry=True` and no prompt, submit again the prompt of its last
    turn that was cut off when the engine running it stopped (its recovery
    note says so); a child whose harness never recorded a session starts a
    fresh one that begins with every prompt it was given.
    """
    if retry:
        if prompt is not None:
            raise DeniedError("denied", "retry submits the cut-off turn's own prompt; give no prompt")
        return _make(Sent, _run(["send", branch, "--retry"]))
    if prompt is None:
        raise DeniedError("denied", "send needs a prompt, or retry=True")
    return _make(Sent, _run(["send", branch, "--", prompt]))


def steer(branch: str, text: str) -> Steer:
    """Add `text` to a descendant's running turn without interrupting it.

    Waits briefly for delivery. Raises NotRunningError when it runs no turn,
    SteerRefusedError when its harness did not take the input, and a
    BranchyardError of kind "unsupported" when its harness cannot take input
    mid-turn.
    """
    return _make(Steer, _run(["send", branch, "--steer", "--", text]))


def integrate(*branches: str):
    """Merge finished descendants into your own branch after their check
    passes. One branch returns a Merged; several are merged together, in
    order, all or none, with their check run once on the result, and return
    a MergedAll. A candidate your branch already contains is recorded as
    merged (already=True), not refused. A failed check raises
    CheckFailedError: its `checks` name every check, the branches it
    belongs to and how it ended, and when siblings sharing it were left
    out, its `integrate_together` names the branches to integrate at once.
    A check that timed out raises CheckTimedOutError, and one that could
    not start CheckNotStartedError, each with the same `checks`."""
    if not branches:
        raise DeniedError("denied", "name at least one branch to integrate")
    value = _run(["integrate", *branches])
    if len(branches) == 1:
        return _make(Merged, value)
    return MergedAll(
        target=value["target"],
        previous=value["previous"],
        commit=value["commit"],
        branches=[_make(Merged, item) for item in value["branches"]],
        checks=value.get("checks"),
        check_results=value.get("check_results"),
    )


def check(branch: Optional[str] = None) -> CheckReport:
    """Run a branch's check (its own, or the one it inherited) on its
    current work the way integrate would, and integrate nothing: your own
    when `branch` is None, or a descendant's. Its files as they are,
    uncommitted ones included, are merged into its parent's work (its
    uncommitted files too, as integrating snapshots them) in a private
    worktree. A check that fails is a CheckReport whose `passed` is
    False, not an error. Run it on yourself before you finish."""
    value = _run(["check"] + ([branch] if branch else []), result_on_failure=True)
    report = _make(CheckReport, value)
    if report.output_tail is None:
        report.output_tail = ""
    return report


def cancel(branch: str) -> Cancelled:
    """Stop a descendant's turn and every turn running below it."""
    return _make(Cancelled, _run(["cancel", branch]))


def discard(branch: str, reason: Optional[str] = None) -> Inspection:
    """Set a settled descendant aside: it ends `discarded` with `reason`,
    keeps its record and cost, is never integrated, and frees its slot in
    your max_children. A running one is refused: cancel it first."""
    args = ["discard", branch]
    if reason is not None:
        args += ["--reason", reason]
    return _make(Inspection, _run(args))


def children() -> Children:
    """Your branch's descendants."""
    return _make(Children, _run(["children"]))


def ask(text: str, wait: Optional[float] = None) -> Asked:
    """Ask your parent a question. Without `wait`, returns once it is sent.

    With `wait`, blocks on `by`'s side for up to that many seconds for an
    answer; a wait that passes with no answer yet is not an error, `answer`
    is just `None`: check `inbox()` later, or `ask` again.
    """
    args = ["ask"]
    if wait is not None:
        args += ["--wait", str(wait)]
    args += ["--", text]
    value = _run(args)
    return Asked(
        message=_message(value["message"]),
        answer=_message(value["answer"]) if value.get("answer") else None,
    )


def report(text: str) -> Message:
    """Report to your parent; no answer is expected."""
    return _message(_run(["report", "--", text]))


def escalate(text: str) -> Message:
    """Escalate to your parent, or, if your rig seat's `escalates_to`
    allows it, an ancestor further up."""
    return _message(_run(["escalate", "--", text]))


def answer(message_id: int, text: str) -> Message:
    """Answer a message (usually a question) from one of your own
    descendants."""
    return _message(_run(["answer", str(message_id), "--", text]))


def approve_plan(branch: str, edited: Optional[str] = None) -> Sent:
    """Approve a descendant's plan that awaits approval, as proposed or with
    `edited` in its place, and start the turn that carries it out."""
    if edited is None:
        return _make(Sent, _run(["plan", "approve", "--", branch]))
    import tempfile

    with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False) as file:
        file.write(edited)
    try:
        return _make(Sent, _run(["plan", "approve", "--file", file.name, "--", branch]))
    finally:
        os.unlink(file.name)


def reject_plan(branch: str, reason: Optional[str] = None, replan: bool = False) -> Sent:
    """Reject a descendant's plan: it ends, or with `replan=True` it plans
    again, read-only, with `reason`."""
    args = ["plan", "reject"]
    if reason is not None:
        args += ["--reason", reason]
    if replan:
        args.append("--replan")
    return _make(Sent, _run(args + ["--", branch]))


def inbox(unread: bool = False) -> Inbox:
    """Every message addressed to you, oldest first; `unread=True` for only
    what has not yet been delivered to a turn."""
    args = ["inbox"] + (["--unread"] if unread else [])
    value = _run(args)
    return Inbox(
        branch=value["branch"],
        messages=[_message(m) for m in value["messages"]],
        steered_this_turn=value.get("steered_this_turn"),
    )


def _wait(branches: List[str], any_: bool, timeout: Optional[float]) -> Waited:
    args = ["wait", *branches]
    if any_:
        args.append("--any")
    if timeout is not None:
        args += ["--timeout", str(timeout)]
    value = _run(args, result_on_failure=True)
    waited = Waited(
        settled=[_make(Inspection, item) for item in value.get("settled", [])],
        pending=list(value.get("pending", [])),
        timed_out=value.get("timed_out"),
    )
    if waited.timed_out:
        raise RunningError(
            "running", f"still running after {timeout}s: {', '.join(waited.pending)}"
        )
    return waited


def wait_any(*branches: str, timeout: Optional[float] = None) -> Waited:
    """Block until the first of `branches` (default: your children still
    running) settles; RunningError after `timeout` seconds. It waits on the
    engine's notifications, not by polling."""
    return _wait(list(branches), True, timeout)


def wait_all(*branches: str, timeout: Optional[float] = None) -> Waited:
    """Block until all of `branches` (default: your children still running)
    have settled; RunningError after `timeout` seconds."""
    return _wait(list(branches), False, timeout)


def wait(branch: str, timeout: Optional[float] = None, poll: float = 1.0) -> Inspection:
    """Wait until `branch` is neither running nor waiting (for its
    prerequisites or on its children) and return its inspection;
    RunningError after `timeout` seconds. `poll` is ignored: the wait is
    the engine's (`by wait`)."""
    del poll
    return _wait([branch], False, timeout).settled[0]


def publish(path: str, name: Optional[str] = None,
            labels: Optional[Dict[str, str]] = None,
            media_type: Optional[str] = None) -> ArtifactRef:
    """Publish the file at `path` as a new immutable artifact of your
    branch, content-addressed by its digest (the blake3 hash of its bytes,
    in hex), with `media_type` (default application/octet-stream).
    Ancestors and descendants of your branch can read it; a sibling needs
    share_artifact.
    """
    args = ["artifact", "publish", path]
    if name is not None:
        args += ["--name", name]
    if media_type is not None:
        args += ["--media-type", media_type]
    for key, value in (labels or {}).items():
        args += ["--label", f"{key}={value}"]
    return _make(ArtifactRef, _run(args))


def list_artifacts() -> List[ArtifactRef]:
    """Every artifact you may read."""
    return [_make(ArtifactRef, item) for item in _run(["artifact", "list"])]


def get_artifact(artifact_id: str, out: str) -> ArtifactRef:
    """Copy an artifact's bytes to `out`, checked against its digest."""
    return _make(ArtifactRef, _run(["artifact", "get", artifact_id, "--out", out]))


def share_artifact(artifact_id: str, to: str) -> None:
    """Share an artifact you may read with branch `to`."""
    _run(["artifact", "share", artifact_id, "--to", to])


def create_scratch(name: str) -> ScratchArea:
    """Create scratch area `name`, owned by your branch, visible to your
    ancestors and descendants at BRANCHYARD_SCRATCH_<NAME>.
    """
    return _make(ScratchArea, _run(["scratch", "create", name]))


def list_scratch() -> List[ScratchArea]:
    """Every scratch area you may reach."""
    return [_make(ScratchArea, item) for item in _run(["scratch", "list"])]


def share_scratch(name: str, to: str) -> None:
    """Share scratch area `name` with branch `to`."""
    _run(["scratch", "share", name, "--to", to])


def lock_scratch(name: str) -> ScratchLock:
    """Acquire scratch area `name`'s writer lock for your branch."""
    return _make(ScratchLock, _run(["scratch", "lock", name]))


def unlock_scratch(name: str) -> None:
    """Release scratch area `name`'s lock if your branch holds it."""
    _run(["scratch", "unlock", name])
