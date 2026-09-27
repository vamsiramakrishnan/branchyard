# Surfaces

Every Branchyard operation means the same thing wherever it is reached. This page lists each operation and option against each surface, and says why wherever one is refused.

Surfaces:

- **SDK**: the Rust crate `branchyard` (`Yard`, `Branch`, `TaskBuilder`, `Delegate`), in-process.
- **by**: the `by` command on a local repository.
- **by --remote**: the `by` command against a server.
- **HTTP**: the server's API (`by serve`; see [the server reference](server.md)).
- **client**: `branchyard-client`, the typed Rust client of that API.
- **delegation**: a harness acting as its branch through `by` in its shell, the Python module, `Delegate` in Rust, or the MCP tools (`by mcp`). The four reach one set of operations and give the same answers ([delegation](delegation.md)); they work in local mode and on a server that allows delegation.

**yes** means supported; **no** gives the reason; a server opt-in is named where the server's operator must allow something first.

## Branch operations

| Operation | SDK | by | by --remote | HTTP | client | delegation |
|---|---|---|---|---|---|---|
| Run a task on one branch | `TaskBuilder::run` | `run` | yes | `POST …/tasks` | `submit_task` | `spawn` creates a child instead |
| Fan out to several harnesses | `run_on` | `fan` | yes | `harnesses` in the task | yes | no: a child has one harness; spawn one per harness |
| Check a [rig](rigs.md) and print its plan | no: the planner is the CLI's (`rig::plan`); build `Seats` yourself | `rig check` | the same, locally: nothing is sent | n/a | n/a | n/a |
| Run a rig's root seat | `TaskOptions::seats` with `delegation` | `rig run` | server opt-in `--allow-delegation` | `seats` in the task | yes | no: a child's seats come from its parent's rig |
| Continue a branch | `Branch::send` | `send` | yes | `POST …/send` | `send` | `send`, to a descendant, returning once its turn started |
| `send --json` (`Sent`) | the SDK returns the `Branch` | yes | yes | the operation's `branches` | yes | yes |
| Fork | `Branch::fork` | `fork` | yes | `POST …/fork` | `fork` | no: children start from a revision, not a session |
| Reincarnate ([lifecycle](lifecycle.md#reincarnation)) | `Branch::reincarnate` | `reincarnate` | yes | `POST …/reincarnate` | `reincarnate` | no: not a delegation operation; act as the branch's owner instead |
| List, show | `branches`, `branch` | `ls`, `show` | yes | yes | yes | `children`, `inspect` |
| Diff | `Branch::diff` | `diff` | yes | yes | yes | no: `inspect` reports the candidate |
| Event log | `events`, `events_since` | `log` | yes | `GET …/events?cursor` | `events` | `events` |
| Activity across branches from a cursor | `Yard::events_since`, `wait_for_events` | `watch` | `watch` | SSE stream | `stream` | no |
| Validated merge | `Yard::merge` | `merge` | yes | `POST …/merge` | `merge` | `integrate`, into the acting branch only |
| Remove | `Yard::remove` | `rm` | yes | `DELETE …` | `remove` | no |
| Cancel a turn and its subtree | `Yard::cancel` | `cancel` | yes, with the server's authority | `POST …/cancel` | `cancel` | `cancel`, descendants only |
| Steer a running turn ([harness support](harness-integration.md#steering-a-running-turn)) | `Branch::steer`, `Yard::steer_as`; `steer_state`, `wait_steer` follow it | `send --steer` | yes, as the server's caller | `POST …/steer` | `steer` | `steer` (`by send --steer`, `branchyard.steer`, `Delegate::steer`, MCP), descendants only |
| Harness profiles | `Yard::harnesses` | `harnesses` | yes, the server's `PATH` | `GET /v1/harnesses` | `harnesses` | no |
| Recover stopped turns | `Yard::recover`, `Yard::open` | on every open | the server, at start and every 30 s | n/a | n/a | n/a |

## Task options

| Option | SDK | by | by --remote | HTTP | client | delegation |
|---|---|---|---|---|---|---|
| Harness, name, base, budget, check | yes | yes | yes | yes | yes | yes, bounded by the envelope |
| Stall detection ([lifecycle](lifecycle.md#stall-detection)) | `Budget::stall_after`/`stall_action` | `--stall-after`, `--stall-action` | yes | `budget.stall_after_seconds`/`stall_action` | yes | the caller's own, not part of the envelope |
| Policy: allow, deny, rules | `Policy` | `--yes`, default deny | yes | `policy` | yes | the parent's, narrowed by `deny` |
| Policy: ask | `Policy::ask` | `--ask` | no: the harness runs on the server, which has no terminal | no, same | no, same | no: a child inherits its parent's policy |
| `isolated` | yes | `--isolated` | yes | yes | yes | inherited |
| `command` | yes | `--command` | server opt-in `--allow-client-commands` | same | same | inherited for the same harness |
| Provider: Microsandbox, Substrate | `TaskOptions::provider` | `--provider` | server opt-in `--allow-provider`; `pass_env` and the key are the server's | `provider` | yes | inherited; a sandboxed turn gets no tools |
| Delegation envelope | `TaskOptions::delegation` | `--delegate` | server opt-in `--allow-delegation` | `delegation` | yes | a child gets a narrower one |
| Rig seats ([rigs](rigs.md)) | `TaskOptions::seats` | `rig run` | server opt-in `--allow-delegation`; each seat's provisioning under the same rules as provisioning below | `seats`, on task requests only | yes | a child gets the seats below its own |
| Allow delegation commands | `Policy::allow_delegation_commands` | `--allow-delegation` | server opt-in `--allow-delegation` | `allow_delegation` | yes | inherited |
| Unapproved tools | `TaskOptions::unapproved_tools` | `--allow-unapproved-tools` | server opt-in `--allow-unapproved-tools` | `unapproved_tools` | yes | inherited |
| Provisioning: secrets ([provisioning](provisioning.md)) | `Provisioning::secrets`, with `isolated` or a sandbox provider | `--secret NAME[=VAR\|=@FILE]`, read from your environment or files | `--secret NAME` only: the server reads each from the source its operator defined with `by serve --secret`; a named source is refused | `provision.secrets`, names only; `403 secret_not_allowed` for one the server does not define | yes | inherited, with the parent's sources |
| Provisioning: MCP servers | `Provisioning::mcp_servers` | `--mcp NAME=COMMAND` | server opt-in `--allow-client-commands`: the server runs them | `provision.mcp_servers`, same opt-in | yes | inherited |
| Provisioning: instructions, model, effort, telemetry, auth method | `Provisioning` | `--instructions FILE`, `--model`, `--effort`, `--telemetry URL\|off`, `--auth` | yes; `by` reads the instructions file and sends its text | `provision` | yes | inherited |

## Delegation operations as a person

A person acts with their own authority, or the server's, bounded by the branch's envelope exactly as a harness is.

| Operation | SDK | by | by --remote | HTTP | client |
|---|---|---|---|---|---|
| Spawn a child of a branch | `Branch::delegate`, then `Delegate::spawn` | `spawn --parent` | yes; server opt-in `--allow-delegation` | `POST …/spawn` | `spawn` |
| Fill a rig seat | `Spawn::seat` | `spawn --parent B --seat S` | yes | `seat` in the spawn | yes |
| Inspect | `Delegate::inspect` | `inspect` | yes | `GET …/inspection` | `inspect` |
| Events page from a cursor | `Delegate::events` | `events` | yes | `GET …/event-page` | `event_page` |
| Integrate a child into its parent | `Delegate::integrate` | `integrate` | yes | `POST …/integrate` | `integrate` |
| Children | `Branch::descendants`, `Delegate::children` | `children` | yes | `GET …/children` | `children` |
| A branch's graph: children, dependencies, revision ([task graphs](graph.md)) | `Yard::graph`, `Delegate::graph` | `graph show` | yes | `GET …/graph` | `graph` |
| Apply a graph proposal, all or nothing | `Delegate::apply_graph` | `graph apply --parent` | yes; server opt-in `--allow-delegation` for spawns | `POST …/graph`; `409 stale_revision` | `apply_graph` |
| Spawn a child that waits for siblings | `Spawn::depends_on`, `after` | `spawn --depends-on [--after integrated]` | yes | `depends_on`, `after` in the spawn | yes |
| Bind a child to scratch areas | `Spawn::bindings` | `spawn --bind NAME:ACCESS` | yes | `bindings` in the spawn | yes |
| Start dependents no engine started | `Yard::resume_graph` | `graph resume` | no: the server does it every 30 s | n/a | n/a |
| Ask the branch's parent a question, optionally waiting for an answer | `Delegate::ask` | `ask "<text>" [--wait SECS]` | yes; capped at 120s | `POST …/ask` | `ask` |
| Report to the branch's parent | `Delegate::report` | `report "<text>"` | yes | `POST …/report` | `report` |
| Escalate to the parent, or further up if a rig seat allows | `Delegate::escalate` | `escalate "<text>"` | yes | `POST …/escalate` | `escalate` |
| Answer a descendant's message | `Delegate::answer` | `answer <message-id> "<text>"` | yes | `POST …/answer` | `answer` |
| The branch's own inbox | `Delegate::inbox` | `inbox [--unread]` | yes | `GET …/inbox` | `inbox` |

`by --remote` prints the same JSON as `by` for each, and the same `{"error": {"kind", "message"}}` for refusals; tests compare them. Outside a harness every messaging command needs `--as <branch>`, since there is no other way to say who is asking; see [delegation](delegation.md#inbox). A message to a branch with a running turn is steered into it on every surface (`SteerDelivery`, the default `DeliveryHook` of every `Yard`), and otherwise delivered at its next turn's start; `Activity::MessagesDelivered` (`messages_delivered` in `by log --json`) records which ([delegation](delegation.md#delivery)).

A harness reaches the same graph operations: `by graph`, `branchyard.graph`/`apply_graph`, `Delegate::apply_graph` and the MCP `apply_graph` and `graph` tools, with the same `stale_revision`, `denied` and other refusals ([task graphs](graph.md#surfaces)).

A harness in a rig fills a seat with `by spawn --seat`, `branchyard.spawn(seat=...)`, `Spawn::seat` or the MCP `spawn` tool's `seat`; every surface refuses a spawn without a seat in a rig, a seat outside one, and a seat its own seat does not delegate to, with the same `denied` error.

## Artifacts and scratch areas

See [storage](storage.md). Reads follow the delegation tree: a branch reads what it or its ancestors or descendants published or own; a sibling needs an explicit share. Every surface reaches them now: local mode, delegation (`by`, Python, `Delegate`, MCP, inside or outside a harness with `--branch`), `by --remote` (always `--branch`, since there is no harness to delegate as), the server's HTTP API and `branchyard-client`.

| Operation | SDK | by | by --remote | HTTP | client | delegation |
|---|---|---|---|---|---|---|
| Publish a file as an artifact | `Yard::publish_artifact`, `Branch::publish` | `artifact publish FILE [--name] [--label K=V]` | yes, `--branch` required | `POST …/artifacts` | `Repo::publish_artifact` | `publish_artifact` |
| List readable artifacts | `Yard::artifacts`, `Branch::artifacts` | `artifact list` | yes | `GET …/artifacts` | `Repo::artifacts` | `list_artifacts` |
| Read an artifact's bytes | `Yard::read_artifact`, `Branch::read_artifact` | `artifact get ID --out PATH` | yes | `GET …/artifacts/{id}` (metadata), `.../content` (bytes) | `Repo::read_artifact` | `get_artifact` |
| Share an artifact with another branch | `Yard::share_artifact` | `artifact share ID --to BRANCH` | yes | `POST …/artifacts/{id}/share` | `Repo::share_artifact` | `share_artifact` |
| Create a scratch area | `Yard::create_scratch` | `scratch create NAME` | yes | `POST …/scratch` | `Repo::create_scratch` | `create_scratch` |
| List reachable scratch areas | `Yard::scratch_areas` | `scratch list` | yes | `GET …/scratch` | `Repo::scratch_areas` | `list_scratch` |
| Share a scratch area | `Yard::share_scratch` | `scratch share NAME --to BRANCH` | yes | `POST …/scratch/{name}/share` | `Repo::share_scratch` | `share_scratch` |
| Acquire a scratch area's writer lock | `Yard::lock_scratch` | `scratch lock NAME` | yes | `POST …/scratch/{name}/lock` | `Repo::lock_scratch` | `lock_scratch` |
| Release a scratch area's writer lock | `Yard::unlock_scratch` | `scratch unlock NAME` | yes | `POST …/scratch/{name}/unlock` | `Repo::unlock_scratch` | `unlock_scratch` |
| A scratch area's lock state | `Yard::scratch_lock_state` | n/a | n/a | `GET /v1/repos/{repo}/scratch/{name}/lock` | `Repo::scratch_lock_state` | n/a |
| A turn's authorized scratch areas | n/a: set automatically | `BRANCHYARD_SCRATCH_<NAME>` (local), a mount (Microsandbox); none (Substrate) | n/a: the server has no turn's environment to expose to a remote caller | n/a | n/a | same |

A remote caller reaches the scratch area's directory only through `lock`/`unlock` and whatever harnesses the server runs; there is no route that reads or writes its files directly (see `docs/storage.md`).

## Storage

| Store | SDK | by | server |
|---|---|---|---|
| SQLite, `.branchyard/state.db` | `Yard::open` | yes | the default, with `DATA-DIR/state.db` for operations |
| PostgreSQL | `Yard::open_postgres`, `postgres` feature | no: local `by` opens `state.db` | `--database`, `postgres` feature |
| Artifact bytes | `.branchyard/artifacts/` | same | each served repository's own `.branchyard/artifacts/`; served remotely (see [storage](storage.md#remote-mode)) |

## Output types

Every result a surface returns is the SDK's serde form: `BranchInfo`, `RecordedEvent`, `Activity`, `Merged`, `HarnessInfo`, `Inspection`, `Spawned`, `Sent`, `Steer`, `EventPage`, `Children`, `Cancelled`, `Recovery`, `FeedPage`, `ArtifactRef`, `ScratchArea`, `ScratchLock`, `Graph`, `GraphApplied`. `by --json` prints them through serde, except `by log --json`, whose flattened event shape is the CLI's own.

## Added with steering

| Surface | Before | Now |
|---|---|---|
| A running turn | refused `send` (`running`); only `cancel` reached it | `Branch::steer`, `by send --steer`, `POST …/steer`, `Repo::steer`, and the `steer` delegation tool deliver input into it through the harness's own mid-turn input, from any process; `not_running` without a turn, `unsupported` with the reason for a profile that cannot take it |
| `Activity` | no steering | `steered` (`id`, `by`, `text`) when the engine writes the input; the harness's `steer_accepted` or `steer_rejected` follows |
| `Error` kinds | no `not_running` | `not_running` (HTTP 409); `by send --steer --json` reports a refused steer as `steer_refused` with the `Steer` |

## Added with task graphs

| Surface | Before | Now |
|---|---|---|
| `BranchStatus` | no state for a child that has not started | `waiting` (prerequisites not settled) and `blocked` (`reason`: a prerequisite failed, was interrupted, stopped at a limit, is blocked or was removed) ([task graphs](graph.md#dependencies)) |
| `Delegate::apply_graph`, `Delegate::graph`, `Yard::graph`, `Yard::resume_graph`, `by graph show/apply/resume`, `branchyard.apply_graph`/`graph`, the MCP `apply_graph`/`graph` tools, `GET`/`POST …/graph`, `Repo::graph`/`apply_graph` | none | atomic graph proposals against the parent's graph revision |
| `Spawn`, `by spawn`, `branchyard.spawn`, the MCP `spawn`, `SpawnRequest` | no dependencies | `depends_on`, `after`, `bindings` |
| `Inspection`, `Spawned` | no graph | `graph_revision`, `depends_on`, `bindings` (`Spawned`: `depends_on`), omitted when empty |
| `Seat` (rig `seats.NAME.bindings`) | none | scratch-area bindings for every child in the seat |
| `Error` kinds | no `stale_revision` | `stale_revision` (HTTP 409, `detail` `{expected, actual}`) |
| `Policy::allow_delegation_commands` | seven subcommands | eight, with `graph` |

## Added with rigs

| Surface | Before | Now |
|---|---|---|
| `by rig check`, `by rig run` | none | a TOML spec lowered to one root branch and its seats ([rigs](rigs.md)) |
| `TaskOptions::seats`, `TaskRequest::seats` | none | the seats a rig's root may spawn; needs an envelope, and on a server `--allow-delegation` |
| `Spawn::seat`, `by spawn --seat`, `seat=`, the MCP `seat`, `SpawnRequest::seat` | none | fill a seat; required in a rig, refused outside one |
| `Inspection` | no seat | `seat` and `seats` for a branch in a rig; absent otherwise |

## Added with lifecycle features

| Surface | Before | Now |
|---|---|---|
| `Budget::stall_after`, `Budget::stall_action`, `BranchInfo::stalled`, `Inspection::stalled`, `Activity::Stalled`/`Resumed` | none | stall detection ([lifecycle](lifecycle.md#stall-detection)); `stalled` is serde-default so JSON stays compatible |
| `Branch::reincarnate`, `by reincarnate`, `POST …/reincarnate`, `client::reincarnate`, `BranchInfo::superseded_by` | none | reincarnation ([lifecycle](lifecycle.md#reincarnation)); `superseded_by` is serde-default |
| `by serve --webhook`/`--webhook-secret`/`--webhook-events`/`--webhook-insecure`, `webhooks` in the JSON config | none | operator-configured webhook notifications ([server](server.md#webhooks)); a server-only addition, no SDK, CLI-local or delegation surface |

## Added with the inbox

| Surface | Before | Now |
|---|---|---|
| `Delegate::{ask, report, escalate, answer, inbox, send_and_wait}`, `Branch::send_and_wait`, `by ask/report/escalate/answer/inbox`, `by send --wait`, the MCP tools, `POST …/{ask,report,escalate,answer}`, `GET …/inbox`, the client and Python functions | none | parent/descendant messages ([delegation](delegation.md#inbox)) |
| `Activity` | no messages | `message` (the `Message`) on the sender's and recipient's logs; `messages_delivered` (`ids`, `via`: `{"path": "steer", "steer"}` or `{"path": "turn_start"}`) on the recipient's |
| `DeliveryHook`, `Yard::set_delivery_hook`/`clear_delivery_hook`, `SteerDelivery` | none | every `Yard` steers a message into its recipient's running turn by default ([delegation](delegation.md#delivery)) |
| Stall detection | children and permission answers excluded | a turn blocked in `ask --wait` is excluded too ([lifecycle](lifecycle.md#stall-detection)) |

## Changed from 4609ca1

| Gap | Before | Now |
|---|---|---|
| `by --remote --provider`, `--delegate`, `--allow-delegation`, `--allow-unapproved-tools` | refused by the CLI | sent in the request; the server's operator opts in |
| `by --remote spawn`, `inspect`, `events`, `integrate`, `children`, `send --json` | refused: "does not run with --remote yet" | server endpoints, the same JSON as local |
| Providers on the server | local only | `--allow-provider microsandbox,substrate` |
| Delegation for the server's harnesses | none | `--allow-delegation`, `--by-path` |
| Unapproved tools on the server | refused | `--allow-unapproved-tools` |
| PostgreSQL | design only | `Yard::open_postgres`, `by serve --database` |
| `HarnessInfo` | not `Serialize`; copied field by field | serde |
| `by --remote artifact`/`scratch`, the server's HTTP API, `branchyard-client` for storage | refused: "does not yet reach a server over --remote" | server endpoints, the same JSON as local; `--max-artifact-bytes` |
