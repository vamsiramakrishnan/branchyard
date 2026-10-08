---
name: delegate
description: Delegate parts of a coding task to child branches with Branchyard. Use when you run on a Branchyard branch (BRANCHYARD_BRANCH is set) and the work splits into independent pieces, needs a second harness, or should be tried more than one way. Covers `by spawn` (including children that wait for siblings), `by graph`, `by inspect`, `by wait`, `by integrate` (several children at once), `by discard`, ending your turn while children run, artifacts, messaging your parent, the Python module and the MCP tools.
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
   Pick a model too when the work is mechanical: a rename across files, a
   format or lint fix, a mechanical port with a test to check it, or
   gathering facts for you to judge. `by spawn --model small` (or a model
   name) gives such a child a cheaper, faster model, so the same budget
   goes further; keep your own model, the default, for design, debugging
   and anything you would have to redo. A harness that cannot choose a model
   refuses the spawn and says why; `by inspect <child>` shows its model.
3. Wait. Either way works:
   - **End your turn.** You do not have to stay awake: when every child
     you delegated has settled, Branchyard starts your next turn by itself
     with a summary of each child (status, diffstat, cost, last message,
     why it failed) and what you can do next. Do not leave a background
     task or monitor polling for them; it is stopped when your turn ends.
   - **Wait in this turn.** `by wait` blocks until your running children
     have settled (`by wait a b`, `--any` for the first, `--timeout S`);
     it is woken by Branchyard, not a poll. `by spawn ... --wait` waits
     for one child.
   `by inspect <child>` shows status, diffstat, cost and its last message;
   `by events <child>` shows its activity. To correct a child that is still
   running, `by send <child> --steer "<text>"` adds to its running turn
   without stopping it. Its answer starts with `accepted:` once the running
   turn took it, and says when the child reads it (for Claude Code, before
   its next model call; never in a later turn), or with `written:` if the
   harness has not confirmed it yet. Some harnesses cannot
   take it, and the refusal says so.
   A child whose turn was cut off because the engine running it stopped
   says so in its status; `by send <child> --retry` submits that turn's
   prompt again.
4. Integrate. When children are `ready`, `by integrate <child>` merges one
   into your branch after its check passes; `by integrate a b c` merges
   several together, in order, runs the check once on the result and moves
   your branch once, all or none. Children that share your check integrate
   together: a child inherits your check unless you give it one (the spawn
   and `by inspect <child>` say which, and which siblings share it), and a
   whole-suite check passes only with all of them, so integrate them
   together rather than merging them into each other. Give a child its own
   check with `by spawn --check` when it should land alone. If you integrate one that shares
   its check and the check fails, the error names the siblings and the
   command (`by integrate a b c`). Never `git merge` children yourself: that
   bypasses the check, and Branchyard records them as merged only after the
   fact. A child your branch already contains is recorded as merged, not
   refused. A conflict names the child and the files, and a failed check
   shows its output: send the child a fix with `by send <child>
   "<prompt>"`, or do it yourself.
5. Clean up. `by cancel <child>` stops a running child and everything
   below it. A child that already finished is not running, so cancel
   changes nothing; set a finished child you will not use aside with
   `by discard <child> --reason "<why>"`. It keeps its record and cost,
   is never integrated or sent more work, and so never holds a slot or
   your budget again, and never keeps you waiting. It keeps its name, its
   worktree and its git branch until `by rm <child>` removes them; what it
   spent still counts in your budget.

Statuses: `running`, `ready` (a candidate to merge), `no_changes`,
`interrupted`, `budget_exceeded`, `failed`, `merged`, `discarded`,
`waiting` and `blocked` (prerequisites), `waiting_on_children` (its turn
ended while its children run; it is woken when they settle).

Inside a harness, `by spawn` refuses `--parent`, `--yes`, `--ask` and
`--permissions`: your children run under your policy. `by spawn --help`
marks every flag you may not pass.

Budget: `--budget-usd` sets a child's cost limit, which refusals and statuses
call `max_usd`. A child that is running, waiting, blocked, awaiting plan
approval or waiting on its own children holds its whole budget of yours. Once it settles (any other status,
`discarded` included) it holds only what it spent, and the rest is yours again;
a removed child's spend still counts. `by inspect` shows what live children
hold (`reserved_usd`, by `reserving_children`) and what settled ones spent (`settled_children_usd`).
Sending a settled child more work holds its budget again, narrowed to what you
have left. Your envelope's `max_children` counts only live children.

## Commits and scratch files

Branchyard commits everything in your worktree at the end of each of your
turns, and in each child's, as commits named `by/<branch>: turn <n>`: you need
not commit, and those commits are Branchyard's. Ending your turn to wait for
your children commits your work the same way. Write scratch files under
`$TMPDIR`, a directory private to your branch, not in `/tmp`, which other
branches share.

## With `by`

```sh
by inspect                      # your own branch: budget left, children
by spawn "Make tests/parser_test.rs deterministic; run cargo test -p parser" \
  --name parser-flake --harness codex --budget-usd 0.50 --max-turns 3
by spawn "Document the retry policy in docs/retries.md" --name retry-docs --budget-usd 0.20
by spawn "Rename parse_opts to parse_options everywhere; run cargo test" \
  --name rename --model small --budget-usd 0.10   # mechanical: a cheaper model
by spawn --prompt-file "$TMPDIR/port-task.md" --name port --budget-usd 0.50   # a long task
by children
by inspect parser-flake
by events parser-flake --cursor 0
by spawn "Add a regression test for issue 42" --name issue-42 --budget-usd 0.30 --wait
by send parser-flake --steer "Use the fixture in tests/data, not a new one."
by wait parser-flake retry-docs --timeout 600
by send parser-flake "The seed must come from the test name, not the clock."
by integrate parser-flake retry-docs
by cancel retry-docs
by discard retry-docs --reason "superseded by parser-flake"
```

`by inspect` does not wait; wait with `by wait`, or end your turn.

Every command takes `--json` for a stable machine-readable result, and
exits non-zero with `{"error": {"kind", "message"}}` on refusal. Run `by`
by name or as `$BRANCHYARD_BY`; call it as a single command without pipes,
`&&` or substitutions, so a permission policy can recognize it.

## From a GitHub issue

If the work is a GitHub issue and `gh` works in your shell, `by spawn --issue 42`
(or an issue URL) makes the issue the child's prompt, under a header naming it,
and names the child `issue-42-<slug>`; a prompt you add is appended as extra
instructions. Pull requests are not yours to open: `by pr` pushes with the
user's credentials and refuses to run inside a harness. Integrate the child;
whoever started your branch opens the pull request.

## When one child needs another's work

Give it `--depends-on`: it is created waiting and starts by itself once the
other has finished (`--after integrated`: once you integrated it, so it
starts from that work). If the other fails, it is `blocked` and never runs.

```sh
by spawn "Add the schema migration" --name schema
by spawn "Use the new column in the API" --name api --depends-on schema --after integrated
by graph show           # your children, what each waits for, and the revision
```

`by graph apply --edits '[...]' --expected-revision N` creates several
children and dependencies at once, all or nothing; if it says
`stale_revision`, run `by graph show` and propose again. Each edit is an
object tagged by `kind`:

```sh
by graph apply --expected-revision 3 --edits '[
  {"kind": "spawn", "name": "schema", "prompt": "Add the migration"},
  {"kind": "spawn", "name": "api", "prompt": "Use the column", "depends_on": ["schema"]},
  {"kind": "add_dependency", "dependent": "docs", "prerequisite": "api"}]'
```

`by graph apply --help` lists every field.

## In a rig

If your instructions say you fill a seat in a rig, or `by inspect` shows a
`seat`, you spawn only by seat, and only the seats listed in its `seats`:

```sh
by spawn --seat implementer "Port the tokenizer to the new API; run its tests"
by spawn --seat reviewer "Review the tokenizer candidate on by/parser-implementer" --wait
```

The seat sets the child's harness, budget, check and instructions; you
may pass a smaller budget, never a larger one. In Python, pass `seat=`;
the MCP `spawn` tool takes `seat`.

## Sharing results and asking your parent

Every child, even one that cannot delegate further, can publish files and
message you, and you can do the same with your own parent:

```sh
by artifact publish results.json --name results --media-type application/json
by artifact list
by artifact get 3 --out data/results.json
by ask "Should the parser accept tabs?" --wait 300
by report "Tokenizer done; formatter next"
by inbox --unread
by answer 12 "Yes, accept tabs"
```

Artifacts you publish are readable by your ancestors and descendants; a
sibling needs `by artifact share ID --to BRANCH`. An artifact's `digest`
is the blake3 hash of its bytes.

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
done = branchyard.wait_all(*(child.name for child in parts), timeout=1800)
ready = [i.name for i in done.settled if i.status.state == "ready"]
try:
    branchyard.integrate(*ready)        # together, checked once
except branchyard.BranchyardError as error:
    print(error.kind, error.message)
```

`branchyard.wait_any(...)` returns when the first settles. Errors are
`DeniedError` (envelope, budget or authority), `RunningError` (also a wait
that timed out), `NotFoundError`, or `BranchyardError` with a `kind`.

## Without a shell

If you cannot run commands, the same operations are MCP tools on the
`branchyard` server: `spawn` (with `depends_on`), `inspect`, `events`, `send`,
`steer`, `propose_integration` (`branch`, or `branches` to integrate several
together), `wait` (`branches`, `any`, `timeout_seconds`), `cancel`, `discard`,
`children`, `graph`, `apply_graph`, the artifact and scratch tools, and `ask`,
`report`, `escalate`, `answer` and `inbox`.
