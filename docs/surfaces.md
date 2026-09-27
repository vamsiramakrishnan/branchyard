# Surfaces

Every Branchyard operation should mean the same thing wherever it is reached. This table lists each operation against each surface and says whether it is supported there, or refused and why.

> **Status.** This is the state at commit `4609ca1`, before the parity work. It is updated as the gaps close.

Surfaces:

- **SDK**: the Rust crate `branchyard` (`Yard`, `Branch`, `TaskBuilder`), in-process.
- **by**: the `by` command on a local repository.
- **by --remote**: the `by` command against a server.
- **HTTP**: the server's API (`by serve`, [server](server.md)).
- **client**: `branchyard-client`, the typed Rust client of that API.
- **delegation**: a harness acting as its branch through `by` in its shell, the Python module, `Delegate` in Rust, or `by mcp` ([delegation](delegation.md)).

Legend: **yes**; **no** with the reason; **n/a** where the operation does not belong on the surface.

## Branch operations

| Operation | SDK | by | by --remote | HTTP | client | delegation |
|---|---|---|---|---|---|---|
| Run a task on one branch | `TaskBuilder::run` | `run` | yes | `POST …/tasks` | `submit_task` | `spawn` (a child) |
| Fan out one task to several harnesses | `run_on` | `fan` | yes | `harnesses` in the task | yes | no: a child has one harness |
| Continue a branch | `Branch::send` | `send` | yes | `POST …/send` | `send` | `send` (starts a descendant's turn) |
| `send --json` (`Sent`) | n/a | yes | no: refused, "does not run with --remote yet" | no endpoint returns `Sent` | no | yes |
| Fork | `Branch::fork` | `fork` | yes | `POST …/fork` | `fork` | no |
| List, show | `branches`, `branch` | `ls`, `show` | yes | yes | yes | `children`, `inspect` |
| Diff | `Branch::diff` | `diff` | yes | yes | yes | no |
| Event log | `events`, `events_since` | `log` | yes | `GET …/events?cursor` | `events` | `events` |
| Activity across branches from a cursor | `Yard::events_since`, `wait_for_events` | `watch` | `watch` | SSE stream | `stream` | no |
| Validated merge | `Yard::merge` | `merge` | yes | `POST …/merge` | `merge` | `integrate` (into the parent only) |
| Remove | `Yard::remove` | `rm` | yes | `DELETE …` | `remove` | no |
| Cancel a turn and its subtree | `Yard::cancel` | `cancel` | yes (server's authority) | `POST …/cancel` | `cancel` | `cancel` (descendants only) |
| Harness profiles | `Yard::harnesses` | `harnesses` | yes (server's `PATH`) | `GET /v1/harnesses` | `harnesses` | no |
| Recover stopped turns | `Yard::recover`, `Yard::open` | on every open | the server, every 30 s | n/a | n/a | n/a |

## Task options

| Option | SDK | by | by --remote | HTTP | client | delegation |
|---|---|---|---|---|---|---|
| Harness, name, base, budget, check | yes | yes | yes | yes | yes | yes, bounded by the envelope |
| Policy: allow, deny, rules | `Policy` | `--yes`, default deny | yes | `policy` | yes | the parent's, narrowed by `deny` |
| Policy: ask | `Policy::ask` | `--ask` | no: the server has no terminal | no | no | no |
| `isolated` | yes | `--isolated` | yes | yes | yes | inherited |
| `command` | yes | `--command` | only if the server allows client commands | same | same | inherited |
| Provider (Microsandbox, Substrate) | `TaskOptions::provider` | `--provider` | no: refused, "the server runs harnesses locally" | no field | no | no: a sandboxed turn gets no tools |
| Delegation envelope | `TaskOptions::delegation` | `--delegate` | no: refused | no field; the server offers no tools | no | children get a narrower one |
| Allow delegation commands | `Policy::allow_delegation_commands` | `--allow-delegation` | no: refused | no | no | inherited |
| Unapproved tools | `TaskOptions::unapproved_tools` | `--allow-unapproved-tools` | no: refused | no field | no | inherited |

## Delegation operations as a person

| Operation | SDK | by | by --remote | HTTP | client |
|---|---|---|---|---|---|
| Spawn a child of a branch | `Branch::delegate` then `Delegate::spawn` | `spawn --parent` | no: "does not run with --remote yet" | no | no |
| Inspect | `Delegate::inspect` | `inspect` | no | no | no |
| Events page with cursor | `Delegate::events` | `events` | no | no (`GET …/events` returns a different shape) | no |
| Integrate a child into its parent | `Delegate::integrate` | `integrate` | no | no | no |
| Children | `Branch::descendants`, `Delegate::children` | `children` | no | no | no |

## Storage

| Store | SDK | by | server |
|---|---|---|---|
| SQLite, `.branchyard/state.db` | yes | yes | yes, and `DATA-DIR/state.db` for operations |
| PostgreSQL | no: designed in [durability](durability.md#postgresql), not built | no | no |

## Output types

| Result | Serialize | Notes |
|---|---|---|
| `BranchInfo`, `RecordedEvent`, `Activity`, `Merged`, `Inspection`, `Spawned`, `Sent`, `EventPage`, `Children`, `Cancelled`, `Recovery`, `FeedPage` | yes | |
| `HarnessInfo` | no | the client and the CLI copy it field by field (`HarnessEntry`, `json::harness`) |
| `BranchInfo` in `by --json` | copied field by field in `json.rs` | the same shape as its serde form |
