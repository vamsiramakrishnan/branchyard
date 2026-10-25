#!/usr/bin/env python3
"""Delegate a subtask to a child branch, wait for it, and land what it made.

This is the meta-harness shape: code that splits work across child branches
and integrates the results. `connect()` gives it the same calls in both
places it can run, so the script is written once:

* Inside a harness turn that Branchyard runs with delegation. The harness
  runs this script as a shell command (Branchyard sets BRANCHYARD_DELEGATION
  for the turn), and every call acts as that turn's branch, on its own
  descendants only, through `by ... --json` (sdk/python/branchyard.py):

      by run "Delegate the parser fix: run python3 examples/python/delegate.py" --allow-delegation

* Outside a turn, over HTTP, as a branch you name on a `by serve` that
  allows delegation (docs/sdk.md#one-api-inside-and-outside-a-harness):

      by serve --repo app=. --allow-delegation           # prints the URL; the token is in .branchyard/server/token
      BRANCHYARD_REMOTE=http://127.0.0.1:8421 BRANCHYARD_TOKEN_FILE=.branchyard/server/token \\
      BRANCHYARD_BRANCH=lead python3 examples/python/delegate.py "Fix the flaky parser test"

  where `lead` is a branch that was started with a delegation envelope.

Outside both, `by` has no branch to act as, so the script says so and exits 2.
It cannot be tried in one process like first_run.py: the inside form needs a
harness that runs it, and the outside form needs a server and a lead branch.
tests/test_sdk_examples.py runs the outside form against a server of its own and the fake ACP agent.

It spawns one child, waits for it to settle, and integrates it into the
branch it acts as when it is ready. `integrate` runs the child's check on the
merge; a failed check goes back to the child as its next turn, and the script
exits 1. Over HTTP `spawn` returns once the child's first turn has settled;
inside a harness it returns as the turn starts: `wait` covers both.
"""

from __future__ import annotations

import argparse
import os
import sys

from branchyard_client import BranchyardError, CheckFailedError, RunningError, connect

NOT_A_BRANCH = (
    "delegate: nothing to act as. Run this inside a harness turn that Branchyard started with delegation, "
    "or set BRANCHYARD_REMOTE, BRANCHYARD_TOKEN_FILE and BRANCHYARD_BRANCH to act as a branch on a `by serve` "
    "that allows delegation (see this file's docstring)."
)


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="Spawn one child, wait for it, integrate it.")
    parser.add_argument("prompt", nargs="?", default="Add a file notes.txt that says what this repository is for")
    parser.add_argument("--name", help="the child's branch name (default: from the prompt)")
    parser.add_argument("--harness", help="the child's harness (default: this branch's)")
    parser.add_argument("--budget-usd", type=float, default=0.5, help="the child's cost limit (default: 0.5)")
    parser.add_argument("--check", help="the child's check command line, run on the merge (default: its parent's)")
    parser.add_argument("--timeout", type=float, default=1800.0, help="seconds to wait for the child")
    args = parser.parse_args(argv)

    if not (os.environ.get("BRANCHYARD_DELEGATION") or os.environ.get("BRANCHYARD_REMOTE")):
        print(NOT_A_BRANCH, file=sys.stderr)
        return 2

    try:
        by = connect()  # inside a turn, over BRANCHYARD_REMOTE, or with your authority: the same calls
        kid = by.spawn(args.prompt, harness=args.harness, name=args.name, budget_usd=args.budget_usd, check=args.check)
        done = by.wait(kid.name, timeout=args.timeout)  # an Inspection; RunningError past the timeout
    except RunningError:
        print(f"delegate: the child did not settle in {args.timeout:g}s; it keeps running", file=sys.stderr)
        return 1
    except BranchyardError as error:
        print(f"delegate: {error}", file=sys.stderr)
        return 1

    print(f"{kid.name}: {done.state}")
    if done.state != "ready":
        print(f"delegate: not integrating a child that is {done.state}; `by log {kid.name}` says why", file=sys.stderr)
        return 1
    try:
        merged = by.integrate(kid.name)  # a Merged; integrate(a, b) is one check for both
    except CheckFailedError as error:  # the same class and detail inside and outside a harness
        by.send(kid.name, f"The check failed on the merge: {error.message}")
        print(f"delegate: {error}; the child has been sent a turn to fix it", file=sys.stderr)
        return 1
    except BranchyardError as error:
        print(f"delegate: {error}", file=sys.stderr)
        return 1
    print(f"merged {kid.name} into {merged.target} at {str(merged.commit)[:10]}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
