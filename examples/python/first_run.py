#!/usr/bin/env python3
"""First run: ask for one change from Python, wait for it, read how it ended.

In a git repository with at least one commit, with `by` on PATH (or in
BRANCHYARD_BY) and the SDK installed:

    pip install ./sdk/python                       # from a checkout; standard library only
    python3 examples/python/first_run.py "Make the flaky parser test deterministic"

There is no server to start and no token to copy. `Client.local` runs a
`by serve` of this process's own on the repository, on a free loopback
port, and stops it when the `with` block ends. The branch it makes lives in
the repository's `.branchyard/`, so afterwards `by diff NAME`, `by merge
NAME` and `by rm NAME` work on it as on any branch. The script prints
how the branch ended and a summary of its diff, and exits 0 for a branch
that is ready (or that changed nothing). A harness that fails, runs out of
budget or is interrupted raises `BranchFailedError` from
`wait(check=True)`: the script prints why and exits 1.

The harness is the repository's default; `--harness codex` names another.
Its tool requests are answered by `policy`: the default here, the
`edit-worktree` preset, lets it read and edit files and refuses commands and
the web (docs/egress.md#permission-presets). `--yes` allows everything, as
`by run --yes` does.

To try it without a model, `--command` runs the harness as a stand-in
command, such as the fake ACP agent a checkout builds
(`cargo build -p branchyard-runtime --bin fake-acp-agent`), whose prompt
`WRITE hello.txt=hi` writes that file:

    python3 examples/python/first_run.py "WRITE hello.txt=hi" \\
        --command target/debug/fake-acp-agent
"""

from __future__ import annotations

import argparse
import os
import sys
import time

from branchyard_client import BranchFailedError, BranchyardError, Client


def serve_args(harness: str, command: str | None) -> list[str]:
    """`by serve` flags: run `harness` as `command` when one is given. The server runs in the repository, so a
    relative path to the program is made absolute here, where the user typed it."""
    if not command:
        return []
    program, *rest = command.split()
    if os.path.exists(program):
        program = os.path.abspath(program)
    return ["--harness-command", f"{harness}={' '.join([program, *rest])}"]


def changed_files(diff: str) -> list[str]:
    """The paths a unified diff touches, in order."""
    return [line.split(" b/", 1)[1] for line in diff.splitlines() if line.startswith("diff --git ")]


def run(args: argparse.Namespace) -> int:
    """Ask for the change, wait for it, print how it ended."""
    # A stand-in command needs a harness ID to answer to; any ID the server knows will do.
    harness = args.harness or ("gemini-cli" if args.command else None)
    name = args.name or f"first-run-{time.strftime('%H%M%S')}"
    policy = {"mode": "allow"} if args.yes else {"preset": "edit-worktree"}

    with Client.local(args.repo, args=serve_args(harness or "gemini-cli", args.command)) as client:
        app = client.repo("app")
        op = app.task(args.prompt, name=name, harness=harness, budget_usd=args.budget_usd, policy=policy)
        # Raises BranchFailedError when the request ran but the harness's turn did not do the work.
        op = client.wait(op, timeout=args.timeout, check=True)

        branch = op.result_branches[0]
        print(f"{branch.name}: {branch.state}")
        candidate = branch.candidate
        if candidate:
            print(
                f"  {candidate['files_changed']} file(s) changed, "
                f"+{candidate['insertions']} -{candidate['deletions']} at {candidate['commit'][:10]}"
            )
            for path in changed_files(app.diff(branch.name)):
                print(f"  {path}")
            print(f"next: by diff {branch.name}    # then: by merge {branch.name}, or by rm {branch.name}")
        else:
            print("  no changes")
            print(f"next: by log {branch.name}    # what it said; by rm {branch.name} when done")
    return 0


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="One change, from Python, end to end.")
    parser.add_argument("prompt", nargs="?", default="Add a file hello.txt that says hello")
    parser.add_argument("--repo", default=".", help="the repository (default: here)")
    parser.add_argument("--harness", help="harness ID, such as codex (default: the repository's)")
    parser.add_argument("--name", help="branch name (default: first-run-<time>)")
    parser.add_argument("--budget-usd", type=float, default=1.0, help="the branch's cost limit (default: 1)")
    parser.add_argument("--timeout", type=float, default=1800.0, help="seconds to wait for the branch")
    parser.add_argument("--yes", action="store_true", help="allow every tool request, not only edits")
    parser.add_argument("--command", help="run the harness as this command, a stand-in for a real one")
    args = parser.parse_args(argv)
    try:
        return run(args)
    except BranchFailedError as error:
        print(f"{error.branch}: {error.state}")
        print(f"first_run: {error.message}", file=sys.stderr)
        return 1
    except (BranchyardError, TimeoutError) as error:
        print(f"first_run: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
