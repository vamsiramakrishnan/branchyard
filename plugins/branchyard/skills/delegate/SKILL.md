---
name: delegate
description: Use whenever you run on a Branchyard branch (BRANCHYARD_BRANCH is set), before you start and before you finish. Covers when to split work into child branches (`by delegate` with a plan, or `by spawn`), how to write a child's brief and prompt, ending your turn while children run, integrating and checking results, and the `by check` you run before you end.
---

# Delegating with Branchyard

You are running on a Branchyard branch: your own git branch and worktree.
You may create child branches, each with its own harness, budget and
worktree, and have their work brought into your branch. You act only as
your own branch and only on your descendants. Branchyard enforces your
envelope (depth, number of children, allowed harnesses) and your budget; a
refusal says why.

The rules below apply to every task on a branch, split or not: run `by check`
before you finish, and end your turn when you wait on children.

## When to delegate

- The task splits into parts that touch different files or modules and can
  proceed in parallel.
- A part suits another harness better, and your envelope allows it.
- The work matters enough that someone else should review it before it
  lands: a reviewer on another vendor's harness catches what its author
  cannot.
- You want two approaches to the same problem and will keep one.

Do small or tightly coupled work yourself. Each child costs a harness
session, and work split along lines that cross the same files conflicts.

## The short way: delegate, end your turn, be resumed

You do not have to orchestrate. Say the whole split once, let the engine
run it, and end your turn; your next turn starts with the result.

1. **Split by ownership.** Give each part the paths it owns (`owns`), so no
   two children touch the same file: siblings that own disjoint paths cannot
   conflict, and a change a child makes outside its paths is refused when it
   lands. Files every part needs, such as a lockfile, belong to no child:
   regenerate them yourself afterwards.
2. **Settle the interfaces first, in a brief.** The brief is what every
   child shares and no prompt should repeat: the goal, the conventions, and
   the contracts between the parts (a type, a function signature, a file
   format). Children are given it each turn. A decision you make later, such
   as an interface settled after a child's question, is one more entry:
   `by brief "The lexer returns Spanned<Token>."`; children running now are
   sent it at once.
3. **Delegate the plan, left to land.** One call creates every child, all
   or none:

   ```sh
   by delegate --plan "$TMPDIR/plan.json" --land
   ```

   ```json
   {"brief": "Tokens are the Token type in src/token.rs. Add no crates.",
    "review": {"harness": "codex", "budget_usd": 0.50},
    "tasks": [
      {"name": "lexer", "prompt": "Write the lexer in src/lexer; run cargo test -p lexer",
       "owns": ["src/lexer"]},
      {"name": "parser", "prompt": "Write the parser in src/parser; run cargo test -p parser",
       "owns": ["src/parser"], "depends_on": ["lexer"], "after": "integrated"}]}
   ```

   With `--land` (or `"land": true` in the plan), the engine integrates each
   child into your branch when it is ready, as `by integrate` would, its
   check on the exact merge included. A child that conflicts with your
   branch is sent a turn to merge your branch and resolve it; one whose
   check fails on the merge is sent the failure to fix; one that changed
   paths it does not own is told to undo them. At most two such turns; then
   it is left for you, with why. With `review` (a harness you may delegate
   to, ideally another vendor's than the children's), a reviewer reads each
   child's work before it lands; its findings go back to the child as a turn
   to fix them. Give the review a `budget_usd` when you have a cost limit:
   each review is a child of yours and spends from your budget. A task may
   name `"harness": "auto"`: the router places it on the harness you may run
   whose recorded outcomes for that kind of task are best, preferring one
   with a free slot, and records why. A child whose harness is full waits
   for a free slot and starts by itself (`waiting_for` says so).
4. **End your turn.** Do not wait, poll or integrate. When everything has
   settled, Branchyard starts your next turn with a summary: what landed in
   your branch (it is in your worktree and your candidate already), what
   was left for you and why, and each child's status, cost and last message.
5. **Finish.** Read what landed, run `by check` on the whole, fix what is
   left (`by send <child> "<fix>"`, or do it yourself; integrate a child
   left for you with `by integrate <child>` once it is fixed), and end your
   turn.

`by delegate` is also the `delegate` tool's arguments (`{"tasks", "brief",
"land", "review"}`), `branchyard.delegate(tasks, brief=..., land=True,
review="codex")` in Python, and `Delegate::delegate` in Rust. A single child
works the same way: `by spawn "..." --owns src/x --land --review codex`.
`by inspect <child>` shows its `landing` (where it landed, or why not, and
the turns sent to fix it) and its `review`.

The rest of this skill is the long way: driving each child yourself.

## Writing a child's prompt

A child sees your committed work, the shared brief and its prompt, not this
conversation. Write the prompt so that someone who knows nothing else could
do the task and show it is done. Say, in this order:

1. The goal, in one sentence.
2. The files it owns, and the ones it may read but not change.
3. The constraints: interfaces already settled (those belong in the brief),
   crates not to add, the style to match.
4. How it checks its work: the exact command, and what passing means.
5. What to report: what changed, what it could not do, what you must do next.

Good:

```sh
by spawn "Write the lexer for the calculator. You own src/lexer/ and may read src/token.rs, which you must not change. Match the error style in src/error.rs. Check with cargo test -p lexer; it must pass. Then report the token kinds you handle and anything you left out." \
  --name lexer --owns src/lexer --budget-usd 0.50
```

Bad: `"Do the lexer."` or `"Implement the plan we discussed."` The first names
no files, no interface and no check, so the child guesses all three; the second
points at a conversation the child never saw. Either one costs you a turn at
integration.

## The loop, by hand

1. Decompose. Give each child a self-contained prompt (see "Writing a
   child's prompt"): the goal, the files involved, the constraints, and how
   to check the result.
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
     task or monitor polling for them. A turn with background work still
     running is held until it finishes (within your turn's limits, and
     at most 30 minutes without a duration limit), so a poll that never
     ends keeps your turn open until that bound stops it. A bound that
     cuts the hold does not cut your answer: the turn still ends
     completed, with a warning naming the background tasks still running
     (those are stopped), so you are parked and woken as usual. Never run `by wait` or an
     orchestration script as a backgrounded shell command and end your
     turn to wait for it: a backgrounded shell command belongs to your own
     session, and once your turn's limits end the hold it dies with the
     session, unlike your children. Either wait with `by wait` in the
     foreground, or just end your turn; Branchyard itself wakes you.
   - **Wait in this turn.** `by wait` blocks until your running children
     have settled (`by wait a b`, `--any` for the first, `--timeout S`);
     it is woken by Branchyard, not a poll. It prints each child as it
     settles (with `--json`, a JSON line on stderr; stdout keeps the one
     result). Keep `--timeout S` under any limit of your own (a shell
     `timeout`, your tool call's time limit): a wait stopped from outside
     prints no result, only the children that settled before. `by spawn
     ... --wait` waits for one child.
   `by inspect <child>` shows status, diffstat, cost and its last message;
   `by events <child>` shows its activity. To correct a child that is still
   running, `by steer <child> "<text>"` (the same as
   `by send <child> --steer "<text>"`) adds to its running turn
   without stopping it. For long text, or text with backticks or quotes a
   shell would mangle, write it to a file and pass `--prompt-file <path>`
   (`-` for standard input) to `by send` or `by steer` instead. Its answer starts with `accepted:` once the running
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
   refused. A failed check shows its output and names every check that
   ran, the children it belongs to and whether it passed, so you know whose
   to fix: send the child a fix with `by send <child> "<prompt>"`, or do it
   yourself. `by check <child>` runs a child's check on its current work,
   as integrating it would, without integrating it.
   - **A conflict between siblings** names the child, the siblings merged
     before it and the files (`b conflicts with by/me plus a in calc.py;
     nothing was integrated`), and nothing moves. Resolve it in the
     conflicting child's branch, then integrate again. Its worktree shares
     the repository's branches, so it can merge the sibling's branch
     (`by/<sibling>`):

     ```sh
     by send b "Run git merge by/a in your worktree, resolve the conflicts in calc.py keeping both changes, commit the merge, run by check, then end your turn."
     by wait b
     by integrate a b
     ```

     b's candidate now contains a, so its merge onto a is clean, and
     siblings that share your check still integrate together. A child
     with a check of its own can land alone instead: `by integrate a`,
     then have b merge your branch (`git merge by/<you>`) and `by
     integrate b`. `by check b` says whether it still conflicts. Do not
     merge them yourself in your own worktree.
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
`waiting` and `blocked` (prerequisites), `awaiting_plan_approval` (a child
spawned with `--plan` wrote its plan and waits for you), `waiting_on_children`
(its turn ended while its children run; it is woken when they settle).

A child spawned with `by spawn --plan` plans read-only first and escalates
its plan to your inbox. Read it with `by plan show <child>`. `by plan approve
<child>` runs it as the child's next turn (`--edit` or `--file PATH` changes
the plan first); `by plan reject <child> --reason "<why>"` ends the child, or
with `--replan` has it plan again. When a descendant's tool or connector call
needs approval, the ask comes to you too: `by approvals ls` lists what waits,
`by approvals allow <id>` or `by approvals deny <id> --reason "<why>"` answers
it. Only an ancestor may decide either.

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

## Choosing a harness

Pick with data, not by name. Before each spawn, ask:

    by fleet advise "<the subtask, as you will give it to by spawn>" --json

It answers without running anything: the subtask's `kind`; the
`candidates` (your task's `[fleet]` entry for that kind, or the harnesses
your envelope allows), each with `available` (and `reason` when not), its
recorded outcomes for this kind (`stats.runs`, `stats.mean_cost_usd`) and
`expected`, the P(OK) the router samples around (0.50 with no outcomes);
the router's own `pick`; your `remaining_usd`; and `notes`. Python:
`branchyard.advise(prompt)`; MCP: `advise`.

Read it like this:

- Prefer the cheapest candidate whose P(OK) for this kind clears what the
  subtask can afford to get wrong: about 0.7 for mechanical work with a
  check, higher for design or debugging you would have to redo.
- With no recorded outcomes every P(OK) is 0.50: take the cheapest that can
  do the work, give it a check, and let the outcome teach the store.
- `notes` names risk words (production, delete, migration, security,
  payment, ...): never give such a subtask to the cheapest tier.
- Escalate on evidence: a child that failed its check or ran out of budget
  is retried on the next tier up, not the same one.
- The router's `pick` is one opinion from the same data; you know the
  subtask and the repository, it does not.

Then say why, so the choice is recorded beside how the child turns out:

    by spawn "Rename Compat::Versioned to Compat::PathVersioned everywhere" \
      --harness codex --model small --check "cargo test" --budget-usd 0.20 \
      --why "mechanical rename, cargo test checks it; codex small is 6/6 on refactors here"

`by log <child>` shows the choice (`chosen by ... : codex model=small: ...`),
and `by fleet stats` counts children's outcomes beside routed runs (its
PARENT column), so the next advice knows.

Split or solo? Do it yourself when the work is small or tightly coupled.
Split when the parts are separable with little shared state (you
supervise, children work), when one change repeats across many targets
(fan out, then integrate together), when correctness matters more than
speed (one child does, another reviews), or when parts have an order (a
graph with `--depends-on`). A split costs a session per child and a merge;
it pays when the children run in parallel or on a cheaper tier.

## Commits and scratch files

Branchyard commits everything in your worktree at the end of each of your
turns, and in each child's, as commits named `by/<branch>: turn <n>`: you need
not commit, and those commits are Branchyard's. Ending your turn to wait for
your children commits your work the same way. Write scratch files under
`$TMPDIR`, a directory private to your branch, not in `/tmp`, which other
branches share.

## Before you finish

Run `by check` before you end your turn. It runs your branch's check (your
own, or the one you inherited) on your work as it is, uncommitted files
included, merged into your parent's branch, the way your parent's
integration will; nothing is committed or integrated. What fails here
fails your integration: a repository rule you never saw is in its output.
Fix it and run `by check` again until it passes. It exits 1 when the check
fails, times out or your work conflicts with your parent's branch, and
says so when you have no check to run. For a conflict, merge your
parent's branch into your work (`git merge by/<parent>`), resolve the
files it names, commit the merge, and run `by check` again.

## With `by`

```sh
by inspect                      # your own branch: budget left, children
by check                        # your work, checked as your integration will be
by spawn "Make tests/parser_test.rs deterministic; run cargo test -p parser" \
  --name parser-flake --harness codex --budget-usd 0.50 --max-turns 3
by spawn "Document the retry policy in docs/retries.md" --name retry-docs --budget-usd 0.20
by spawn "Rename parse_opts to parse_options everywhere; run cargo test" \
  --name rename --model small --budget-usd 0.10 \
  --why "mechanical rename with a test; the cheapest tier does these"   # recorded on the child
by spawn --prompt-file "$TMPDIR/port-task.md" --name port --budget-usd 0.50   # a long task
by children
by inspect parser-flake
by events parser-flake --cursor 0
by spawn "Add a regression test for issue 42" --name issue-42 --budget-usd 0.30 --wait
# Ask for the answer as JSON, in a shape (see "Asking for a result" below)
by spawn "List the files that call parse_opts" --name callers \
  --result-schema '{"type":"object","required":["files"],"properties":{"files":{"type":"array","items":{"type":"string"}}}}' \
  --result-required
by steer parser-flake "Use the fixture in tests/data, not a new one."
by wait parser-flake retry-docs --timeout 600
by send parser-flake "The seed must come from the test name, not the clock."
by send parser-flake --prompt-file "$TMPDIR/review.md"   # a long or quoted follow-up
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

## Asking for a result

Read a child's answer as JSON instead of prose by saying its shape at spawn:
`--result-schema '<JSON Schema>'` (or `--result-schema-file PATH`), with
`--result-required` when it must answer. The schema may use only `type`
(`object`, `array`, `string`, `number`, `integer`, `boolean`, `null`, or a
list of them), `properties`, `required`, `additionalProperties`, `items`,
`enum`, `const`, `minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum`,
`minLength`, `maxLength`, `pattern`, `minItems`, `maxItems`, `uniqueItems`,
`anyOf` and `oneOf`, and annotations such as `title` and `description`; any
other keyword, such as `$ref`, `allOf` or `format`, is refused by name. The
child reports with `by report "<text>" --result '<JSON>'`, which is refused,
naming the path (`result.files[2]: expected string, got number`), when the
JSON does not match. You read it as `result` in `by inspect <child> --json` and in each
settled child of `by wait --json`; a child that ends its turn without a
required result leaves a warning in your log. In Python,
`branchyard.spawn(..., result={"schema": {...}, "required": True})` and
`branchyard.inspect(child).result`; the MCP `spawn` tool takes `result`.
The result is the child's claim: `by check` and `by integrate` still decide
what merges.

## Sharing results and asking your parent

Every child, even one that cannot delegate further, can publish files and
message you, and you can do the same with your own parent:

```sh
by artifact publish results.json --name results --media-type application/json
by artifact list
by artifact get 3 --out data/results.json
by ask "Should the parser accept tabs?" --wait 300
by report "Tokenizer done; formatter next"
by report "Found two files" --result '{"files": ["a.rs", "b.rs"]}'   # when your parent set a result_contract
by report-status working "Porting the tokenizer's tests"
by inbox --unread
by answer 12 "Yes, accept tabs"
```

`by report-status <state> "<one line>" [--link URL]` states your own status
beside the one Branchyard derives: `working`, `idle`, `blocked`, `complete`
or `failure`, in at most 280 characters. Your parent sees the latest in
`by inspect` and the person watching in `by watch`; `blocked` and `failure`
are also escalated to your parent's inbox. It is a claim, not a message
and not a result: `by report` is how you tell your parent something.

Artifacts you publish are readable by your ancestors and descendants; a
sibling needs `by artifact share ID --to BRANCH`. Whoever can already
read the artifact may run that share, not only its publisher: the
publishing sibling itself, or a common ancestor of both. The receiving
sibling cannot, since it cannot read the artifact yet. `BRANCH`
must already exist: spawn the consumer first, or have it `by artifact
get` a copy a common ancestor republished to it. An artifact's `digest`
is the blake3 hash of its bytes.

## With Python

The `branchyard` module is on `PYTHONPATH`; it runs `by --json` for you.

```python
import branchyard

me = branchyard.inspect()
# remaining_usd is None on a branch with no cost limit: then give no budget.
share = None if me.remaining_usd is None else me.remaining_usd / 2
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

Or hand the engine the whole split and end your turn; your next turn says
what landed:

```python
branchyard.delegate(
    [
        {"name": "tokenizer", "prompt": "Port the tokenizer; run its tests", "owns": ["src/tokenizer"]},
        {"name": "formatter", "prompt": "Port the formatter; run its tests", "owns": ["src/formatter"]},
    ],
    brief="The new API is in src/api.rs; keep the public names.",
    land=True,
    review={"harness": "codex", "budget_usd": 0.50},
)
```

`branchyard.wait_any(...)` returns when the first settles. Errors are
`DeniedError` (envelope, budget or authority), `RunningError` (also a wait
that timed out), `NotFoundError`, or `BranchyardError` with a `kind`.

## Without a shell

If you cannot run commands, the same operations are MCP tools on the
`branchyard` server: `delegate` (`tasks`, `brief`, `land`, `review`), `brief`,
`spawn` (with `depends_on`, `owns`, `land`, `review` and `result`), `inspect`, `events`, `send`,
`steer`, `propose_integration` (`branch`, or `branches` to integrate several
together), `check` (your own work before you finish, or a child's), `wait` (`branches`, `any`, `timeout_seconds`), `cancel`, `discard`,
`children`, `graph`, `apply_graph`, `approve_plan` and `reject_plan` (a
plan-first child's plan), `answer_approval`, the artifact and scratch tools, and
`ask`, `report` (with `result`), `escalate`, `report_status`, `answer` and
`inbox`.
