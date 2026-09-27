# Delegation

A harness running on a Branchyard branch can act as a meta-harness: it creates child branches, gives each a harness, a prompt and a budget, watches them, merges the ones it wants into its own branch, and cancels the rest. Children can delegate further when their envelope allows. This works in local mode and through a server whose operator allows it, tested against a fake ACP agent; no real harness has delegated through it yet.

There is one set of operations and one authority model. Four surfaces reach them:

| Surface | Use it when |
|---|---|
| `by spawn`, `by inspect`, `by events`, `by send`, `by integrate`, `by cancel`, `by children` | The harness can run shell commands. This is the primary path: every coding harness has a shell, and `--json` output composes in scripts. |
| The Python module `branchyard` | The harness writes Python to orchestrate: loops, fan-out, waiting. It runs `by --json` for you. |
| The Rust SDK, `branchyard::Delegate` | You write the meta-harness in Rust, in or out of a harness. |
| Branchyard's MCP server (`by mcp`) | The harness cannot run commands, or its shell cannot reach the repository, but it can call MCP tools. |

Each surface calls the same operations in the engine that runs the harness's turn, so they give the same answers and refusals.

## Turning it on

```sh
by run "Split the parser rewrite into tokenizer and formatter work, delegate both, and integrate them" \
  --delegate --budget-usd 3 --max-turns 5 --yes
```

`--delegate` lets the harness create children one level deep; `--delegate=2` allows grandchildren. In the SDK, set `TaskOptions::delegation` to an `Envelope`. The envelope is stored with the branch, so later sends keep it.

`by run` waits until every branch it delegated has finished, whichever process runs it, says which are still running while it waits, and prints a table of them. A delegated branch whose engine stopped is recovered and ends `interrupted` rather than being waited for. `by ls` shows the tree.

## What a delegating harness gets

Only a turn whose envelope allows children gets these, and only for the duration of that turn:

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
| `by spawn "<prompt>" [--harness H] [--name N] [--base REV] [--budget-usd X] [--max-turns N] [--max-minutes N] [--check "CMD"] [--max-depth N] [--deny T,T] [--wait]` | Creates a child of this branch and returns once it has started; `--wait` waits for its turn to end | Needs `--parent <branch>`; the child runs in this process, so the command always waits. `--yes`/`--ask` answer its permissions |
| `by inspect [<branch>]` | This branch, or a descendant | Any branch |
| `by events [<branch>] [--cursor N] [--limit N]` | Same | Any branch |
| `by send <branch> "<prompt>"` | Starts a descendant's next turn and returns | Runs the turn in the foreground, as before |
| `by integrate <branch>` | Merges a descendant into this branch | Merges a delegated child into its parent |
| `by cancel <branch>` | Stops a descendant's turn and every turn below it | Any branch and its subtree |
| `by children [<branch>]` | This branch's descendants | Any branch's |

Every command takes `--json`. With it, stdout holds exactly one JSON value, and harness activity goes to stderr. A failure prints `{"error": {"kind": "...", "message": "..."}}` and exits 1. Kinds are stable: `denied` (envelope, budget or authority), `running`, `unknown_branch`, `no_candidate`, `conflict`, `check_failed`, `target_moved`, `dirty_target`, `unsupported`, `state`, and the rest of `branchyard::Error::kind`.

### JSON shapes

These are the Rust types' serde forms, identical across `by --json`, the Python module and the MCP tools.

| Command | Result |
|---|---|
| `spawn` | `Spawned`: `{name, git_branch, harness, profile, base, depth, status, budget: {max_usd, max_turns, max_minutes}}`. With `--wait`, or outside a harness: `Inspection` |
| `inspect` | `Inspection`: `{name, status, harness, profile, parent, children, depth, turns, candidate, cost_usd, subtree_cost_usd, max_usd, remaining_usd, envelope, last_message}` |
| `events` | `EventPage`: `{branch, events: [{at_ms, activity}], next_cursor, total}` |
| `send` | `Sent`: `{name, status}` |
| `integrate` | `Merged`: `{branch, target, previous, commit}` |
| `cancel` | `Cancelled`: `{cancelled: [branch]}` |
| `children` | `Children`: `{branch, descendants: [BranchInfo]}` |

`status` is `{"state": "running" | "ready" | "no_changes" | "interrupted" | "budget_exceeded" | "failed" | "merged", ...}`. `candidate` is `{commit, files_changed, insertions, deletions}` or null. `events` without `--cursor` returns the most recent; pass `next_cursor` back to continue.

## Python

The module is standard library only. It finds `by` from `BRANCHYARD_BY`, else on `PATH`, and raises `DeniedError`, `RunningError`, `NotFoundError` or `BranchyardError`, each with the `kind`.

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

`spawn`, `inspect`, `events`, `send`, `propose_integration`, `cancel` and `children`, with the arguments of the CLI flags (`budget` is `{max_usd, max_turns, max_minutes}`; `check`, `harnesses` and `deny` are arrays). Refusals come back as tool results with `isError: true` and the reason, so the model can adjust; a malformed call is a JSON-RPC error. The server uses the official Rust SDK, `rmcp` 3.4, server role and stdio transport only. It is `by mcp`, or the standalone `branchyard-mcp` binary.

## The envelope

| Limit | Rule |
|---|---|
| Depth | `max_depth` counts levels below a branch. A child's is at most one less than its parent's; at 0, the harness gets no tools. |
| Width | `max_children` per branch, counting children until they are removed. A child's is at most its parent's. |
| Harnesses | Harness or profile IDs children may run; empty means the parent's own profile only. A child may be allowed only what its parent is. |
| Cost | A child's `max_usd` must fit in what its parent has left: the parent's limit minus its own spend minus every other child's reservation. A child reserves its whole limit, or what its subtree has spent if that is more. A parent with a cost limit must give each child one. The parent's own turns stop once its spend plus its children's reservations reach its limit. |
| Turns and duration | A child's are at most its parent's, and default to them. |
| Permissions | A child runs under its parent's policy with the parent's added denials first (`--deny`). Nothing a child asks for widens it. |

A child's limits and denials are stored with it and bound every later turn, whoever sends it.

Branchyard also ships an opt-in permission rule, `Policy::allow_delegation_commands(by_path)` or `--allow-delegation`. It allows exactly the harness's shell commands that run `by` (by name, or the exposed path) with one of the seven delegation subcommands, as a single simple command: plain or quoted words, no variables, substitutions, globs, redirections, pipes or command lists. It looks through one `sh -c` or `bash -lc` wrapper, which is how Codex reports commands. Like any rule it is ordered, so an earlier deny, such as one a parent imposed, still wins. The subcommands act within the envelope, so the rule grants nothing beyond it. It trusts `PATH` to resolve `by` to the one the engine put first; a harness that can rewrite its `PATH` can already run anything.

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

A person reaches the same operations remotely, with the server's authority, exactly as they act locally outside a harness: `spawn` needs `--parent` and is bounded by the parent's envelope, `integrate` merges a child into the branch that delegated it, and `inspect`, `events` and `children` read any branch. `by --remote … send --json` prints `Sent` once the turn and its subtree have ended on the server. The JSON is the same as local mode's, and refusals print the same `{"error": {"kind", "message"}}`. The HTTP routes are in [the server reference](server.md#api-reference).

Without `--allow-delegation`, the server refuses an envelope, `allow_delegation`, a spawn, and a send to a branch that was given an envelope (`403 delegation_not_allowed`); reading and integrating need no opt-in.

## How it runs

Children run on threads of the process that runs their parent's turn, whether that process is `by run` or your own program. A spawn returns once the child's record exists and its thread has started. While any of its turns may delegate, the engine listens on a Unix socket in `.branchyard/delegation/`, or in the temporary directory when that path is too long; `by`, the Python module and the MCP server reach it there. A socket, unlike a loopback port, stays reachable from a sandbox without network that can still see the repository.

`by cancel` records a durable cancel request for the running turn (see [durability](durability.md#cancellation-and-deadlines)), which the engine running it checks every 100 ms, in whichever process that is; the turn is interrupted like a budget stop, and ends `interrupted`. A request is bound to the turn it was asked of and never stops a later one. A branch none of whose turns submitted a prompt, such as a child cancelled before its harness opened a session, has no conversation to continue: a later `send` starts a fresh session with only the prompt it sends, and records a warning that says so. A branch that ran a prompt and has no session is still refused.

A child's own spend counts against every ancestor through the reservations. `inspect` reports `subtree_cost_usd`, the reported spend of a branch and its descendants.

## Not guaranteed

- Cost limits for harnesses that report no cost, such as every ACP agent today.
- Tool calls longer than a harness's own MCP or shell timeout, such as an integration whose check runs for many minutes.
- Isolation. Local mode runs everything as your user.
- Delegation from a sandboxed harness. The tools reach the engine over a host socket with the host's `by`, so a turn with `--provider microsandbox` and `--delegate` fails, and a sandboxed branch runs without the tools.
- A boundary through a server. Harnesses on the server still run as the server's user unless a sandbox provider is used, and a sandboxed turn gets no tools; the envelope stops honest mistakes there too.
- Any real harness delegating end to end. The projections were checked against Claude Code 2.1.283 and codex-cli 0.157.1 without model calls; the full loop was tested against the fake ACP agent only.
