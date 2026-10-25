#!/usr/bin/env python3
"""Stream a branch's activity as it happens, until the branch settles.

In a git repository with at least one commit, with `by` on PATH (or in
BRANCHYARD_BY) and the SDK installed:

    pip install ./sdk/python                       # from a checkout; standard library only
    python3 examples/python/stream_events.py "Make the flaky parser test deterministic"

`Client.local` runs a `by serve` of this process's own on the repository
(see first_run.py). The script asks for a change, opens the repository's
event stream from the position the request was admitted at (`op.cursor`, so
nothing the branch does before the first read is missed) and prints one line
per entry: the branch's status changes, the prompt, what the harness says as
it says it, the candidate it produced. It stops at the first status that
means the branch is not going to move without you: ready, no changes, failed,
over budget or interrupted. Exit 0 when the branch is ready or changed nothing,
1 otherwise.

The same loop serves a bot that mirrors every branch of a server into a
channel: `client.repo("app").stream()` from now, without `branches=`, and keep
`stream.cursor` to resume after a restart (docs/sdk.md#streaming).

`--command` runs the harness as a stand-in command, as in first_run.py:

    python3 examples/python/stream_events.py "WRITE hello.txt=hi" \\
        --command target/debug/fake-acp-agent
"""

from __future__ import annotations

import argparse
import os
import sys
import time

from branchyard_client import BranchyardError, Client, FeedEntry

# The states a branch stays in until a person or its parent does something.
SETTLED = ("ready", "no_changes", "failed", "budget_exceeded", "interrupted")
OK = ("ready", "no_changes")


def serve_args(harness: str, command: str | None) -> list[str]:
    """`by serve` flags: run `harness` as `command` when one is given. The server runs in the repository, so a
    relative path to the program is made absolute here, where the user typed it."""
    if not command:
        return []
    program, *rest = command.split()
    if os.path.exists(program):
        program = os.path.abspath(program)
    return ["--harness-command", f"{harness}={' '.join([program, *rest])}"]


def describe(entry: FeedEntry) -> str | None:
    """One line for a feed entry, or None for those not worth a line."""
    activity = entry.get("activity")
    if not isinstance(activity, dict):
        return None
    if entry.status is not None:
        state = entry.status.state
        why = entry.status.get("reason") or entry.status.get("limit")
        return f"status: {state}" + (f" ({why})" if why else "")
    if entry.text:
        return f"says: {entry.text.rstrip()}"
    if "prompt" in activity:
        return f"prompt: {activity['prompt']}"
    if "snapshot" in activity:
        snapshot = activity["snapshot"]
        return f"candidate: {snapshot['files_changed']} file(s), +{snapshot['insertions']} -{snapshot['deletions']}"
    event = entry.harness_event
    if event is not None and event.get("type") not in ("message_delta", "ready", "session_closed"):
        return f"harness: {event.get('type')}"
    return None


def run(args: argparse.Namespace) -> int:
    """Ask for the change, print its activity until it settles."""
    harness = args.harness or ("gemini-cli" if args.command else None)
    name = args.name or f"stream-{time.strftime('%H%M%S')}"
    policy = {"mode": "allow"} if args.yes else {"preset": "edit-worktree"}
    serve = serve_args(harness or "gemini-cli", args.command)

    final = ""
    with Client.local(args.repo, args=serve) as client:
        app = client.repo("app")
        op = app.task(args.prompt, name=name, harness=harness, budget_usd=args.budget_usd, policy=policy)
        # From where the request was admitted, for this branch only; `with` closes the connection.
        with app.stream(op.cursor, branches=[name], read_timeout=5.0) as stream:

            def stop_if_over() -> None:
                # A quiet stream and a finished operation: it was refused before the branch ran.
                if client.operation(op.id).done:
                    stream.close()

            stream.on_idle = stop_if_over
            for entry in stream:
                line = describe(entry)
                if line:
                    print(f"{entry.seq:>4}  {entry.branch}  {line}", flush=True)
                if entry.status is not None and entry.status.state in SETTLED:
                    final = entry.status.state
                    break
        op = client.wait(op, timeout=60)  # it ends right after the branch settles
        if op.failed:
            print(f"stream_events: the request {op.state}: {(op.error or {}).get('message', '')}", file=sys.stderr)
            return 1
    print(f"{name}: {final}")
    return 0 if final in OK else 1


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="Print a branch's activity until it settles.")
    parser.add_argument("prompt", nargs="?", default="Add a file hello.txt that says hello")
    parser.add_argument("--repo", default=".", help="the repository (default: here)")
    parser.add_argument("--harness", help="harness ID, such as codex (default: the repository's)")
    parser.add_argument("--name", help="branch name (default: stream-<time>)")
    parser.add_argument("--budget-usd", type=float, default=1.0, help="the branch's cost limit (default: 1)")
    parser.add_argument("--yes", action="store_true", help="allow every tool request, not only edits")
    parser.add_argument("--command", help="run the harness as this command, a stand-in for a real one")
    try:
        return run(parser.parse_args(argv))
    except (BranchyardError, TimeoutError) as error:
        print(f"stream_events: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
