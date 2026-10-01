# Surfaces

Every Branchyard operation means the same thing wherever it is reached. This page lists each operation and option against each surface, and says why wherever one is refused.

Surfaces:

- **SDK**: the Rust crate `branchyard` (`Yard`, `Branch`, `TaskBuilder`, `Delegate`), in-process.
- **by**: the `by` command on a local repository.
- **by --remote**: the `by` command against a server: `--remote URL` (or `BRANCHYARD_REMOTE`), with `--token-file`, `--repo` and `--ca-file` (or their `BRANCHYARD_*` variables), before or after the command.
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
| List checkpoints ([checkpoints](checkpoints.md)) | `Branch::checkpoints`; `recorded_checkpoints` from events | `show`, `log` | yes, from the event log | the `checkpoint` events | `events` | no: `events` shows them |
| Fork at a checkpoint | `Branch::fork_at` | `fork --at N` | no: needs an API operation; refused with a message | no | no | no: children start from a revision |
| Rewind to a checkpoint | `Branch::rewind` | `rewind --to N` | no: needs an API operation; refused with a message | no | no | no: not a delegation operation |
| Try a branch in this checkout | `Yard::try_on`, `try_off`, `try_status`, `try_recover` | `try`, `try --off`, `try --status` | no: the server's checkout is not yours; refused | no | no | no |
| Compare attempts | `Yard::compare`, `fan_branches`, `diff_between`; `compare_attempt`, `mark_unique`, `diff_files` | `compare` | yes, but not `--check` or `--diff` | from branches, events and diffs | the same | no |

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
| Start dependents no engine started | `Yard::resume_graph` | `graph resume` | no: every server and `by worker` does it every 30 s, one claim winning | n/a | n/a |
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

## Added with tenants

A server-only addition: identity, scopes and quotas are meaningful only where more than one credential can reach the same process, so there is no SDK, local-CLI or delegation-surface counterpart.

| Surface | Before | Now |
|---|---|---|
| `tokens` entries, `credentials`, `tenants` in the JSON config; `principal`, `tenant`, `scopes`/`repos` fields | one flat list of equal, all-powerful bearer tokens | each credential names a principal: a tenant, a subject, scopes (`read`/`run`/`merge`/`admin`) and an optional repository allowlist ([server](server.md#identity-and-scopes)). A `tokens` entry that gives none of these is unchanged: every scope, every repository, the `default` tenant |
| `branchyard-server token new` / `by serve token new` | none | generates a token and its hashed `credentials` entry; the plaintext is never stored |
| `403 scope_required`, `403 repo_not_allowed`, `429 quota_exceeded` | not distinct from `denied`/`unauthorized` | every endpoint checks the caller's scope and repository allowlist; quotas (`max_running`, `max_branches`, `max_cost_usd`, `max_artifact_bytes`) are enforced per tenant, `max_running` and `max_branches` in the admission's transaction, so across servers sharing a database and across restarts ([server](server.md#quotas)) |
| `GET /v1/operations/{id}` on another tenant's operation | reachable by any token | `404 unknown_operation`, indistinguishable from an ID that never existed |
| `GET /v1/operations?idempotency_key=`, `Idempotency-Key` | scoped to the token name | scoped to the principal within its tenant; another tenant's operation is `404 unknown_operation` |
| `by worker` | needed a token like a server | needs no `tokens` or `credentials`: it runs each operation as the principal recorded when it was admitted |

## Changed with the clap command line

Every `by` command, flag and JSON output is unchanged; the parsing around them is stricter and the help is generated. See the [README](../README.md#command-line).

| Surface | Before | Now |
|---|---|---|
| Global options (`--remote`, `--token-file`, `--repo`, `--ca-file`) | before the command only | before or after it; variables read by clap's `env`, blank ones still unset |
| `graph`, `rig`, `artifact`, `scratch` actions | one flag set shared by every action | real subcommands, each with only its own flags (`artifact list --out` is refused); `--json`, and `--branch` for `artifact` and `scratch`, may still come before the action, as the Python module passes them |
| Usage errors | `by: MESSAGE` and `Try 'by help CMD'.`, exit 2 | clap's `error: MESSAGE`, a `tip:` where one helps (a similar command or flag, quoting a prompt), the command's usage line, exit 2 |
| `--help` | anywhere on the command line, it won over any other mistake | it wins over missing arguments and everything after it; an unknown flag before it is reported instead |
| `--check`, `--command` and `PAGER` splitting | own POSIX-like splitter | `shlex`, which also drops a `#` comment starting a word |
| New | none | `by completions <bash\|zsh\|fish\|powershell\|elvish>`, `by man`, short flags, durations with units, `$` in `--budget-usd` |

## Added with setup

Setup and configuration are the CLI's: they write files for `by` and the server to read, so there is no SDK, HTTP or delegation counterpart. A harness sets Branchyard up through `by init --json` like any other `by` command ([setup](setup.md)).

| Surface | Before | Now |
|---|---|---|
| `by init [TOPIC]` | none: server JSON, rig TOML, token files and secrets tables were written by hand | a terminal wizard (`cliclack`) over one interview per topic: `project`, `server`, `rig`, `deploy`, `plugin`; refuses without a terminal |
| `by init TOPIC --json --next`, `--dry-run`, `--apply [--force]` | none | the same interview as a JSON protocol (`schema/setup.protocol.json`): batches of at most four questions, a plan of files with diffs and validator verdicts, writes only valid plans and replaces a differing file only with `--force`; clap subcommand flags, so two steps at once, `--force` without `--apply`, `--answers` without a step or a step without a topic is a usage error (exit 2), and the topic completes in every shell |
| `branchyard.toml`, `~/.config/branchyard/config.toml` | none: every default came from flags and `BRANCHYARD_*` variables | defaults under flags and variables (`schema/branchyard.config.json`): `run` and `fan` take every default, `send`, `fork`, `reincarnate` and `spawn` only `permissions`, `serve` its `--config`, remote commands `[remote]`; not read inside a harness's branch |
| `by config show`, `path`, `validate`, `schema` | none | the effective configuration with each value's source, where the files are, a strict check, the schema; clap subcommands, `--json` before or after the action |
| `by serve --check`, `branchyard-server --check` | none | load and validate a configuration as serving would, without serving or writing anything; a flag of the server's clap parser, which `by serve` forwards like the rest; `branchyard.toml`'s `[serve] config` is added as `--config` only when the server's parser reports no `--config`/`-c` on the command line |
| The `setup` skill and `/branchyard:setup` | the plugin had the `delegate` skill only | `plugins/branchyard/skills/setup` drives `by init --json` with the harness's own question tool; the Claude plugin's `commands/setup.md` starts it; `install_skill.py --skill setup`; `by init plugin` installs both skills |

## Added with checkpoints

| Surface | Before | Now |
|---|---|---|
| `Activity::Checkpoint`, `Rewound`, `ForkedAt`; `Checkpoint`, `SessionContinuity` (`schema/contract.json`) | none | a checkpoint per turn and how a rewound or forked-at branch's session continues ([checkpoints](checkpoints.md)); new variants of the event enum, so a strict reader of older events is unaffected and one of newer events must accept them |
| `by show --json` | the branch | also `checkpoints` (`current`, `base`, the list) |
| `by fork --at N` | none | fork from checkpoint N; conflicts with `--fresh-session` |
| `by rewind`, `by try`, `by compare` | none | new commands in *Work on branches* |
| `Repository::check_commit`, `CheckResult` (`branchyard-workspace`) | none | a check on an exact commit in a private worktree |
| `refs/branchyard/<branch>/<incarnation>/turn-N`, `refs/branchyard-try/*`, `.branchyard/try/` | none | checkpoint refs (deleted by `rm`), and a try's pins and saved state |

## Added with pull requests

GitHub is reached through the user's `gh`, which runs where the repository and its git remote are ([pull requests](pull-requests.md)). The SDK records and folds the steps; the CLI talks to `gh`.

| Operation | SDK | by | by --remote | HTTP | client | delegation |
|---|---|---|---|---|---|---|
| Start from an issue | no: pass the prompt; `slug` names the branch | `run`, `fan`, `spawn` `--issue URL\|#N\|N` | yes: `gh` runs on the client, the server gets the prompt (the link is its header; no `issue_linked` event) | n/a | n/a | `spawn --issue`, with the harness's own `gh` login; the link is the prompt's header |
| Check the candidate alone | `Branch::verify_candidate` | inside `pr` | no: `pr` is local only | no | no | no |
| Push the candidate | `Branch::push_candidate` | inside `pr` | no: the server's repository has its own remote and credentials; not built | no | no | no |
| Open or update a pull request | no: the CLI's, through `gh` | `pr` | no, `unsupported` | no | no | no: `by pr` refuses inside a harness, since it pushes with the user's credentials |
| Follow it, feeding CI and reviews back | no | `pr --watch` | no, `unsupported` | no | no | no |
| Merge readiness | `Activity::PullRequest` events; fold them | `show`, `show --json` (`merge_readiness`) | yes, from the branch's events (a server's branches have none yet) | the events | the events | `events` shows them |
| Observe now | no | `show --refresh` | no, `unsupported` | no | no | no |
| Open the worktree in an editor | `BranchInfo::worktree` | `open` | no, `unsupported`: the worktree is on the server | n/a | n/a | n/a |

| Surface | Before | Now |
|---|---|---|
| `Activity` | no pull-request steps | `pull_request` (`PullRequestActivity`, tagged by `kind`: `issue_linked`, `checked`, `pushed`, `opened`, `updated`, `observed`, `feedback_delivered`, `feedback_undelivered`, `watch_stopped`); `schema/contract.json` regenerated |
| `by show --json` | the branch | the branch and `merge_readiness` (`null` without pull-request steps) |
| `by run`, `fan`, `spawn` | a prompt was required | optional with `--issue`; `Usage: by run [OPTIONS] [PROMPT]` |
| `Repository::verify`, `Repository::push` (`branchyard-workspace`) | none | a check on one commit in a temporary worktree; a push of one commit to a remote branch, without hooks or a terminal prompt |

## Added with the cockpit, and at integration

The cockpit branch made `by watch` act on the selected branch; integrating it with the checkpoints and pull-request branches bound the keys it had reserved. Each key runs the `by` command named, with the dashboard's global flags, so remote mode is the command's own.

| Key in `by watch` | Runs | by --remote |
|---|---|---|
| `s`, `S`, `R`, `x`, `m`, `f` | `send`, `send --steer`, `send` (resume), `cancel`, `merge`, `fork` | yes |
| `d`, `l`, `y` | `diff` and `log` panes, copy the name | yes |
| `Y` | copy the worktree's path | no: the worktree is on the server |
| `p`, `P` | `pr` (waited for, output in a pane), `pr --watch` (in the background), each after a yes | no: pushes from this machine with your `gh` login |
| `o` | `open`, in-process through `open::plan`/`launch`; a terminal editor gets the screen until it exits | no: the worktree is on the server |
| `r` | `rewind --to N --yes`, N typed under the checkpoint list, then a yes | no: needs an API operation |
| `c` | `compare` of the branch and its siblings, in a pane | yes, from records, events and diffs |
| `t` | `try` after a yes; on the tried branch, `try --off` | no: changes this machine's checkout |

| Surface | Before | Now |
|---|---|---|
| `by watch`'s detail pane | status, cost, inbox, children, prompt, events | also the checkpoint the branch is at, its merge readiness once it has pull-request steps, and whether it is being tried |
| `--no-notify`, `[notify]` in `branchyard.toml` | none | notices from `by watch` and waiting commands ([README](../README.md#watching-branches)); `schema/branchyard.config.json` regenerated |
| `--log-format` (server, `by serve`, `by worker`, `branchyard-herdr`) | human-readable only | `pretty` (the default) or `json`, one object per line |
| `branchyard_workspace::git`, `Git` | private | public: the one place that starts `git` on the host; `Git::stdin` and `Git::run_bytes` added at integration for `by try`'s patches and blobs, and `by pr` uses it too |
| `Yard::try_recorded` | none | the recorded try, read without the try lock (what `by watch` polls) |
| `branchyard::AttemptCheck` | `branchyard::CheckRun` (from checkpoints) | renamed at integration: the pull-request branch's `CheckRun` (a check on one commit, in `schema/contract.json`) keeps the name |
| `by serve`/`branchyard-server` shutdown | connections drained for up to 10 s, then the grace period | both at once, counted from SIGTERM or SIGINT, so the process exits within `--shutdown-grace` (at least 1 s for requests in flight); signal handlers installed before startup ([server](server.md#running-it)) |
| `deploy/compose.yaml`, `by init deploy`'s compose | Docker's 10 s stop timeout | `stop_grace_period: 75s`, above the default 60 s grace |

## Added with the workspace lifecycle

A repository's scripts are a trust decision, so each surface takes it where its owner can: a person through `by`, an operator in the server's configuration, SDK code by passing them. See [workspace](workspace.md).

| Surface | Before | Now |
|---|---|---|
| `[workspace]` in `branchyard.toml`, `[projects."<root>".workspace]` in the user file | none | `copy`, `setup`, `run.NAME`, `teardown` (`schema/branchyard.config.json`); `by run`, `fan`, `fork`, `reincarnate` and `rig run` apply it once trusted; `send` and a harness's `by` never read it |
| `TaskOptions::workspace`, `WorkspaceSpec` | none | copy and setup before a new branch's first turn, teardown at removal; stored with the branch and inherited by forks, reincarnations and delegated children without one |
| `BRANCHYARD_WORKTREE`, `BRANCHYARD_PORT` | none | given to a local harness on every turn, and to workspace scripts with `BRANCHYARD_BRANCH` and `BRANCHYARD_ROOT`; the port is reserved in the store per branch |
| `Activity::Workspace` (`workspace` in `by log --json`) | none | each copy, setup, run and teardown, with its commands, exit code and output tail |
| `by workspace show [BRANCH]`, `trust`, `untrust`, `run [BRANCH] [NAME] [--detach]` | none | local only; refused with `--remote` |
| `Yard::workspace`, `workspace_env`, `remove_reporting`, `record_workspace`, `deny_workspace_scripts` | none | a branch's workspace and port, the variables for running in its worktree, removal with its teardown's report |
| `by merge --rm` | none | merge, then remove as `by rm` does (teardown included); with `--remote` too |
| `allow_workspace_scripts` in the server's JSON config | none | `true` or served repository names; the server reads those repositories' `[workspace]` itself, never a request's (a `workspace` field is an unknown field, `400`), and refuses every other repository's scripts |

## Added with sandbox snapshots

A sandboxed branch can keep its sandbox between turns and branch new sandboxes from provider snapshots under its checkpoints; see [sandbox snapshots](sandbox-snapshots.md). Unqualified on every provider.

| Surface | Before | Now |
|---|---|---|
| `SandboxOptions::keep`, `snapshots`, `max_paused`, `live_branch`; `SubstrateOptions::keep`, `snapshots`, `max_paused` | none | `--keep-sandbox pause\|destroy`, `--sandbox-snapshots N`, `--max-paused N` (with `--provider microsandbox` or `substrate`), `--live-branch` (Microsandbox); `keep`, `snapshots`, `max_paused`, `live_branch` in `[microsandbox]`; in the `provider` object over HTTP and `by --remote`, under the server's `--allow-provider`; inherited by forks and delegated children with the provider |
| `Activity::Sandbox` (`SandboxEvent`, `SandboxOrigin`) | none | each turn's sandbox and where it came from (fresh with a reason, resumed, branched from a checkpoint, a fan's prepared one), kept, not kept, evicted, a snapshot released or not taken; `sandbox` in `by log --json`, a `sandbox:` line in `by`'s output, and in every event stream |
| `Checkpoint::sandbox` (`SandboxSnapshot`) | none | the provider snapshot taken with a checkpoint: provider, handle, scope, consistency, method; in `schema/contract.json` |
| `WorkspaceReport::ran_in`, `inherited_from`; `RanIn` | none | where setup or teardown ran (host or sandbox), and the branch whose setup a branched sandbox inherited |
| `by fork --at N`, `Branch::fork`, `fork_at`, `rewind`, a delegated child, a rig seat, a graph dependent, `by fan` | a fresh sandbox, git and setup | the matching provider snapshot first, then the fresh path, saying which; `by fan` runs setup once when its provider can live-branch |
| `Yard::use_sandbox_provider` | none | SDK only: run Microsandbox-provider branches through a given `SandboxProvider` (tests, embedding) |
| `SandboxProvider::pause`, `resume`, `branch_live`, `release_checkpoint`; `Capabilities::pause`, `live_branch`, `has`; `Feature`, `LIVE_BRANCH`, `PAUSE`, `FULL_SNAPSHOT`; `SandboxSpec::persist`; `SandboxState::Paused`; `branchyard_sandbox::fake` | none | the provider contract's sandbox-level branching, defaults `Unsupported` |
| `sandboxes` table (SQLite), `by_sandboxes` (PostgreSQL) | none | kept sandboxes and snapshots per branch, deleted with it ([durability](durability.md)) |

## Added with prepared environments and worker labels

See [prepared environments](environments.md) and [worker labels](server.md#worker-labels). Sandbox environments are unqualified on every real provider.

| Surface | Before | Now |
|---|---|---|
| `[workspace] prepare`, `inputs`, `share`; `WorkspaceSpec::prepare`, `inputs`, `share` | setup in every new worktree | setup once per environment key; later branches restore it (clone, copy, or a link for `share`), or branch their sandbox from its snapshot; the last good build when one fails |
| `.worktreeinclude` | ignored | its ignored literal paths copied into every new worktree with a workspace; `by` gives a branch an empty workspace when the file exists without `[workspace]` |
| `WorkspaceReport::environment` (`EnvironmentUse`), `SandboxOrigin::Environment` | none | how setup related to a prepared environment: `built`, `restored`, `last_good`, `not_kept`, with the key used, method and reason; in `schema/contract.json` |
| `Yard::environments`, `environment_key`, `rebuild_environment`, `prune_environments` | none | SDK only |
| `by env list`, `show [KEY]`, `rebuild`, `prune [KEY...] [--keep N] [--older-than DAYS]` | none | local only; refused with `--remote` (a server's environments are on its host); `rebuild` needs trust and never runs in a harness |
| `require_labels` on task, send, fork, reincarnate and spawn requests; `Operation::requires`, `waiting` | none | only a worker carrying every label claims the operation; `waiting` says why one waits once `unclaimable_after` passes |
| `GET /v1/repos/{repo}/operations?branch=B`, `Repo::operations` (`OperationList`) | none | the caller's tenant's unfinished operations of a repository |
| `--require-label` on `by run`, `fan`, `send`, `fork`, `reincarnate` | none | with `--remote`; refused locally |
| `by serve`/`by worker`/`branchyard-server` `--label`, `--unclaimable-after`; `labels`, `unclaimable_after_seconds` in the configuration | none | the worker's labels; how long before an unclaimable operation says why |
| `by show BRANCH --remote` for a branch not created yet | `unknown_branch` | the queued operation that will create it, with `requires` and `waiting` |
| `workers` (SQLite), `by_workers` (PostgreSQL); `requires` on the queue | none | live workers with their labels; each queue row's required labels |

## Added with connectors

A branch's harness calls GitHub, Slack or an internal API through Anvil's gateway with a per-turn token for its grant; see [connectors](connectors.md).

| Surface | Before | Now |
|---|---|---|
| `Provisioning::connectors` (`GrantEntry`, `GrantMode`, `Confirm`) | none | `--connector GRANT` on `run`, `fan`, `send`, `fork`; `[connectors] grants` for new private branches; a seat's `connectors` in a rig; `provision.connectors` over HTTP and `by --remote` (`403 connectors_not_configured` on a server without `connectors`); stored with the branch |
| `Spawn::connectors`, `SpawnSpec::connectors` | none | `by spawn --connector`, the MCP `spawn` tool and graph proposals' `connectors`, `branchyard.spawn(..., connectors=[...])`, `connectors` on the server's spawn request; always narrowed to the parent's grant, an entry with nothing in common refused `denied` |
| `TaskOptions::actor`, `connectors::Actor` | none | who a new branch acts for at the gateway; the server sets it to the request's principal; inherited by forks and children |
| `Yard::use_connectors`, `connectors()`, `ingest_connector_audit`; `connectors::{Gateway, KeyRing, Claims, Packager, AnvilPackager, Bundle}`, `connectors::gateway::{GatewayCommand, Supervisor, Background}` | none | SDK: give a yard its gateway, sign tokens, package connectors, read the audit log, supervise Anvil; `by` sets the gateway from `[connectors]`, a server from its `connectors` configuration |
| `Activity::ConnectorCall` (`ConnectorCall`); `Activity::Provisioned::connectors` | none | each gateway call on its branch's log: `connector_call` in `by log --json` and event streams, a `connector:` line in `by log`, the current call in `by watch`; the provisioned connectors; in `schema/contract.json` |
| `by gateway start [--foreground]\|stop\|status\|rotate-key [--keep N]\|jwks` | none | local only: run Anvil's gateway supervised, inspect it, rotate the yard's signing key; a server runs one with `"run_gateway": true` |
| `by connect CONNECTOR [--account NAME] [--api-key-stdin] [--open]` | none | local only: `anvil connect` as you, with a token naming you and granting nothing |
| `GET /.well-known/jwks.json` | none | the server's public keys, no token; `404` without `connectors` |
| `[connectors]` in `branchyard.toml`, `connectors` in the server configuration | none | in `schema/branchyard.config.json` and `schema/server.config.json`; a rig seat's `connectors` in `schema/rig.json` |

## Added with the ports

Ports from emdash and Orca ([roadmap](roadmap.md), Wave 1). The review and catalog commands are the CLI's; thread resolution is the pull-request watch's, and stays local only with it.

| Operation | SDK | by | by --remote | HTTP | client | delegation |
|---|---|---|---|---|---|---|
| Comment on a diff and send the comments as one prompt | no: `Branch::diff` and `send`; the editor and format are the CLI's | `review [--print] [--file F] [--detach]` | yes: the diff comes from the server, the editor runs here, the prompt is a `send` | `GET` the diff, then a send | `diff`, then a send | no: `by review` refuses inside a harness (it needs a person and an editor); send a prompt with `by send` |
| Answer and resolve the review threads a push addressed | `PullRequestActivity::ThreadsResolved` records it | inside `pr --watch` (`--no-resolve` to leave them) | no: `pr` is local only | no | no | no |
| Harness CLIs Branchyard knows of | `branchyard_controls::catalog::harnesses` | `harnesses --all [--json]` | yes: the catalog is built in; profiles are listed, `PATH` is not checked | no | no | the `by` command |
| Connector catalog | `branchyard_controls::catalog::connectors` | `connectors catalog [--json]` | yes, built in (no server is contacted) | no | no | the `by` command |
| Import another tool's workspace configuration | `branchyard_setup::import` | inside `init project` | n/a: setup is local | n/a | n/a | n/a |

| Key in `by watch` | Runs | by --remote |
|---|---|---|
| `v` | `review --detach` in the terminal the dashboard leaves to it, for its editor; the dashboard comes back when it exits | yes |

| Surface | Before | Now |
|---|---|---|
| `PullRequestActivity` | ... `watch_stopped` | also `threads_resolved` (`commit`, `threads`: `ResolvedThread` `id`, `path`, `replied`, `resolved`, `error`); `schema/contract.json` regenerated |
| The watch's `reviewThreads` query | `isResolved`, path, line, comments | also each thread's `id` |
| `branchyard_controls::harness::Harness` | Herdr, Scion and integration-target names | also `emdash` and `orca` names; 47 harnesses, 20 of them new from emdash's and Orca's registries |
| `WorkspaceFacts` (setup) | lockfile suggestions | also `notes` (what an import left out), and imported files in `found`; `schema/setup.protocol.json` unchanged |
| `watch::actions::Run` | `Background`, `Wait`, `Pane`, `Copy`, `Toggle`, `Open` | also `Terminal`: a `by` command the dashboard leaves the screen to |

## Changed from 4609ca1

| Gap | Before | Now |
|---|---|---|
| `by --remote --provider`, `--delegate`, `--allow-delegation`, `--allow-unapproved-tools` | refused by the CLI | sent in the request; the server's operator opts in |
| `by --remote spawn`, `inspect`, `events`, `integrate`, `children`, `send --json` | refused: "does not run with --remote yet" | server endpoints, the same JSON as local |
| Providers on the server | local only | `--allow-provider microsandbox,substrate` |
| Delegation for the server's harnesses | none | `--allow-delegation`, `--by-path` |
| Unapproved tools on the server | refused | `--allow-unapproved-tools` |
| PostgreSQL | design only | `Yard::open_postgres`, `by serve --database` |
| Several servers, and execute-only workers, on one database | one server per data directory | `by serve --database` on each, `by worker --database`; operations looked up by key with `GET /v1/operations?idempotency_key=` (`Client::operation_by_key`) ([server](server.md#several-servers-on-one-database)) |
| `HarnessInfo` | not `Serialize`; copied field by field | serde |
| `by --remote artifact`/`scratch`, the server's HTTP API, `branchyard-client` for storage | refused: "does not yet reach a server over --remote" | server endpoints, the same JSON as local; `--max-artifact-bytes` |

## Added with the fleet table, routing and the judge

Routing, judging and the outcome store run in local mode; `by --remote` refuses each with a message. See [fleet](fleet.md).

| Surface | Before | Now |
|---|---|---|
| `[fleet.<kind>]`, `[fleet.default]` in `branchyard.toml` (either file) | none | `candidates` (`harness`, `model`, `effort`, `command`), `attempts`, `budget_usd`, `max_turns`, `max_minutes`, `judge`, `failover`, `exploration`, `environment`, `connectors`; checked strictly; in `schema/branchyard.config.json` |
| `by run` without `--harness` when a `[fleet]` exists | `[defaults] harness`, else claude-code | routed through the table; `[defaults] harness` applies only without a `[fleet]` |
| `by run --auto`, `by fan --auto`, `--kind`, `--seed`; `by fan --attempts`, `--judge`; `by fan` without `--harness` | `fan` required `--harness` | `Yard::route`, `run_routed`, `fan_routed`, `run_with_kind` (`Fleet`, `FleetEntry`, `FleetCandidate`, `RouteOptions`, `Route`, `Routed`, `TaskKind`, `classify`); `--auto` with `--harness` is refused |
| `by judge`, `by fleet stats`, `by fleet route` | none | `Yard::judge` (`JudgeOptions`, `Judge`, `HarnessJudge`, `Judgement`, `parse_verdict`, `deterministic_scores`), `Yard::outcomes`, `fleet_stats` |
| `Activity::Fleet` (`FleetActivity`: `routed`, `failed_over`, `judging`, `judged`) | none | `fleet` in `by log --json`, a line in `by log`; in `schema/contract.json` |
| Failover | none | after a routed run's, fan's or send's turn fails for its harness: `Yard::failover`; `harness_fault` classifies |
| A send naming no model | replaced the branch's provisioning, dropping its model | keeps the branch's model and effort; a different model is refused once a turn has run |
| `by compare --pick --json` | printed the merge line before the JSON | JSON only (locally); `by judge --pick` uses the same code |
| `by compare --fan NAME` | `NAME-<harness>` | also a routed fan's `NAME-<harness>-<n>` |
| `outcomes` table (SQLite), `by_outcomes` (PostgreSQL) | none | one row per created top-level branch, kept after removal ([fleet](fleet.md#outcomes)) |

## Added with Wave 2's developer conveniences

Quota meters, more trackers, listening ports and adopting sessions ([usage](usage.md), [pull requests](pull-requests.md#other-trackers), [workspace](workspace.md#listening-ports)). Everything here reads this machine (its session files, its processes, its environment's tokens), so it is local only except where said.

| Operation | SDK | by | by --remote | HTTP | client | delegation |
|---|---|---|---|---|---|---|
| Usage of each local login's 5-hour and weekly windows | no: the CLI's (`usage.rs`) | `usage [--json]` | runs locally: it reads this machine's files, whatever `--remote` says | no | no | the `by` command, in the harness's own environment |
| Warn or refuse near a limit; skip a routed candidate | `RouteOptions::excluded` (the reasons the CLI computed) | `[usage] guard`, `near_percent`, `skip_over` on `run`, `fan` | no: routing is local, and the guard is not applied | no | no | no |
| An issue from Linear, Jira or GitLab | no: the prompt and `IssueLink` (`tracker`, `key`) are recorded | `run`, `fan`, `spawn --issue linear:KEY\|jira:KEY\|gitlab:PATH#N\|URL` | yes: fetched here with the environment's tokens, the prompt sent; the gateway path is local only | the prompt | the prompt | `spawn --issue`, with the harness's environment |
| Start from a pull request's head | `PullRequestActivity::Started` records it | `run`, `fan --pr N` | no: the head is fetched into this repository | no | no | no |
| Listening ports of a branch | no: the CLI's (`ports.rs`) | `workspace ports\|browse\|kill`, `show` (`listening`), `workspace show BRANCH` | no: the processes are on the server | no | no | `by workspace ports` with `$BRANCHYARD_BRANCH` |
| Several run scripts at once | `Yard::workspace_env` | `workspace run BRANCH A B --detach` | no | no | no | the `by` command |
| Adopt a Claude Code or Codex session | `Yard::adopt(AdoptSpec)`, `Activity::Adopted` (`Adoption`) | `adopt [--list] [SESSION] [--name N] [--harness ID] [--no-diff] [--json]` | no: the session's files and directory are here | no | no | no: refused inside a harness |

| Key in `by watch` | Runs | by --remote |
|---|---|---|
| `b` | `workspace browse` on the selected branch, waited for | no |
| `K` | `workspace kill --yes` after a yes | no |

| Surface | Before | Now |
|---|---|---|
| `IssueLink` | `number`, `url`, `title` | also `tracker` and `key`, omitted for GitHub; `schema/contract.json` regenerated |
| `PullRequestActivity` | ... `threads_resolved` | also `started` (a `PullRequestRef`) |
| `Activity` | ... `connector_call` | also `adopted`; `adopted` in `by log --json` |
| `RouteOptions` | `kind`, `seed`, `attempts`, `failover` | also `excluded` |
| `Gateway` (connectors) | `person_token` (no grant) | also `person_token_granted` |
| `branchyard.toml` | ... `[connectors]` | also `[usage]` (`guard`, `near_percent`, `skip_over`, `claude_five_hour_tokens`, `claude_weekly_tokens`, `[usage.accounts.NAME]` with `harness`, `dir`) and `[trackers.linear\|jira\|gitlab]` (`url`, `gateway_tool`); `schema/branchyard.config.json` regenerated |
| `by workspace run` | `[BRANCH] [NAME]` | `[BRANCH] [NAME...]`; several names need `--detach`, each gets its own port, and `BRANCHYARD_BRANCH_PORT` |
| `by watch` header and detail pane | ... | a usage summary (every minute); a `ports` line per listener (every five seconds) |
