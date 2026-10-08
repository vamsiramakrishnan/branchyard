# Delegation

A harness running on a Branchyard branch can act as a meta-harness: it creates child branches, gives each a harness, a prompt and a budget, watches them, merges the ones it wants into its own branch, and cancels the rest. Children can delegate further when their envelope allows. This works in local mode and through a server whose operator allows it, tested against a fake ACP agent; no real harness has delegated through it yet.

There is one set of operations and one authority model. Four surfaces reach them:

| Surface | Use it when |
|---|---|
| `by spawn`, `by inspect`, `by events`, `by send` (and `by send --steer`), `by wait`, `by integrate`, `by cancel`, `by discard`, `by children`, `by graph` | The harness can run shell commands. This is the primary path: every coding harness has a shell, and `--json` output composes in scripts. |
| The Python module `branchyard` | The harness writes Python to orchestrate: loops, fan-out, waiting. It runs `by --json` for you. |
| The Rust SDK, `branchyard::Delegate` | You write the meta-harness in Rust, in or out of a harness. |
| Branchyard's MCP server (`by mcp`) | The harness cannot run commands, or its shell cannot reach the repository, but it can call MCP tools. |

Each surface calls the same operations in the engine that runs the harness's turn, so they give the same answers and refusals. One table, [`branchyard::operations`](../crates/branchyard/src/operations.rs), names every operation on every surface: its `by` subcommand and flags, its MCP tool and arguments, its Python function and keyword arguments, its `Delegate` method, where it may run (inside a harness, outside one, through a server), which flags a harness may not pass and why, and the capability a harness's token needs for it. Parity tests fail when a surface lacks an operation or an argument the table lists, or has one it does not (`branchyard`: the Python module, `Delegate` and the engine's dispatch; `branchyard-mcp`: the tools and their schemas; `branchyard-cli`: each subcommand's flags), and when this page, [storage](storage.md) or the delegation skill shows a `by` command or flag that does not exist. `by <command> --help` takes its notes on what a harness may not pass from the same table, and so does `--allow-delegation`'s list of commands. A new operation, such as a command that waits for children, is a row there; the tests then name each surface still missing it. Children may depend on one another, and a branch can change its graph of children in one atomic proposal; see [task graphs](graph.md). The same four surfaces also reach [artifacts and scratch areas](storage.md) (`by artifact`, `by scratch`, `branchyard.publish`/`create_scratch`, `Delegate::publish_artifact`, the MCP `publish_artifact`/`create_scratch` tools and their siblings): shared storage a branch's descendants and ancestors can read without a merge, tested the same way.

## Turning it on

```sh
by run "Split the parser rewrite into tokenizer and formatter work, delegate both, and integrate them" \
  --delegate --budget-usd 3 --max-turns 5 --yes
```

`--delegate` lets the harness create children one level deep; `--delegate=2` allows grandchildren. In the SDK, set `TaskOptions::delegation` to an `Envelope`. The envelope is stored with the branch, so later sends keep it.

`by run` waits until every branch it delegated has finished, whichever process runs it, says which are still running while it waits, and prints a table of them. When the harness ended its turn while its children ran, `by run` also waits for the turn that wakes it ([below](#waiting-on-children)) and summarizes the branch as that turn left it. A delegated branch whose engine stopped is recovered rather than waited for: `ready` with a warning when its lost turn had written work, else `interrupted` ([durability](durability.md#recovery)). `by ls` shows the tree.

## Waiting on children

A harness may end its turn while children it delegated still run. Claude Code does this on its own: it moves a long wait into a background task and ends its turn ("I'll get a notification when they finish"). Branchyard does not take the end of that turn for the end of the branch's work:

- When a turn of a branch that may delegate ends `ready` or `no_changes` while one of its descendants is `running`, `waiting` for prerequisites, or itself waiting on its children, the branch is recorded `waiting_on_children`, with what its turn ended as and its limits, instead of finished. Its log says which children it waits on (`delegated: wait <branch>: ...`).
- When all of them have settled, its next turn starts by itself, resuming its harness session, with a prompt between `<branchyard-wake>` tags: for each child it waited on, its status, candidate (`1 file +12 -3 at 1a2b3c4d5e`), cost, its last message (the last 600 characters) and why it failed or is blocked; one line for its other children not yet merged; and what it can do next (`by integrate a b`, `by send`, `by inspect`, `by wait`). If that turn ends while children run again, it is parked and woken again.
- It waits for **all** of them, not the first, so one wake carries every result and a parent of six children is not woken six times. A parent that wants to act on each child as it finishes waits inside its turn: `by wait --any`. A `blocked` child does not keep its parent parked, since only the parent can unblock it; the wake lists it with its reason. Nor does a settled one (`discarded` included), which never runs again on its own.

**Bounds.** At most `max_wakes` automatic wakes in a row (`Envelope::max_wakes`, 8 by default; a child's is at most its parent's); a turn something else starts (a `send`, a plan approval) resets the count. The wake is a turn like any other, under the parent's own cost, turn and duration limits, stored with it when it parked; a parent whose cost or turn limit is spent, or that reached `max_wakes`, is not woken and ends as its parked turn did, with a warning saying why.

**Opting out.** `by run --no-wake`, `Envelope::no_wake()` (`max_wakes` 0) in `TaskOptions::delegation`: the turn ends as it is, and the caller waits for the children (`by wait`, `Delegate::wait_for`).

**Whichever process.** The parked state is in the store, and whichever engine settles the last child looks at its parked ancestors, in any process: starting the wake is a compare-and-swap from `waiting_on_children` to `running` with the first lease, so exactly one engine wakes it. The wake runs under the options of the parked turn when the process that ran it starts it (so `by run`'s policy and console), else under those of the turn that settled the last child, which are the parent's policy and tools. A wait for the subtree (`by run`, `Branch::wait_subtree`, every server operation that waits for one) wakes what it finds parked and waits for that turn too. After a crash, `Yard::resume_graph` (`by graph resume --yes`, and every server's and `by worker`'s recovery tick) wakes a parked branch whose children have all settled, under the options it is given; `by send <branch>` continues it by hand at any time. `by cancel` on a parked branch ends it `interrupted` and stops its children.

## What a delegated harness gets

Every turn of a branch that was given delegation gets these, for the duration of that turn: a root started with `--delegate`, and every child it delegated to at any depth, including a leaf whose envelope allows no children of its own (`max_depth` 0).

| What | Where |
|---|---|
| A token for this turn | `BRANCHYARD_DELEGATION`, and `.branchyard/delegation/<branch>.json` (mode 0600) with the broker's socket |
| Its branch and repository | `BRANCHYARD_BRANCH`, `BRANCHYARD_ROOT` (the harness's working directory is its worktree, not the repository root) |
| `by` | `BRANCHYARD_BY`, and `by`'s directory first on `PATH` |
| The Python module | `.branchyard/sdk/python/branchyard.py`, first on `PYTHONPATH` |
| The MCP server | `by mcp --root <root> --branch <name>`, projected per harness (below) |
| Instructions | The delegation skill for a branch that may spawn; for a leaf, a short note ([`plugins/branchyard/leaf.md`](../plugins/branchyard/leaf.md)) on what it may do. Projected per harness (below) |

What the token may do is the branch's capabilities, not its depth: every token acts as its own branch only, on itself (`inspect`, `events`, `graph`, `children`, and `wait`, which a leaf, with no children, never blocks in), on the artifacts and scratch areas it may reach under [storage](storage.md)'s grants (its own, its ancestors' and descendants', and what was shared to it), and on its parent's inbox (`ask`, `report`, `escalate`, its own `inbox`). Spawning and acting on descendants (`spawn`, `send`, `steer`, `integrate`, `cancel`, `discard`, `answer`, the plan and approval answers, `graph apply`) need an envelope whose `max_depth` is above 0; a leaf asking for them is refused `denied`, with the reason. So a leaf can publish what it made, ask its parent a question and wait for the answer, and report, but never touch another branch's work.

Every local harness Branchyard starts gets `BRANCHYARD_BRANCH`, `BRANCHYARD_ROOT`, `BRANCHYARD_BY` and `by` first on `PATH` (when `by` was found: `TaskOptions::delegation_cli`, the running `by`, `by` beside it, or on `PATH`), delegated or not. `by` inside a harness without a token refuses to act, so it never acts with your authority by accident. The variable names avoid `KEY`, `SECRET` and `TOKEN`, which Codex strips from its shell commands' environment by default (the `*KEY*`, `*SECRET*` and `*TOKEN*` patterns are in the codex-cli 0.157.1 binary).

Nothing is written to the branch's worktree. The Python module, the skill and the tokens live under `.branchyard/`, which git ignores through `info/exclude`, so they never reach a candidate. A test checks that a delegating turn that changes nothing leaves its worktree clean and produces no candidate.

### Projection per harness

The delegation server and skill reach the harness through [provisioning](provisioning.md), the same path as a task's own `--mcp` servers and `--instructions`: Branchyard's server is listed first, and a task's instructions are joined with the skill (a Claude Code plugin then carries neither; the appended system prompt carries both). Each harness's provisioner keeps them on the driver's session channel below.

| Harness | MCP server | Skill | Evidence |
|---|---|---|---|
| Claude Code (stream-json) | `--mcp-config <file>`, a 0600 file holding `{"mcpServers": {"branchyard": {"type": "stdio", "command", "args", "env"}}}`, the JSON the Agent SDK passes for its `mcpServers` option; a file, because the token is a variable and a command line is readable by every process on the host | `--plugin-dir .branchyard/plugin`, a plugin holding the skill | Claude Code 2.1.283, no model call: `initialize` lists `branchyard:delegate`; `mcp_status` reports the `by mcp` server connected. `claude --help`: `--mcp-config <configs...>`, `--plugin-dir <path>` |
| Codex (App Server) | `config: {"mcp_servers": {"branchyard": {command, args, env}}}` on `thread/start`, `thread/resume` and `thread/fork` | `developerInstructions` on the same requests | `codex app-server generate-json-schema` (0.157.1): all three requests carry `config` (open object) and `developerInstructions` (string). Live, no model call: `thread/start` accepted both, and `mcpServerStatus/list` showed `by mcp` connected with its seven tools. Resume and fork were not checked live. |
| Antigravity | In a private home (`--isolated`): `mcpServers` in `~/.gemini/config/mcp_config.json` (mode 0600, since the server's variables carry the token); otherwise refused | In a private home: a managed block in `~/.gemini/GEMINI.md`; otherwise refused | Scion's Antigravity provisioner; not checked against `agy` |
| Pi, Amp | Refused | Refused | No verified way to pass either, so a turn asking for delegation fails with the driver's refusal rather than running without the tools |
| ACP agents | Stdio servers in `mcpServers` on `session/new`, `session/resume` and `session/load` | No instructions field exists, so the first prompt of each session starts with the skill between `<branchyard-instructions>` tags; the recorded prompt is unchanged | Frames deserialize as `agent-client-protocol-schema` 1.9.1 request types; the fake agent reads the servers, starts `branchyard-mcp` and calls it |

The skill is a Claude Code skill, [`plugins/branchyard/skills/delegate/SKILL.md`](../plugins/branchyard/skills/delegate/SKILL.md), with `name` and `description` frontmatter. It teaches when to delegate and how: decompose, pick a harness per subtask, set budgets within the envelope, end the turn and be woken or wait with `by wait`, integrate (several together when they share a check), cancel, with worked examples for `by` and Python. You can also load the plugin yourself: `claude --plugin-dir plugins/branchyard`.

## The CLI

Inside a delegating harness, each command acts as the harness's branch, on its descendants only. Outside a harness, the same commands act with your authority, and the envelope still applies.

| Command | Inside a harness | Outside a harness |
|---|---|---|
| `by spawn "<prompt>" \| --prompt-file PATH [--seat S] [--harness H] [--name N] [--base REV] [--budget-usd X] [--max-turns N] [--max-minutes N] [--check "CMD"] [--max-depth N] [--max-children N] [--harnesses H,H] [--deny T,T] [--depends-on A,B [--after integrated]] [--bind SCRATCH:ACCESS] [--connector GRANT] [--plan] [--model M] [--wait]` | Creates a child of this branch and returns once it has started (or, with `--depends-on`, once it is created waiting); `--wait` waits for its turn to end. `--prompt-file PATH` reads the task from a file, `-` from standard input, for a long one (Python's `spawn` passes its prompt this way, on stdin). The output names the check the child must pass when it is integrated, and whether it is its parent's. With `--plan` the child's first turn is read-only and its plan is escalated to this branch's inbox ([plans](plans-and-goals.md#delegated-children)). `--model` picks the child's model ([models](#models)). `--parent`, `--yes`, `--ask`, `--permissions` and `--allow-unapproved-tools` are refused: the parent is the harness's own branch, and a child runs under its parent's policy | Needs `--parent <branch>`; the child runs in this process, so the command always waits. `--yes`/`--ask` answer its permissions |
| `by plan approve <child> [--edit \| --file FILE]`, `by plan reject <child> [--reason TEXT] [--replan]` | Approve a descendant's plan (its next turn runs it) or reject it (it ends, or plans again); returns once the turn has started | A person decides any branch's plan |
| `by inspect [<branch>]` | This branch, or a descendant | Any branch |
| `by events [<branch>] [--cursor N] [--limit N]` | Same | Any branch |
| `by send <branch> "<prompt>"` | Starts a descendant's next turn and returns | Runs the turn in the foreground, as before |
| `by send <branch> --retry` | Submits again the prompt of the descendant's last turn that was cut off when the engine running it stopped (the recovery note names the command), until a prompt reaches its harness; refused, with what to do instead, when there is none. MCP `send` with `retry: true`, Python `send(branch, retry=True)`, `Delegate::retry` | The same for any branch (`Branch::retry_prompt` gives the prompt); not through a server yet |
| `by send <branch> --steer "<text>"` | Adds the text to a descendant's running turn without interrupting it, and waits up to 10 s for the turn to take it | Any branch's running turn, in any process |
| `by integrate <branch> [<branch>...]` | Merges a descendant into this branch; several are merged together, in order, all or none, checked once ([below](#integrating-several-children)) | Merges delegated children into their parent; several must share it |
| `by wait [<branch>...] [--any] [--all] [--timeout S]` | Blocks until descendants settle: all of them, or the first with `--any`; without branches, its children still running. With `--timeout` it gives up, prints what is pending with `timed_out`, and exits 1 | Needs the branches; any branch |
| `by cancel <branch>` | Stops a descendant's turn and every turn below it. A child that had already stopped (ready, over budget, failed, merged, discarded) is not changed, and that is a success, not a refusal, as integrating a merged child is: the answer says `had already stopped (over budget: max_usd (--budget-usd)), so the cancel changed nothing` and what to use instead (`by integrate`, `by send` or `by discard`) | Any branch and its subtree |
| `by discard <branch> [--reason TEXT]` | Sets a settled descendant aside: it ends `discarded` with the reason, runs no more turns, is never integrated, keeps its record, worktree and cost, and, like any settled child, holds no slot in this branch's `max_children` and only what it spent of its budget; unlike a ready one, it is never sent more work, so it never holds them again. A running one is refused: cancel it first | Any branch; `by rm` then removes it and frees its name (spawning another child of that name is refused until then, and the refusal says so). It prints one line, `discarded NAME: REASON; its record, worktree and cost stay until by rm NAME`. `by rm` takes a child off its parent's children, the one list `by inspect` and `by children` both read, and its spend stays in its parent's subtree cost and budget. Not through a server yet |
| `by children [<branch>]` | This branch's descendants | Any branch's |
| `by graph show [<branch>]`, `by graph apply FILE \| --edits JSON --expected-revision N` | This branch's graph of children, or a descendant's; applies a proposal to this branch's children ([task graphs](graph.md)); `by graph apply --help` shows the edit format with an example | `graph show` any branch's; `graph apply` needs `--parent <branch>` and waits for the children; `by graph resume [--yes]` starts dependents no engine started |
| `by plan approve <child> [--file FILE]`, `by plan reject <child> [--reason TEXT] [--replan]`, `by approvals allow <id>` | Answer a descendant's plan, or an approval its turn waits on ([effects](effects.md)) | A person answers any branch's |
| `by artifact publish FILE [--name N] [--media-type TYPE] [--label K=V]`, `by artifact list`, `by artifact get ID --out PATH`, `by artifact share ID --to BRANCH` | Every delegated branch, a leaf included, as itself ([storage](storage.md)) | Needs `--branch <branch>` |
| `by scratch create NAME`, `by scratch list`, `by scratch lock NAME`, `by scratch unlock NAME`, `by scratch share NAME --to BRANCH` | The same | Needs `--branch <branch>` |
| `by ask`, `by report`, `by escalate`, `by answer`, `by inbox` | Message the parent, or answer a descendant ([below](#inbox)); every delegated branch may ask, report, escalate and read its inbox | Needs `--as <branch>` |

`by <command> --help` marks each flag a harness may not pass, with the reason, and says which commands a server does not offer yet.

Every command takes `--json`. With it, stdout holds exactly one JSON value, and harness activity goes to stderr. A failure prints `{"error": {"kind": "...", "message": "...", "detail": {...}}}` and exits 1; `detail` is there when the error carries more than its message. A `check_failed` integration that left out siblings sharing the check carries `{check, inherited_from, siblings, unsettled, integrate_together}`, and its message names them and ends with the command to run (``... integrate them together, `by integrate a b c` ``). The broker passes `detail` through, the MCP tools put the same text in the tool result, and Python raises `CheckFailedError` with `integrate_together` and `unsettled`. Kinds are stable: `denied` (envelope, budget or authority), `running`, `not_running`, `steer_refused`, `unknown_branch`, `no_candidate`, `conflict`, `check_failed`, `target_moved`, `dirty_target`, `unsupported`, `state`, and the rest of `branchyard::Error::kind`.

### Integrating several children

A child inherits its parent's check unless its spawn gives one (`Spawned.check`, and `check_inherited`, say which; so does `by spawn`'s output and `--help`). The check runs on the merge when the child is integrated, so when the parent's check runs the whole test suite, siblings that each implement part of it pass it only together. `by integrate a b c` (`Delegate::integrate_all`, `branchyard.integrate("a", "b", "c")`, the MCP `propose_integration` with `branches`, `POST …/integrate` with `with`) merges their candidates in the order given in one temporary worktree, runs each distinct check of theirs once on the final merge, and moves the parent's branch with one compare-and-swap: all of them, or none. One merge commit per child sits on the parent's line, so each child's merge is still its own. A conflict names the child that conflicted, the children merged before it and the files (`conflict`, the message `b conflicts with by/root plus a in calc.py; nothing was integrated`). Each child's merge is journaled before the swap, as a single integration's is ([durability](durability.md#journaled-steps)), and every child is recorded `merged`.

**Already contained is not an error.** A child whose candidate the parent's branch already contains, say brought in by a sibling that merged it, is recorded `merged` through the commit that brought it in, and the result says `already: true` and `via` (`5a8d4a6 Merge words (b1242a7...) into by/root`). Whenever the parent's branch moves, however it moved (an integration, a merge its harness ran itself, a turn's checkpoint), Branchyard records as merged each of its `ready` or `interrupted` children whose candidate it now contains, with a warning on the child's log naming the commit; what depends on them is looked at again ([task graphs](graph.md#dependencies)). Never `git merge` children by hand to get around a check: their status follows, but the check does not run.

### JSON shapes

These are the Rust types' serde forms, identical across `by --json`, the Python module and the MCP tools.

| Command | Result |
|---|---|
| `spawn` | `Spawned`: `{name, git_branch, harness, profile, base, depth, status, budget: {max_usd, max_turns, max_minutes}}`, and `seat` for a child spawned by seat, `check` (with `check_inherited: true` when it is its parent's). With `--wait`, or outside a harness: `Inspection` |
| `inspect` | `Inspection`: `{name, status, harness, profile, parent, children, depth, turns, candidate, cost_usd, subtree_cost_usd, max_usd, remaining_usd, reserved_usd, reserving_children, settled_children_usd, envelope, allowed_harnesses, last_message}`, `check` with `check_inherited: true` when it is its parent's and `check_shared_with`, the siblings sharing it that may still be integrated (`by inspect` prints `check  python3 run_tests.py (inherited from meta); shared with b, c, so they are integrated together: by integrate a b c`), and for a branch in a rig its `seat` and the `seats` it may spawn. `last_message` is the harness's final message of its last turn (its text after its last tool call), not every text of the turn run together; one longer than 4000 characters keeps its beginning and its end |
| `events` | `EventPage`: `{branch, events: [{at_ms, activity}], next_cursor, total}` |
| `send` | `Sent`: `{name, status}` |
| `send --steer` / `steer` | `Steer`: `{id, branch, by, text, requested_at_ms, state, boundary}`, `state` `{"state": "accepted" \| "written" \| "pending"}`; a refusal is the error `steer_refused`, carrying the `Steer`. See [steering](#steering-a-child) |
| `integrate` | `Merged`: `{branch, target, previous, commit}`, and `already: true` with `via` when the target already contained the candidate. With several branches, `MergedAll`: `{target, previous, commit, branches: [Merged], checks}`. Merges stack, so each `Merged`'s `previous` is the merge before it and `previous..commit` is its own range; `checks` are the distinct checks run once on the result. `by integrate a b` prints one line per merge with that range, then `by/meta moved once: X..Y, after `CHECK` passed once on the result` (or `with no check`) |
| `wait` | `Waited`: `{settled: [Inspection], pending: [branch]}`, and `timed_out: true` when the timeout passed first |
| `cancel` | `Cancelled`: `{cancelled: [branch]}`, and, when nothing was running, `already: true` and `note`: what the branch is and what to use instead |
| `discard` | `Inspection` of the discarded branch, `status` `{"state": "discarded", "reason"}` |
| `children` | `Children`: `{branch, descendants: [BranchInfo]}` |
| `graph show` / `graph` | `Graph`: `{branch, revision, children: [{name, status, depends_on, bindings, seat}], dependencies: [{dependent, prerequisite, after}]}` |
| `graph apply` / `apply_graph` | `GraphApplied`: `{branch, revision, spawned: [Spawned], dependencies}`; a stale revision is the error `stale_revision` |

`status` is `{"state": "running" | "waiting" | "waiting_on_children" | "ready" | "no_changes" | "interrupted" | "budget_exceeded" | "failed" | "blocked" | "merged" | "discarded" | "awaiting_plan_approval", ...}`; `waiting` and `blocked` are for a child with prerequisites ([task graphs](graph.md#dependencies)), and a dependent of a discarded child is `blocked`; `waiting_on_children` is for a parent whose turn ended while its children ran ([above](#waiting-on-children)). A limit's name in a status or a refusal says the flag that sets it: `budget_exceeded` with `max_usd` is `--budget-usd` (inspect's text shows `over budget: max_usd (--budget-usd)`), and a spawn refused for its cost says `max_usd (--budget-usd) 0.6 exceeds what meta has left, $0.3000`. A harness is named the same way everywhere, by the harness ID a person types and the profile it resolved to: `spawned x on claude-code (claude-code-stream-json)`. inspect names the envelope's harnesses; an empty list is the branch's own profile, shown as `harnesses: claude-code (claude-code-stream-json) only (its own)`, and `allowed_harnesses` in JSON lists what `--harness` may name (`["claude-code-stream-json"]` for an empty list). An envelope's `max_wakes` is omitted while it is the default, 8. `Spawned` has `depends_on`, and `Inspection` `graph_revision`, `depends_on` and `bindings`, each omitted when empty. `candidate` is `{commit, files_changed, insertions, deletions}` or null. `events` without `--cursor` returns the most recent; pass `next_cursor` back to continue.

## Python

The module is standard library only. It finds `by` from `BRANCHYARD_BY`, else on `PATH`, and raises `DeniedError`, `RunningError`, `NotRunningError`, `SteerRefusedError`, `NotFoundError`, `StaleRevisionError`, `CheckFailedError` or `BranchyardError`, each with the `kind` and the error's `detail`. `branchyard.steer(branch, text)` adds to a running child's turn; `branchyard.discard(branch, reason=None)` sets a settled one aside; `branchyard.graph()` and `branchyard.apply_graph(edits, expected_revision)` reach [task graphs](graph.md), and `spawn` takes `depends_on`, `after`, `bindings`, `max_children`, `harnesses` and `plan`. `branchyard.wait_all(*branches, timeout=None)` and `wait_any(...)` run `by wait` and return a `Waited`, raising `RunningError` when the timeout passes; `wait(branch)` is the same for one branch. `branchyard.integrate(a, b, ...)` integrates several together and returns a `MergedAll`; one branch returns a `Merged`. A `status` (and a `Steer`'s `state`) is a `Status`: still the dict `by --json` gives, so `status["state"]` works, whose keys also read as attributes (`status.state`, `status.reason`, `status.limit`) and which compares equal to its state's name (`status == "ready"`); `Inspection.state` is the same. `branchyard.publish(path, name=, labels=, media_type=)` records the artifact's media type.

```python
import branchyard

me = branchyard.inspect()
child = branchyard.spawn("Port the tokenizer to the new API; run its tests",
                         harness="codex", name="tokenizer",
                         budget_usd=(me.remaining_usd or 0) / 2)
done = branchyard.wait(child.name, timeout=1800)
if done.status.state == "ready":
    branchyard.integrate(child.name)
```

Its source is [`sdk/python/branchyard.py`](../sdk/python/branchyard.py); the engine installs the same file for each delegating turn.

## Rust

```rust
use branchyard::{Budget, Delegate, Spawn};

// Inside a harness: the branch the engine started this process for.
let me = Delegate::from_env()?;
let child = me.spawn(Spawn {
    harness: Some("codex".into()),
    budget: Budget::usd(0.5),
    ..Spawn::new("Make tests/parser_test.rs deterministic")
})?;
let done = me.wait(&child.name, std::time::Duration::from_secs(1800))?;
me.integrate(&done.name)?;
// Several together, checked once; a wait on the store's notifications.
let waited = me.wait_for(&["tokenizer", "formatter"], false, None)?;
me.integrate_all(&["tokenizer", "formatter"])?;
```

`Yard::as_branch(token)` finds the branch a token was issued to, in the engine's process or through its broker. `Branch::delegate(options)` acts as a branch with your own authority. `Delegate::call(tool, json)` takes the MCP tools' arguments and returns their results. `Branch::wait_subtree` waits until no descendant is running: it joins those on this process's threads, and waits for any another process drives through its durable status, recovering one whose engine stopped ([durability](durability.md#waiting-for-turns-in-other-processes)). Call it before the process exits, or the children on its threads are left to recovery. It also wakes a parked branch of the subtree, and the branch itself, whose children have settled, and waits for that turn. `Delegate::wait` waits for one branch the same way; `Delegate::wait_for(branches, any, timeout)`, `wait_any` and `wait_all` (and `Yard::wait_for`, with your authority) for several, returning a `Waited`.

## MCP tools

`spawn`, `inspect`, `events`, `send`, `steer` (`{branch, text}`), `propose_integration` (`{branch}`, or `{branches}` to integrate several together), `wait` (`{branches?, any?, timeout_seconds?}`, returning `Waited`), `cancel`, `discard` (`{branch, reason?}`), `children`, `apply_graph` (`{expected_revision, edits}`) and `graph` (`{branch?}`) ([task graphs](graph.md)), the storage tools (`publish_artifact`, `list_artifacts`, `get_artifact`, `share_artifact`, `create_scratch`, `list_scratch`, `share_scratch`, `lock_scratch`, `unlock_scratch`; see [storage](storage.md)), `ask`, `report`, `escalate`, `answer` and `inbox` (`{unread?}`), `approve_plan` (`{branch, edited?}`) and `reject_plan` (`{branch, reason?, replan?}`) ([plans](plans-and-goals.md#delegated-children)), `answer_approval` (`{id, allow, reason?}`, [effects](effects.md)), with the arguments of the CLI flags (`spawn`'s `plan` makes the child plan first) (`budget` is `{max_usd, max_turns, max_minutes}`; `check`, `harnesses`, `deny`, `depends_on` and `connectors` are arrays; `seat` names a rig seat; `bindings` is `[{scratch, access}]`; `ask`'s `wait_seconds` blocks for an answer). Refusals come back as tool results with `isError: true` and the reason, so the model can adjust; a malformed call is a JSON-RPC error. A leaf's server lists the same tools, and refuses the ones its token does not reach. The server uses the official Rust SDK, `rmcp` 3.4, server role and stdio transport only. It is `by mcp`, or the standalone `branchyard-mcp` binary.

## Inbox

A branch can message another branch it has authority over, not only inspect it: a running turn can ask its parent a question, report progress, escalate a problem, and a parent can answer. Every message is durable in the store (both SQLite and PostgreSQL), typed `{id, from, to, kind, text, in_reply_to, at}` where `kind` is `question`, `report`, `escalation` or `answer`, and is also an event (`Activity::Message`) on both the sending and the receiving branch's log, so `by events`/`log` show it.

Authority follows the delegation tree, checked the same way spawning is:

| `kind` | May go to |
|---|---|
| `question`, `report` | Only the sender's own parent |
| `escalation` | The sender's parent, always; further up an ancestor only if the sender is in a [rig](rigs.md) and its seat's `escalates_to` names that ancestor's seat |
| `answer` | From a branch to any of its own descendants, not only a direct child |

A leaf branch (`max_depth` 0, such as a leaf rig seat) asks, reports, escalates and reads its inbox like any other: messaging is a capability of every delegated branch, not of depth. It cannot `answer` (it has no descendants) or spawn. A parent answers a leaf's question with `by answer`, and the leaf's `by ask --wait SECS` returns with the answer.

Every surface reaches the same five operations, with the same authority and the same JSON:

| Command | Tool / Python / Rust | What |
|---|---|---|
| `by ask "<text>" [--wait SECS]` | `ask` / `branchyard.ask(text, wait=None)` / `Delegate::ask` | Ask your parent a question. Without a wait, returns once it is sent (`Asked{message, answer: None}`). With one, blocks — in the process running your turn, so across processes when reached through the broker — for up to that long for an answer; a wait that passes with no answer yet is not an error, `answer` is `None`. |
| `by report "<text>"` | `report` / `branchyard.report` / `Delegate::report` | Report to your parent; no answer is expected. |
| `by escalate "<text>"` | `escalate` / `branchyard.escalate` / `Delegate::escalate` | Escalate to your parent, or, in a rig, further up if your seat's `escalates_to` allows it. |
| `by answer <message-id> "<text>"` | `answer` / `branchyard.answer` / `Delegate::answer` | Answer one of your own descendants' messages (usually a question), addressed back to whoever sent it. |
| `by inbox [--unread]` | `inbox` / `branchyard.inbox(unread=False)` / `Delegate::inbox` | Every message addressed to you, oldest first; `--unread` for only what has not yet been delivered to a turn. A message steered into your running turn counts as unread in that turn until it ends (the JSON lists its id in `steered_this_turn`): the harness takes steered input only at its next step, so an `--unread` made in between would otherwise miss it. A message is durable before `by ask`, `report` or `escalate` returns; a child's text saying it asked, written before it ran the command, may come first. |

Outside a harness, each of these needs `--as <branch>` (there is no other way to say who is asking); a person then acts with their own authority, bounded the same way. `by --remote` reaches the same operations over HTTP (`GET …/inbox`, `POST …/ask|report|escalate|answer`), with the same JSON and the same refusals; `by ask --remote --wait` blocks on the server, capped at 120 seconds so one request cannot tie up a worker indefinitely — poll `inbox` for a longer wait.

### Delivery

A message sits *pending* until it is acknowledged. Acknowledging it (marking it delivered) is the same store write as handing it to a turn, so a crash never delivers a message twice and never silently drops one — the same intent-before-effect discipline as [durable turns](durability.md). There are two paths, and each records `Activity::MessagesDelivered { ids, via }` on the recipient's log, `via` being `{"path": "steer", "steer": <id>, "boundary": <name>}` or `{"path": "turn_start", "boundary": "turn_start"}`; `by log` shows it as `delivered #7 into the running turn (steered input 3, claude_next_model_call)` or `delivered #7 at the turn's start (turn_start)`. For a steered message the event follows the state by a moment: the engine records it just after the store marks the message delivered, so a reader can briefly see a message delivered whose event has not been appended yet.

`boundary` names the protocol boundary the message actually landed at, per harness, from [`Driver::steer_boundary`](../crates/branchyard-harness/src/lib.rs) — the same mechanisms `docs/harness-integration.md` "Steering a running turn" documents (idea from Straitjacket's cross-harness relay, `docs/comparison.md` Absorption plan → Straitjacket): `claude_next_model_call` (queued as a stream-json `user` message, delivered before the next model call), `codex_turn_steer` (`turn/steer`, recorded once the model response in progress finishes), `pi_steer` (the `steer` command, delivered before the next model call within the same run), or `acp_session_steering` (the `_session/steering` extension, only when the agent advertises it). A driver that cannot steer at all reports `unsupported` (never reachable, since [`try_deliver_now`](../crates/branchyard/src/inbox.rs) never delivers through it); `turn_start` is the same for every profile, since prepending to the prompt is not protocol-specific. The field is additive and defaults on read (`"not_recorded"` for a `steer` row logged before this field existed, `"turn_start"` for `turn_start`), so an older event log still deserializes.

**Into a running turn, by steering.** Every `Yard` — so the SDK, `by` and `by serve` alike — starts with one `DeliveryHook`, `SteerDelivery`. When a message is sent and its recipient has a running turn, the hook queues it as [steered input](#how-it-runs) for that turn from the sender (`Activity::Steered { by: <sender> }` on the recipient's log), rendered as the same `<branchyard-inbox>` block as below with one message in it, and waits up to 2 seconds for the engine running the turn, in whichever process, to write it to the harness. The steer is linked to the message in the same store transaction that queues it, and the engine marks the message delivered in the same transaction that settles the steer as written or accepted; if the harness then refuses it, the same settle returns the message to pending. So `delivered` flips exactly when the running turn has the text, whether or not the sender was still waiting. No running turn (`NotRunning`), a profile that cannot take input mid-turn (`Unsupported`), or a harness that refuses it at runtime (an ACP agent without the `_session/steering` extension, say) leaves the message pending for the next turn's start.

**At the start of the next turn.** Otherwise a branch's pending messages are given to it at the start of its next turn: prepended to the prompt it actually submits, as one `<branchyard-inbox>...</branchyard-inbox>` block, oldest first, each line `[#id] kind from sender: text`. The block is bounded (at most 20 messages or about 8,000 characters at once); whatever does not fit stays pending, noted as `...and N more messages queued for a later turn`. Only the messages actually included are acknowledged, so a large backlog drains gradually across turns rather than in one giant prompt. The recorded `Activity::Prompt` and the journaled `submit` step hold the combined text, so recovery's replay guarantee ([durability](durability.md)) covers it too: a crash before the prompt reaches the harness leaves those messages pending, and one after never gives them again.

**Never both.** A message sent while its recipient's turn is still opening is steered (steered input waits for the prompt to be submitted), so that turn's start skips any message whose steer is still queued for the same turn. If the start reads the message just before the steer is queued and delivers it in the prompt, the steer later finds it already delivered and is refused unwritten. A steer queued for a turn that ended first is refused (or, if its engine died, is never written), and the message goes out at the next turn's start.

`Yard::set_delivery_hook` replaces `SteerDelivery` with another hook (for a caller that keeps a branch's turn open outside this engine); `Yard::clear_delivery_hook` removes it, so that every message waits for the recipient's next turn.

## The envelope

| Limit | Rule |
|---|---|
| Depth | `max_depth` counts levels below a branch. A child's is at most one less than its parent's; at 0, it may not spawn, and keeps the tools every delegated branch has (itself, its storage, its parent's inbox). |
| Width | `max_children` per branch, counting its live children: running, waiting, blocked, awaiting plan approval or waiting on its own children. A settled child (ready, no changes, interrupted, over budget, failed, merged, discarded) does not count, and neither does a removed one; sending a settled child more work is refused while the live ones fill the envelope. A child's is at most its parent's. |
| Harnesses | Harness or profile IDs children may run; empty means the parent's own profile only (`by inspect` names it). A child may be allowed only what its parent is. |
| Cost | A child's `max_usd` (`--budget-usd`) must fit in what its parent has left: the parent's limit minus its own spend minus what its children hold ([budgets](#budgets)). A parent with a cost limit must give each child one. The parent's own turns stop once its spend plus what its children hold reach its limit. |
| Turns and duration | A child's are at most its parent's, and default to them. |
| Permissions | A child runs under its parent's policy with the parent's added denials first (`by spawn --deny`), and the denials its root was started with (`by run --deny`, `TaskOptions::deny`). Nothing a child asks for widens it. |
| Connectors | A child's grant is its parent's, its seat's or its own ask, intersected with its parent's ([below](#connectors)). |

A child's limits and denials are stored with it and bound every later turn, whoever sends it.

### Budgets

A child holds part of its parent's budget, and how much depends on whether it can still spend without asking:

- **Live** (running, waiting for prerequisites, blocked, with a plan awaiting approval, or waiting on its own children): its whole `max_usd`, or what its subtree has spent if that is more. It may spend up to its limit without its parent's say, so that much is set aside.
- **Settled** (ready, no changes, interrupted, over budget, failed, merged, discarded): only what its subtree spent. The rest of its limit is its parent's again at once, to spend or to give another child.
- **Removed**: what its subtree spent when it was removed. The store writes that figure into the parent's record as it deletes the child (the record's `removed` ledger, kept by every later write of the parent), so neither `subtree_cost_usd` nor what the parent has left forgets money that was spent.

A **ready** child the parent may still send to is settled: it holds what it spent until it is sent something. A **discarded** one is settled for good: a send to it is refused. A `send` (or a delegated `send` through `by`, Python or MCP) to a settled child makes it live again, so its limit must fit again: it must fit in the parent's `max_children`, and if the part of its limit it has not spent is more than its parent has left, its limit is narrowed to what is left (`max_usd` becomes its spend so far plus what the parent had left) and its log records a warning saying so. A parent with nothing left refuses the send. A person's own `by send` to a delegated child bypasses this, as it bypasses the parent's other choices; the child's own limit still holds.

`inspect` shows the split: `reserved_usd` is what its `reserving_children` live children hold, `settled_children_usd` what its settled and removed children spent, and `remaining_usd` its `max_usd` less its own spend and both. `by inspect` prints it as `budget $R of $M left ($X reserved by N live children, $Y spent by settled ones)`.

A running branch's own cost is known while its turn runs. Each model call's usage, as the harness reports it during the turn (Claude Code's `assistant` frames carry their call's tokens and model), is priced from the catalog (`catalog/pricing.toml`) or taken from the harness's own per-call figure, added to what the branch had spent, written to its record, and checked against its limit; the harness's cumulative figure replaces the estimate whenever it reports one (Claude Code's at each `result`). A parent therefore sees its running children's spend, and a branch inspecting itself sees its own instead of `cost unknown`. The estimate runs low by the output still streaming when a call's last block arrived.

Branchyard also ships an opt-in permission rule, `Policy::allow_delegation_commands(by_path)` or `--allow-delegation`. It allows exactly the harness's shell commands that run `by` (by name, or the exposed path) with an operation the [operations table](../crates/branchyard/src/operations.rs) allows inside a harness (`spawn`, `inspect`, `events`, `send`, `wait`, `integrate`, `cancel`, `discard`, `children`, `graph show`/`graph apply`, the `artifact` and `scratch` commands but export and import, `ask`, `report`, `escalate`, `answer`, `inbox`, `plan approve`/`plan reject`, `approvals allow`), as a single simple command: plain or quoted words, no variables, substitutions, globs, redirections, pipes or command lists. It looks through one `sh -c` or `bash -lc` wrapper, which is how Codex reports commands. Like any rule it is ordered, so an earlier deny, such as one a parent imposed, still wins. The subcommands act within the envelope, so the rule grants nothing beyond it. It trusts `PATH` to resolve `by` to the one the engine put first; a harness that can rewrite its `PATH` can already run anything.

## Seats

A branch started from a [rig](rigs.md) (`by rig run`, or `TaskOptions::seats`) spawns only by seat: `by spawn --seat NAME`, `branchyard.spawn(..., seat=NAME)`, `Spawn::seat`, or the MCP tool's `seat`, and only the seats its own seat `delegates_to`. The seat fixes the child's harness, check, isolation and instructions and sets limits and denials the request may only narrow; the envelope above still bounds everything. A branch outside a rig cannot name a seat. See [rigs](rigs.md#spawning-by-seat).

## Models

A child runs the model its spawn names (`by spawn --model M`; `Spawn::model`; the MCP `spawn` tool's and a graph proposal's `model`; `branchyard.spawn(..., model=M)`; `model` on the server's spawn request), else its seat's (`model` in its seat's provisioning), else its parent's (`by run --model`), else its harness's default. `M` is a model name or a size alias (`small`, `medium`, `large`, `extra-large`) where the harness defines one ([provisioning](provisioning.md)). A cheaper model suits mechanical work (a rename, a format fix, a port its check verifies, gathering facts), so the same budget goes further; keep the default for design and debugging.

Whether a harness can be given a model is its driver's `model` capability ([compatibility](compatibility.md)): the Claude Code stream-json, Codex app-server, Antigravity and Pi drivers pass it on (Claude Code as `--model`); the ACP and Amp drivers cannot, and a spawn naming a model for one is refused as `unsupported` with the driver's reason (`gemini-cli-acp cannot be given a model: ACP v1 has no model parameter; ...`) before anything is created. A blank model is refused as `denied`. `by inspect <child>` shows its model (`model` in the JSON, omitted for the harness's default), as do `by ls --json` (`model` in each branch) and `Spawned::model`; `by spawn` prints `spawned rename on claude-code (claude-code-stream-json) with model haiku from ...`, and the spawn's event says `started on ... with model haiku`.

## Connectors

A child's connector grant ([connectors](connectors.md)) is only ever narrower than its parent's. It is what the spawn asks for (`by spawn --connector GRANT`, repeatable; `Spawn::connectors`; the MCP `spawn` tool's and a graph proposal's `connectors`; `branchyard.spawn(..., connectors=[...])`), else its seat's `connectors` (a seat that names none gives none), else its parent's grant, and it is always intersected with the parent's: each entry keeps only the operations, mode, confirmation and account both allow. An entry the parent allows nothing of is refused as `denied`, naming it and the parent's grant. A later send that gives a child a new grant is narrowed the same way. The gateway enforces whatever grant a turn's token carries; the narrowing is Branchyard's.

| Parent | Asked | Child |
|---|---|---|
| `github:write:issues.*`, `linear:read` | nothing | the same |
| the same | `github:write+confirm`, `linear:write` | `github:write:issues.*`, `linear:read` |
| the same | `github:read:issues.list` | `github:read:issues.list` |
| the same | `slack:read` or `github:read:pulls.*` | refused |

## Authority and its limits

A harness never inherits Branchyard's own variables: every `BRANCHYARD_*` variable in the engine's environment, such as `BRANCHYARD_REMOTE` or an outer harness's token, is removed, and the engine then sets only this branch's.

The acting branch comes from the token, never from a name in a request: `branch` arguments name targets. An operation on anything other than the acting branch's descendants is refused (`inspect` and `events` also accept the branch itself), and so is every operation the branch's capabilities do not include (see [what a delegated harness gets](#what-a-delegated-harness-gets)): the engine checks the operation's capability on every call, so a leaf's token reaches its own branch, its storage and its parent's inbox, and nothing else. Tests refuse each of a leaf's out-of-scope calls through `by`: spawning, inspecting its parent, acting as another branch with `--branch`, reading a sibling's artifact, cancelling, discarding or integrating. A token is issued when a delegating turn starts and revoked when it ends; the engine checks it on every request, and the MCP server and `by` refuse a token that matches no running turn before asking.

In local mode this stops honest mistakes: a harness that confuses branches, reaches for its parent, or escapes its budget by asking. It does not stop a hostile harness running as your user. Such a harness can read every token file in `.branchyard/`, connect to the broker's socket, and act as any branch with a running turn; it can also run git and `by` directly with your authority. Server mode, with harnesses in sandboxes and tokens that never leave the server, is where the envelope becomes a boundary.

## Through a server

A server started with `by serve --allow-delegation` offers the same tools to the harnesses it runs. The harness runs on the server's host, so the mechanism is the one above: the engine in the server process issues the token, listens on the broker socket under the served repository's `.branchyard/delegation/`, and puts the server's `by` first on the harness's `PATH` (`--by-path` chooses it; by default it is the server's own executable under `by serve`). Children run on the server's threads, and the operation that ran the parent's turn waits for them, as `by run` does, and reports them as `descendants`.

```sh
by serve --allow-delegation                      # on the server
by --remote URL run "Split this and delegate" --delegate --allow-delegation --yes
by --remote URL spawn "Port the tokenizer" --parent root --budget-usd 1 --yes --json
by --remote URL inspect kid --json; by --remote URL events kid --cursor 0 --json
by --remote URL children root --json; by --remote URL integrate kid --json
```

A person reaches the same operations remotely, with the server's authority, exactly as they act locally outside a harness: `spawn` and `graph apply` need `--parent` and are bounded by the parent's envelope, `integrate` merges a child into the branch that delegated it, and `inspect`, `events` and `children` read any branch. `by --remote … send --json` prints `Sent` once the turn and its subtree have ended on the server. The JSON is the same as local mode's, and refusals print the same `{"error": {"kind", "message"}}`. The HTTP routes are in [the server reference](server.md#api-reference).

Without `--allow-delegation`, the server refuses an envelope, `allow_delegation`, a spawn, and a send to a branch that was given an envelope (`403 delegation_not_allowed`); reading and integrating need no opt-in.

## How it runs

Children run on threads of the process that runs their parent's turn, whether that process is `by run` or your own program. A spawn returns once the child's record exists and its thread has started. While any of its turns may delegate, the engine listens on a Unix socket in `.branchyard/delegation/`, or in the temporary directory when that path is too long; `by`, the Python module and the MCP server reach it there. A socket, unlike a loopback port, stays reachable from a sandbox without network that can still see the repository.

`by cancel` records a durable cancel request for the running turn (see [durability](durability.md#cancellation-and-deadlines)), which the engine running it checks every 100 ms, in whichever process that is; the turn is interrupted like a budget stop, and ends `interrupted`. A request is bound to the turn it was asked of and never stops a later one. A branch none of whose turns submitted a prompt, such as a child cancelled before its harness opened a session, has no conversation to continue: a later `send` starts a fresh session with only the prompt it sends, and records a warning that says so. A branch that ran a prompt and has no session, such as one whose engine stopped before Claude Code named its session, also starts a fresh session, whose prompt begins with every prompt the branch was given before (in full, with what it replied), so its task is not lost with the session; the warning says so. A turn the engine was cut off in is not submitted again on its own: the recovery note says so and names `by send <branch> --retry`, which submits that prompt again. A turn that gives no `--budget-usd`, `--max-turns` or `--max-minutes` keeps the ones the branch's turns were last given, so the `by send` that continues a branch after its engine stopped runs under the same limits and `by inspect` still shows its budget; a turn that gives one replaces it.

### Steering a child

`by send --steer`, `branchyard.steer(branch, text)`, `Delegate::steer` and the MCP `steer` tool add input to a descendant's running turn instead of waiting for it to end: queued durably and bound to that turn like a cancel, written to its harness by the engine running it, and recorded in the child's log as `steered` by the parent, and in the parent's as a `steer` delegation. They wait up to 10 seconds for the turn to take it and return the `Steer`, whose `state` says where it is:

| `state` | Meaning |
|---|---|
| `accepted` | It joined the running turn: the harness queued it into the turn in flight, and the model reads it at `boundary` (for Claude Code `claude_next_model_call`: before its next model call, still in this turn). |
| `written` | Written to the harness's input; the harness has not confirmed it within the wait. It was `delivered` before 8 October 2026, and stored rows of that name still read as `written`. |
| `pending` | Still queued in Branchyard; the engine running the turn has not written it yet. |
| `refused` | Never reached the model (an error, `steer_refused`): the harness refused it, an interrupt cancelled it, or the turn ended first. |

`by send --steer`'s text starts with the state, in these words, then says what it means, with the boundary in plain words: `accepted: it joined units's running turn, and the model reads it before its next model call (steer 1)`, `written: sent to units's harness for its running turn; the harness has not confirmed it yet (steer 1)`, or `pending: ...`. `--json` gives the `Steer` with `boundary` as the profile names it (`claude_next_model_call`).

Steered input is never queued for a later turn: input the running turn does not take is refused, and `send` without `--steer` starts the next turn. When the harness takes the input depends on its protocol ([harness integration](harness-integration.md#steering-a-running-turn)): Claude Code, Pi and Codex before their next model call, claude-agent-acp at once, interrupting the response in progress but not the turn. A child whose harness cannot take input mid-turn (Amp, Antigravity, an ACP agent without the steering extension) is refused with the reason, never interrupted in its place; a child with no running turn is `not_running`, and `send` continues it instead. The child's budget and permissions do not change.

A child's own spend counts against every ancestor through the reservations. `inspect` reports `subtree_cost_usd`, the reported spend of a branch and its descendants.

## Not guaranteed

- Cost limits for harnesses that report no cost, such as every ACP agent today.
- Tool calls longer than a harness's own MCP or shell timeout, such as an integration whose check runs for many minutes, or a `by wait` without `--timeout` (ending the turn and being woken has no such limit).
- A wake under the parked turn's own policy after its process exits: an engine in another process wakes it under the policy of the turn that settled its last child, and `resume_graph` under what it is given (on a server, the default, deny).
- A parked branch whose children never settle (one waiting on a prerequisite that is never integrated) stays parked; `by send` or `by cancel` ends that.
- Isolation. Local mode runs everything as your user.
- Delegation from a sandboxed harness. The tools reach the engine over a host socket with the host's `by`, so a turn with `--provider microsandbox` and `--delegate` fails, and a sandboxed branch runs without the tools.
- A boundary through a server. Harnesses on the server still run as the server's user unless a sandbox provider is used, and a sandboxed turn gets no tools; the envelope stops honest mistakes there too.
- Any real harness delegating end to end. The projections were checked against Claude Code 2.1.283 and codex-cli 0.157.1 without model calls; the full loop was tested against the fake ACP agent only.
- Delivering a message into a running turn. Without a `DeliveryHook` wired in (none is today), every message waits for the recipient's next turn to start; steering will fill this in.
- `ask --wait` on a server past 120 seconds; it is capped, not refused, so poll `inbox` instead for a longer wait.
