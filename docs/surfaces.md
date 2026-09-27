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

`by --remote` prints the same JSON as `by` for each, and the same `{"error": {"kind", "message"}}` for refusals; tests compare them.

A harness in a rig fills a seat with `by spawn --seat`, `branchyard.spawn(seat=...)`, `Spawn::seat` or the MCP `spawn` tool's `seat`; every surface refuses a spawn without a seat in a rig, a seat outside one, and a seat its own seat does not delegate to, with the same `denied` error.

## Artifacts and scratch areas

See [storage](storage.md). Reads follow the delegation tree: a branch reads what it or its ancestors or descendants published or own; a sibling needs an explicit share. Not yet reached by `by --remote` or the server's HTTP API; local mode and delegation (`by`, Python, `Delegate`, MCP, inside or outside a harness with `--branch`) are built.

| Operation | SDK | by | delegation |
|---|---|---|---|
| Publish a file as an artifact | `Yard::publish_artifact`, `Branch::publish` | `artifact publish FILE [--name] [--label K=V]` | `publish_artifact` |
| List readable artifacts | `Yard::artifacts`, `Branch::artifacts` | `artifact list` | `list_artifacts` |
| Read an artifact's bytes | `Yard::read_artifact`, `Branch::read_artifact` | `artifact get ID --out PATH` | `get_artifact` |
| Share an artifact with another branch | `Yard::share_artifact` | `artifact share ID --to BRANCH` | `share_artifact` |
| Create a scratch area | `Yard::create_scratch` | `scratch create NAME` | `create_scratch` |
| List reachable scratch areas | `Yard::scratch_areas` | `scratch list` | `list_scratch` |
| Share a scratch area | `Yard::share_scratch` | `scratch share NAME --to BRANCH` | `share_scratch` |
| Acquire a scratch area's writer lock | `Yard::lock_scratch` | `scratch lock NAME` | `lock_scratch` |
| Release a scratch area's writer lock | `Yard::unlock_scratch` | `scratch unlock NAME` | `unlock_scratch` |
| A turn's authorized scratch areas | n/a: set automatically | `BRANCHYARD_SCRATCH_<NAME>` (local), a mount (Microsandbox); none (Substrate) | same |

## Storage

| Store | SDK | by | server |
|---|---|---|---|
| SQLite, `.branchyard/state.db` | `Yard::open` | yes | the default, with `DATA-DIR/state.db` for operations |
| PostgreSQL | `Yard::open_postgres`, `postgres` feature | no: local `by` opens `state.db` | `--database`, `postgres` feature |
| Artifact bytes | `.branchyard/artifacts/` | same | the server's data directory (planned; not yet served remotely) |

## Output types

Every result a surface returns is the SDK's serde form: `BranchInfo`, `RecordedEvent`, `Activity`, `Merged`, `HarnessInfo`, `Inspection`, `Spawned`, `Sent`, `Steer`, `EventPage`, `Children`, `Cancelled`, `Recovery`, `FeedPage`, `ArtifactRef`, `ScratchArea`, `ScratchLock`. `by --json` prints them through serde, except `by log --json`, whose flattened event shape is the CLI's own.

## Added with steering

| Surface | Before | Now |
|---|---|---|
| A running turn | refused `send` (`running`); only `cancel` reached it | `Branch::steer`, `by send --steer`, `POST …/steer`, `Repo::steer`, and the `steer` delegation tool deliver input into it through the harness's own mid-turn input, from any process; `not_running` without a turn, `unsupported` with the reason for a profile that cannot take it |
| `Activity` | no steering | `steered` (`id`, `by`, `text`) when the engine writes the input; the harness's `steer_accepted` or `steer_rejected` follows |
| `Error` kinds | no `not_running` | `not_running` (HTTP 409); `by send --steer --json` reports a refused steer as `steer_refused` with the `Steer` |

## Added with rigs

| Surface | Before | Now |
|---|---|---|
| `by rig check`, `by rig run` | none | a TOML spec lowered to one root branch and its seats ([rigs](rigs.md)) |
| `TaskOptions::seats`, `TaskRequest::seats` | none | the seats a rig's root may spawn; needs an envelope, and on a server `--allow-delegation` |
| `Spawn::seat`, `by spawn --seat`, `seat=`, the MCP `seat`, `SpawnRequest::seat` | none | fill a seat; required in a rig, refused outside one |
| `Inspection` | no seat | `seat` and `seats` for a branch in a rig; absent otherwise |

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
