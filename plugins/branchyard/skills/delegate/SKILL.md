---
name: delegate
description: Delegate parts of a coding task to child branches with Branchyard. Use when you run on a Branchyard branch (BRANCHYARD_BRANCH is set) and the work splits into independent pieces, needs a second harness, or should be tried more than one way. Covers `by spawn`, `by inspect`, `by integrate` and the Python module.
---

# Delegating with Branchyard

You are running on a Branchyard branch: your own git branch and worktree.
You may create child branches, each with its own harness, budget and
worktree, and merge their results into your branch. You act only as your
own branch and only on your descendants. Branchyard enforces your envelope
(depth, number of children, allowed harnesses) and your budget; a refusal
says why.

## When to delegate

- The task splits into parts that touch different files or modules and can
  proceed in parallel.
- A part suits another harness better, and your envelope allows it.
- You want two approaches to the same problem and will keep one.

Do small or tightly coupled work yourself. Each child costs a harness
session, and merging overlapping edits produces conflicts.

## The loop

1. Decompose. Give each child a self-contained prompt: the goal, the files
   involved, the constraints, and how to check the result. A child sees
   your committed work, not this conversation.
2. Spawn. Pick a harness per subtask and a budget that fits in what you
   have left (`by inspect` shows `remaining_usd`). A child starts from your
   current work: your uncommitted changes are committed to your branch
   first.
3. Watch. `by inspect <child>` shows status, diffstat, cost and its last
   message; `by events <child>` shows its activity. Wait with `--wait` or
   by polling; do not busy-loop faster than every few seconds.
4. Integrate. When a child is `ready`, `by integrate <child>` merges it
   into your branch after its check passes. Your working tree moves to the
   merge. A conflict or failed check is an error: send the child a fix with
   `by send <child> "<prompt>"`, or do it yourself.
5. Clean up. `by cancel <child>` stops a child and everything below it.

Statuses: `running`, `ready` (a candidate to merge), `no_changes`,
`interrupted`, `budget_exceeded`, `failed`, `merged`.

## With `by`

```sh
by inspect                      # your own branch: budget left, children
by spawn "Make tests/parser_test.rs deterministic; run cargo test -p parser" \
  --name parser-flake --harness codex --budget-usd 0.50 --max-turns 3
by spawn "Document the retry policy in docs/retries.md" --name retry-docs --budget-usd 0.20
by children
by inspect parser-flake
by events parser-flake --cursor 0
by spawn "Add a regression test for issue 42" --name issue-42 --budget-usd 0.30 --wait
by send parser-flake "The seed must come from the test name, not the clock."
by integrate parser-flake
by cancel retry-docs
```

Every command takes `--json` for a stable machine-readable result, and
exits non-zero with `{"error": {"kind", "message"}}` on refusal. Run `by`
by name or as `$BRANCHYARD_BY`; call it as a single command without pipes,
`&&` or substitutions, so a permission policy can recognize it.

## With Python

The `branchyard` module is on `PYTHONPATH`; it runs `by --json` for you.

```python
import branchyard

me = branchyard.inspect()
share = (me.remaining_usd or 0) / 2
parts = [
    branchyard.spawn("Port the tokenizer to the new API; run its tests",
                     name="tokenizer", budget_usd=share),
    branchyard.spawn("Port the formatter to the new API; run its tests",
                     name="formatter", budget_usd=share),
]
for child in parts:
    done = branchyard.wait(child.name, timeout=1800)
    if done.status["state"] == "ready":
        try:
            branchyard.integrate(child.name)
        except branchyard.BranchyardError as error:
            print(child.name, error.kind, error.message)
```

Errors are `DeniedError` (envelope, budget or authority), `RunningError`,
`NotFoundError`, or `BranchyardError` with a `kind`.

## Without a shell

If you cannot run commands, the same operations are MCP tools on the
`branchyard` server: `spawn`, `inspect`, `events`, `send`,
`propose_integration`, `cancel` and `children`.
