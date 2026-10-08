# Delegation

A harness running on a Branchyard branch can act as a meta-harness: it creates child branches, gives each a harness, a prompt and a budget, watches them, merges the ones it wants into its own branch, and cancels the rest. Children can delegate further when their envelope allows. This works in local mode and through a server whose operator allows it, tested against a fake ACP agent; no real harness has delegated through it yet.

There is one set of operations and one authority model. Four surfaces reach them:

| Surface | Use it when |
|---|---|
| `by spawn`, `by inspect`, `by events`, `by send` (and `by send --steer`), `by integrate`, `by cancel`, `by children`, `by graph` | The harness can run shell commands. This is the primary path: every coding harness has a shell, and `--json` output composes in scripts. |
| The Python module `branchyard` | The harness writes Python to orchestrate: loops, fan-out, waiting. It runs `by --json` for you. |
| The Rust SDK, `branchyard::Delegate` | You write the meta-harness in Rust, in or out of a harness. |
| Branchyard's MCP server (`by mcp`) | The harness cannot run commands, or its shell cannot reach the repository, but it can call MCP tools. |

Each surface calls the same operations in the engine that runs the harness's turn, so they give the same answers and refusals. Children may depend on one another, and a branch can change its graph of children in one atomic proposal; see [task graphs](graph.md). The same four surfaces also reach [artifacts and scratch areas](storage.md) (`by artifact`, `by scratch`, `branchyard.publish`/`create_scratch`, `Delegate::publish_artifact`, the MCP `publish_artifact`/`create_scratch` tools and their siblings): shared storage a branch's descendants and ancestors can read without a merge, tested the same way.

## Turning it on

```sh
by run "Split the parser rewrite into tokenizer and formatter work, delegate both, and integrate them" \
  --delegate --budget-usd 3 --max-turns 5 --yes
```

`--delegate` lets the harness create children one level deep; `--delegate=2` allows grandchildren. In the SDK, set `TaskOptions::delegation` to an `Envelope`. The envelope is stored with the branch, so later sends keep it.

`by run` waits until every branch it delegated has finished, whichever process runs it, says which are still running while it waits, and prints a table of them. A delegated branch whose engine stopped is recovered and ends `interrupted` rather than being waited for. `by ls` shows the tree.

## What a delegated harness gets

Every turn of a branch that was given delegation gets these, for the duration of that turn: a root started with `--delegate`, and every child it delegated to at any depth, including a leaf whose envelope allows no children of its own (`max_depth` 0).

| What | Where |
|---|---|
| A token for this turn | `BRANCHYARD_DELEGATION`, and `.branchyard/delegation/<branch>.json` (mode 0600) with the broker's socket |
| Its branch and repository | `BRANCHYARD_BRANCH`, `BRANCHYARD_ROOT` (the harness's working directory is its worktree, not the repository root) |
| `by` | `BRANCHYARD_BY`, and `by`'s directory first on `PATH` |
| The Python module | `.branchyard/sdk/python/branchyard.py`, first on `PYTHONPATH` |
| The MCP server | `by mcp --root <root> --branch <name>`, projected per harness (below) |
| The delegation skill | Projected per harness (below) |

Every harness Branchyard starts gets `BRANCHYARD_BRANCH` and `BRANCHYARD_ROOT`, delegating or not. `by` inside a harness without a token refuses to act, so it never acts with your authority by accident. The variable names avoid `KEY`, `SECRET` and `TOKEN`, which Codex strips from its shell commands' environment by default (the `*KEY*`, `*SECRET*` and `*TOKEN*` patterns are in the codex-cli 0.157.1 binary).

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

The skill is a Claude Code skill, [`plugins/branchyard/skills/delegate/SKILL.md`](../plugins/branchyard/skills/delegate/SKILL.md), with `name` and `description` frontmatter. It teaches when to delegate and how: decompose, pick a harness per subtask, set budgets within the envelope, wait or poll, integrate, cancel, with worked examples for `by` and Python. You can also load the plugin yourself: `claude --plugin-dir plugins/branchyard`.

## The CLI

Inside a delegating harness, each command acts as the harness's branch, on its descendants only. Outside a harness, the same commands act with your authority, and the envelope still applies.

| Command | Inside a harness | Outside a harness |
|---|---|---|
| `by spawn "<prompt>" [--seat S] [--harness H] [--name N] [--base REV] [--budget-usd X] [--max-turns N] [--max-minutes N] [--check "CMD"] [--max-depth N] [--deny T,T] [--depends-on A,B [--after integrated]] [--bind SCRATCH:ACCESS] [--connector GRANT] [--plan] [--wait]` | Creates a child of this branch and returns once it has started (or, with `--depends-on`, once it is created waiting); `--wait` waits for its turn to end. With `--plan` the child's first turn is read-only and its plan is escalated to this branch's inbox ([plans](plans-and-goals.md#delegated-children)) | Needs `--parent <branch>`; the child runs in this process, so the command always waits. `--yes`/`--ask` answer its permissions |
| `by plan approve <child> [--edit \| --file FILE]`, `by plan reject <child> [--reason TEXT] [--replan]` | Approve a descendant's plan (its next turn runs it) or reject it (it ends, or plans again); returns once the turn has started | A person decides any branch's plan |
| `by inspect [<branch>]` | This branch, or a descendant | Any branch |
| `by events [<branch>] [--cursor N] [--limit N]` | Same | Any branch |
| `by send <branch> "<prompt>"` | Starts a descendant's next turn and returns | Runs the turn in the foreground, as before |
| `by send <branch> --steer "<text>"` | Adds the text to a descendant's running turn without interrupting it, and waits up to 10 s for delivery | Any branch's running turn, in any process |
| `by integrate <branch>` | Merges a descendant into this branch | Merges a delegated child into its parent |
| `by cancel <branch>` | Stops a descendant's turn and every turn below it | Any branch and its subtree |
| `by children [<branch>]` | This branch's descendants | Any branch's |
| `by graph show [<branch>]`, `by graph apply FILE \| --edits JSON --expected-revision N` | This branch's graph of children, or a descendant's; applies a proposal to this branch's children ([task graphs](graph.md)) | `graph show` any branch's; `graph apply` needs `--parent <branch>` and waits for the children; `by graph resume [--yes]` starts dependents no engine started |

Every command takes `--json`. With it, stdout holds exactly one JSON value, and harness activity goes to stderr. A failure prints `{"error": {"kind": "...", "message": "..."}}` and exits 1. Kinds are stable: `denied` (envelope, budget or authority), `running`, `not_running`, `steer_refused`, `unknown_branch`, `no_candidate`, `conflict`, `check_failed`, `target_moved`, `dirty_target`, `unsupported`, `state`, and the rest of `branchyard::Error::kind`.

### JSON shapes

These are the Rust types' serde forms, identical across `by --json`, the Python module and the MCP tools.

| Command | Result |
|---|---|
| `spawn` | `Spawned`: `{name, git_branch, harness, profile, base, depth, status, budget: {max_usd, max_turns, max_minutes}}`, and `seat` for a child spawned by seat. With `--wait`, or outside a harness: `Inspection` |
| `inspect` | `Inspection`: `{name, status, harness, profile, parent, children, depth, turns, candidate, cost_usd, subtree_cost_usd, max_usd, remaining_usd, envelope, last_message}`, and for a branch in a rig its `seat` and the `seats` it may spawn |
| `events` | `EventPage`: `{branch, events: [{at_ms, activity}], next_cursor, total}` |
| `send` | `Sent`: `{name, status}` |
| `send --steer` / `steer` | `Steer`: `{id, branch, by, text, requested_at_ms, state}`, `state` `{"state": "delivered" \| "accepted" \| "pending"}`; a refusal is the error `steer_refused`, carrying the `Steer` |
| `integrate` | `Merged`: `{branch, target, previous, commit}` |
| `cancel` | `Cancelled`: `{cancelled: [branch]}` |
| `children` | `Children`: `{branch, descendants: [BranchInfo]}` |
| `graph show` / `graph` | `Graph`: `{branch, revision, children: [{name, status, depends_on, bindings, seat}], dependencies: [{dependent, prerequisite, after}]}` |
| `graph apply` / `apply_graph` | `GraphApplied`: `{branch, revision, spawned: [Spawned], dependencies}`; a stale revision is the error `stale_revision` |

`status` is `{"state": "running" | "waiting" | "ready" | "no_changes" | "interrupted" | "budget_exceeded" | "failed" | "blocked" | "merged", ...}`; `waiting` and `blocked` are for a child with prerequisites ([task graphs](graph.md#dependencies)). `Spawned` has `depends_on`, and `Inspection` `graph_revision`, `depends_on` and `bindings`, each omitted when empty. `candidate` is `{commit, files_changed, insertions, deletions}` or null. `events` without `--cursor` returns the most recent; pass `next_cursor` back to continue.

## Python

The module is standard library only. It finds `by` from `BRANCHYARD_BY`, else on `PATH`, and raises `DeniedError`, `RunningError`, `NotRunningError`, `SteerRefusedError`, `NotFoundError`, `StaleRevisionError` or `BranchyardError`, each with the `kind`. `branchyard.steer(branch, text)` adds to a running child's turn; `branchyard.graph()` and `branchyard.apply_graph(edits, expected_revision)` reach [task graphs](graph.md), and `spawn` takes `depends_on`, `after` and `bindings`.

```python
import branchyard

me = branchyard.inspect()
child = branchyard.spawn("Port the tokenizer to the new API; run its tests",
                         harness="codex", name="tokenizer",
                         budget_usd=(me.remaining_usd or 0) / 2)
done = branchyard.wait(child.name, timeout=1800)
if done.status["state"] == "ready":
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
```

`Yard::as_branch(token)` finds the branch a token was issued to, in the engine's process or through its broker. `Branch::delegate(options)` acts as a branch with your own authority. `Delegate::call(tool, json)` takes the MCP tools' arguments and returns their results. `Branch::wait_subtree` waits until no descendant is running: it joins those on this process's threads, and waits for any another process drives through its durable status, recovering one whose engine stopped ([durability](durability.md#waiting-for-turns-in-other-processes)). Call it before the process exits, or the children on its threads are left to recovery. `Delegate::wait` waits for one branch the same way.

## MCP tools

`spawn`, `inspect`, `events`, `send`, `steer` (`{branch, text}`), `propose_integration`, `cancel`, `children`, `apply_graph` (`{expected_revision, edits}`) and `graph` (`{branch?}`) ([task graphs](graph.md)), the storage tools (`publish_artifact`, `list_artifacts`, `get_artifact`, `share_artifact`, `create_scratch`, `list_scratch`, `share_scratch`, `lock_scratch`, `unlock_scratch`; see [storage](storage.md)), `ask`, `report`, `escalate`, `answer` and `inbox`, `approve_plan` (`{branch, edited?}`) and `reject_plan` (`{branch, reason?, replan?}`) ([plans](plans-and-goals.md#delegated-children)), with the arguments of the CLI flags (`spawn`'s `plan` makes the child plan first) (`budget` is `{max_usd, max_turns, max_minutes}`; `check`, `harnesses`, `deny` and `depends_on` are arrays; `seat` names a rig seat; `bindings` is `[{scratch, access}]`; `ask`'s `wait_seconds` blocks for an answer). Refusals come back as tool results with `isError: true` and the reason, so the model can adjust; a malformed call is a JSON-RPC error. The server uses the official Rust SDK, `rmcp` 3.4, server role and stdio transport only. It is `by mcp`, or the standalone `branchyard-mcp` binary.

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
| `by inbox [--unread]` | `inbox` / `branchyard.inbox(unread=False)` / `Delegate::inbox` | Every message addressed to you, oldest first; `--unread` for only what has not yet been delivered to a turn. |

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
| Depth | `max_depth` counts levels below a branch. A child's is at most one less than its parent's; at 0, the harness gets no tools. |
| Width | `max_children` per branch, counting children until they are removed. A child's is at most its parent's. |
| Harnesses | Harness or profile IDs children may run; empty means the parent's own profile only. A child may be allowed only what its parent is. |
| Cost | A child's `max_usd` must fit in what its parent has left: the parent's limit minus its own spend minus every other child's reservation. A child reserves its whole limit, or what its subtree has spent if that is more. A parent with a cost limit must give each child one. The parent's own turns stop once its spend plus its children's reservations reach its limit. |
| Turns and duration | A child's are at most its parent's, and default to them. |
| Permissions | A child runs under its parent's policy with the parent's added denials first (`--deny`). Nothing a child asks for widens it. |
| Connectors | A child's grant is its parent's, its seat's or its own ask, intersected with its parent's ([below](#connectors)). |

A child's limits and denials are stored with it and bound every later turn, whoever sends it.

Branchyard also ships an opt-in permission rule, `Policy::allow_delegation_commands(by_path)` or `--allow-delegation`. It allows exactly the harness's shell commands that run `by` (by name, or the exposed path) with one of the eight delegation subcommands (`spawn`, `inspect`, `events`, `send`, `integrate`, `cancel`, `children`, `graph`), as a single simple command: plain or quoted words, no variables, substitutions, globs, redirections, pipes or command lists. It looks through one `sh -c` or `bash -lc` wrapper, which is how Codex reports commands. Like any rule it is ordered, so an earlier deny, such as one a parent imposed, still wins. The subcommands act within the envelope, so the rule grants nothing beyond it. It trusts `PATH` to resolve `by` to the one the engine put first; a harness that can rewrite its `PATH` can already run anything.

## Seats

A branch started from a [rig](rigs.md) (`by rig run`, or `TaskOptions::seats`) spawns only by seat: `by spawn --seat NAME`, `branchyard.spawn(..., seat=NAME)`, `Spawn::seat`, or the MCP tool's `seat`, and only the seats its own seat `delegates_to`. The seat fixes the child's harness, check, isolation and instructions and sets limits and denials the request may only narrow; the envelope above still bounds everything. A branch outside a rig cannot name a seat. See [rigs](rigs.md#spawning-by-seat).

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

The acting branch comes from the token, never from a name in a request: `branch` arguments name targets. An operation on anything other than the acting branch's descendants is refused (`inspect` and `events` also accept the branch itself). A token is issued when a delegating turn starts and revoked when it ends; the engine checks it on every request, and the MCP server and `by` refuse a token that matches no running turn before asking.

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

`by cancel` records a durable cancel request for the running turn (see [durability](durability.md#cancellation-and-deadlines)), which the engine running it checks every 100 ms, in whichever process that is; the turn is interrupted like a budget stop, and ends `interrupted`. A request is bound to the turn it was asked of and never stops a later one. A branch none of whose turns submitted a prompt, such as a child cancelled before its harness opened a session, has no conversation to continue: a later `send` starts a fresh session with only the prompt it sends, and records a warning that says so. A branch that ran a prompt and has no session is still refused.

`by send --steer`, `branchyard.steer(branch, text)`, `Delegate::steer` and the MCP `steer` tool add input to a descendant's running turn instead of waiting for it to end: queued durably and bound to that turn like a cancel, written to its harness by the engine running it, and recorded in the child's log as `steered` by the parent, and in the parent's as a `steer` delegation. They wait up to 10 seconds for delivery and return the `Steer`. When the harness takes the input depends on its protocol ([harness integration](harness-integration.md#steering-a-running-turn)): Claude Code, Pi and Codex before their next model call, claude-agent-acp at once, interrupting the response in progress but not the turn. A child whose harness cannot take input mid-turn (Amp, Antigravity, an ACP agent without the steering extension) is refused with the reason, never interrupted in its place; a child with no running turn is `not_running`, and `send` continues it instead. The child's budget and permissions do not change.

A child's own spend counts against every ancestor through the reservations. `inspect` reports `subtree_cost_usd`, the reported spend of a branch and its descendants.

## Not guaranteed

- Cost limits for harnesses that report no cost, such as every ACP agent today.
- Tool calls longer than a harness's own MCP or shell timeout, such as an integration whose check runs for many minutes.
- Isolation. Local mode runs everything as your user.
- Delegation from a sandboxed harness. The tools reach the engine over a host socket with the host's `by`, so a turn with `--provider microsandbox` and `--delegate` fails, and a sandboxed branch runs without the tools.
- A boundary through a server. Harnesses on the server still run as the server's user unless a sandbox provider is used, and a sandboxed turn gets no tools; the envelope stops honest mistakes there too.
- Any real harness delegating end to end. The projections were checked against Claude Code 2.1.283 and codex-cli 0.157.1 without model calls; the full loop was tested against the fake ACP agent only.
- Messaging from a leaf branch (`max_depth` 0). It has no delegation tools at all, so it cannot `ask`, `report` or `escalate` either.
- Delivering a message into a running turn. Without a `DeliveryHook` wired in (none is today), every message waits for the recipient's next turn to start; steering will fill this in.
- `ask --wait` on a server past 120 seconds; it is capped, not refused, so poll `inbox` instead for a longer wait.
