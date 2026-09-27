# Branchyard server

`branchyard-server` (also `by serve`) serves one or more repositories, each under a name, over an authenticated HTTP JSON API with Server-Sent Events. It runs the local-mode engine in-process: the same tasks, branches, event logs, budgets, policies and validated merges as `by`, reached over the network. `by --remote URL` runs every `by` command against it, and [`branchyard-client`](../crates/branchyard-client/src/lib.rs) is the typed Rust client.

> **Status.** Tested hermetically over real HTTP on loopback against a fake ACP agent, and on PostgreSQL 16 for the database store. Not yet run against a real harness through the server, not load-tested, and not deployed. The default provider is the local process provider: **no isolation** (see [Security](#security)). The server's operator can allow the Microsandbox and Substrate providers, delegation and unapproved tools; [surfaces](surfaces.md) lists what works on which surface.

## Running it

```sh
cd path/to/repo
by serve                      # serves this repository as its directory name on 127.0.0.1:8421
# branchyard-server: created a token in .branchyard/server/token; clients pass --token-file with it

by --remote http://127.0.0.1:8421 --token-file .branchyard/server/token ls
```

Flags (`by serve --help` or `branchyard-server --help`):

| Flag | Meaning |
|---|---|
| `--config FILE` | JSON configuration, below. Flags override it |
| `--listen ADDR` | `IP:port`; default `127.0.0.1:8421`. Port 0 picks one; the server prints `listening on URL` to stdout |
| `--repo NAME=PATH` | Serve the repository at `PATH` as `NAME` (repeatable). Default: the repository containing the current directory, named after its directory |
| `--data-dir DIR` | Operation registry and activity feeds. Default: `.branchyard/server` in the first repository |
| `--token-file FILE` | Accept the token on the first line of `FILE` (repeatable). Default: `DATA-DIR/token`, created with mode 600 if missing |
| `--tls-cert FILE`, `--tls-key FILE` | PEM certificate chain and key; serve HTTPS |
| `--insecure-bind` | Allow plain HTTP on a non-loopback address; prints a loud warning |
| `--harness-command H=CMD` | Launch harness `H` as `CMD` (split on spaces) when a request names no command |
| `--allow-client-commands` | Accept a request's own `command` (see [Security](#security)) |
| `--allow-provider P,...` | Accept requests naming these providers besides `local`: `microsandbox`, `substrate` (repeatable). Default: local only |
| `--allow-delegation` | Accept delegation envelopes, `allow_delegation` and the spawn endpoint: harnesses get the delegation tools with this server's `by` |
| `--by-path PATH` | The `by` a delegating harness gets. Default: this executable when it is `by` (as under `by serve`), else `by` beside it, else on `PATH` |
| `--allow-unapproved-tools` | Accept `unapproved_tools`: profiles whose tools bypass the request's policy (Antigravity, Pi, Amp) |
| `--secret NAME[=VAR\|=@FILE]` | A secret requests may name in `provision.secrets`, read from this server's variable `NAME` or `VAR`, or from `FILE`, at each turn; repeatable. See [provisioning](provisioning.md#through-a-server) |
| `--database URL` | Keep branch state and operations in PostgreSQL (`postgres://user@host/db`) instead of SQLite. Needs a build with the `postgres` feature; see [PostgreSQL](#postgresql) |
| `--max-running N` | Operations running at once; more wait queued. Default 8 |
| `--shutdown-grace SECS` | At shutdown, how long running operations may finish. Default 60 |
| `--quiet` | Do not log requests |

Configuration file (relative paths resolve against the file's directory; unknown keys are errors):

```json
{
  "listen": "127.0.0.1:8421",
  "data_dir": "/var/lib/branchyard",
  "repos": { "app": "/srv/app", "docs": "/srv/docs" },
  "tokens": [
    { "name": "ci", "token_file": "/etc/branchyard/ci.token" },
    { "name": "alice", "token": "at-least-16-characters" }
  ],
  "tls": { "cert": "/etc/branchyard/cert.pem", "key": "/etc/branchyard/key.pem" },
  "max_body_bytes": 1048576,
  "max_running": 8,
  "shutdown_grace_seconds": 60,
  "harness_commands": { "codex": ["/opt/codex/bin/codex"] },
  "allow_client_commands": false,
  "allow_providers": ["substrate"],
  "allow_delegation": false,
  "by_path": "/usr/local/bin/by",
  "allow_unapproved_tools": false,
  "secrets": { "ANTHROPIC_API_KEY": "ANTHROPIC_API_KEY", "CODEX_AUTH": "@/etc/branchyard/codex-auth.json" },
  "database": "postgres://branchyard@db.internal/branchyard"
}
```

The server refuses to start with no repository, no token, a token shorter than 16 characters, an unknown provider name, a `by_path` that is not a file, a `database` that is not a `postgres://` URL, or a plain-HTTP bind to anything but loopback without `--insecure-bind`. It warns when a token file, or a configuration holding inline tokens or a database password, is readable by other users. A password in `--database` is visible to other users of the host in its process list; prefer the configuration file, mode 600.

SIGINT or SIGTERM starts a graceful shutdown: no new connections or operations (`503 shutting_down`), event streams end, requests in flight finish, and running operations get the grace period. Operations still running after it are recorded as `interrupted`. A second signal stops waiting at once.

## Authentication

Every route except `GET /healthz` needs `Authorization: Bearer <token>`, including unknown routes, so routes cannot be probed anonymously. Tokens come from the configuration or token files. The presented token is compared with every configured token in time that depends only on lengths, and neither tokens nor headers are ever logged. A missing or wrong token gets `401 unauthorized` with `WWW-Authenticate: Bearer`.

The token's configured `name` is the caller's identity for idempotency scoping. There is no per-token authorization: every token can do everything on every served repository.

## API reference

All bodies are JSON (`Content-Type: application/json` is required on `POST`, else `415`). Branches, statuses and recorded events use the SDK's own serde forms, the same values `Yard::branches` and `Branch::events` return. Every response carries `x-request-id` (the caller's, when it sends a usable one).

| Method and path | Does | Returns |
|---|---|---|
| `GET /healthz` | Liveness, no token | `ok` |
| `GET /v1/repos` | Served repositories | `{"repos": [{"name", "root"}]}` |
| `GET /v1/harnesses` | Harness profiles, as found on the server's `PATH` | `{"harnesses": [{"harness", "profile", "default", "available", "qualification"}]}` |
| `POST /v1/repos/{repo}/tasks` | Run a task on one branch, or one branch per harness | `202` operation |
| `GET /v1/repos/{repo}/branches` | Every branch, oldest first | `{"branches": [BranchInfo]}` |
| `GET /v1/repos/{repo}/branches/{b}` | One branch | `BranchInfo` |
| `POST /v1/repos/{repo}/branches/{b}/send` | Continue its session with another prompt | `202` operation |
| `POST /v1/repos/{repo}/branches/{b}/fork` | New branch from its candidate | `202` operation |
| `POST /v1/repos/{repo}/branches/{b}/reincarnate` | New branch from its candidate, always a fresh session, with a generated handoff brief | `202` operation |
| `POST /v1/repos/{repo}/branches/{b}/merge` | Validated merge of its candidate | `202` operation |
| `POST /v1/repos/{repo}/branches/{b}/cancel` | Stop its running turn and every running turn delegated below it | `{"cancelled": ["b", …]}` |
| `DELETE /v1/repos/{repo}/branches/{b}` | Remove worktree and record | `{"removed": "b"}` |
| `GET /v1/repos/{repo}/branches/{b}/diff` | Candidate diff against the base | `{"diff": "..."}` |
| `GET /v1/repos/{repo}/branches/{b}/events?cursor=N` | Recorded events after the first `N` (default 0); a branch's events are numbered from 1 | `{"events": [RecordedEvent], "cursor": M}`; pass `M` next |
| `POST /v1/repos/{repo}/branches/{b}/spawn` | A child of `b`, as `by spawn --parent b` makes one; needs `--allow-delegation` | `202` operation |
| `POST /v1/repos/{repo}/branches/{b}/integrate` | Merge delegated child `b` into the branch that delegated it, as `by integrate b` | `202` operation |
| `GET /v1/repos/{repo}/branches/{b}/inspection` | `b` as a delegating parent sees it, as `by inspect b --json` | `Inspection` |
| `GET /v1/repos/{repo}/branches/{b}/event-page?cursor=N&limit=M` | Up to `M` (default 50, at most 200) events after the first `N`, or the most recent without a cursor, as `by events b --json` | `EventPage` |
| `GET /v1/repos/{repo}/branches/{b}/children` | Every branch `b` delegated to, directly or below, as `by children b --json` | `Children` |
| `GET /v1/repos/{repo}/events/stream?cursor=N` | SSE of activity across branches after feed position `N`; without a cursor, from now | `text/event-stream` |
| `GET /v1/operations/{id}` | An operation's status and result | operation |

### Requests

```json
POST /v1/repos/app/tasks
{
  "prompt": "Make the flaky parser test deterministic",
  "harness": "claude-code",
  "harnesses": ["claude-code", "codex"],
  "name": "flaky",
  "base": "main",
  "budget": { "max_usd": 2.0, "max_turns": 3, "max_seconds": 1200,
              "stall_after_seconds": 300, "stall_action": "notify" },
  "policy": { "mode": "deny", "rules": [{ "tool": "Read", "allow": true }, { "tool": "mcp__*", "allow": false }] },
  "check": ["cargo", "test"],
  "isolated": false,
  "command": ["/opt/claude/bin/claude"],
  "provider": { "kind": "substrate", "endpoint": "http://ate:8080", "router": "http://ate:8081/{atespace}/{actor}/",
                "template": "by-bridge", "key": "/etc/branchyard/bridge.key", "pass_env": ["ANTHROPIC_API_KEY"] },
  "delegation": { "max_depth": 1, "max_children": 4, "harnesses": [] },
  "allow_delegation": true,
  "unapproved_tools": false
}
```

Only `prompt` is required. Give `harness` for one branch or `harnesses` for one branch each (`<name>-<harness>`), not both. `policy.mode` is `allow` or `deny` (the default); rules apply first, in order. There is no remote `ask`: the server has no terminal to ask on. `budget.max_seconds` applies per call, like the SDK's `max_duration`. `budget.stall_after_seconds` and `budget.stall_action` (`notify`, the default, or `interrupt`) are the SDK's `Budget::stall_after`/`stall_action` (`docs/lifecycle.md#stall-detection`); a branch's `stalled` field then reflects the same live state a local `by ls` would show. `command` is refused (`403 command_not_allowed`) unless the server allows client commands; without it, the server uses its `harness_commands` entry for the harness, else the profile's executable on its `PATH`. One task runs one command, so harnesses with different configured commands must be separate tasks.

`provider` is [`branchyard::Provider`](../crates/branchyard/src/lib.rs)'s serde form, tagged by `kind`: `local`, `microsandbox` or `substrate`, with [the provider's options](providers.md). Anything but `local` is refused (`403 provider_not_allowed`) unless the operator allowed that provider. Everything in it is about the server: `pass_env` names are read from **the server's environment** at each turn, a Substrate `key` must be an absolute path to a file on the server (`400` otherwise), and a Microsandbox provider needs a server built with the `microsandbox` feature on a host with KVM.

`provision` is [`branchyard::Provisioning`](provisioning.md)'s serde form: `secrets` (names only, `[{"name": "ANTHROPIC_API_KEY"}]`; a `from` is refused with `400`, and a name the operator did not define with `403 secret_not_allowed`; values come from the server's table), `auth`, `mcp_servers` (commands the server runs: refused unless it allows client commands), `instructions`, `model`, `effort` and `telemetry`. It is accepted on task, send and fork requests and stored with the branch, with the server's sources.

`delegation` is an [`Envelope`](delegation.md#the-envelope), `{max_depth, max_children, harnesses}`, all three required; with it the harness gets the delegation tools, through the server's `by`, for each of its turns. `allow_delegation` adds the rule that allows the harness's shell commands running that `by` with a delegation subcommand, after the policy's own rules, like `by --allow-delegation`. Either is refused (`403 delegation_not_allowed`) unless the server allows delegation. `seats` makes the branch a [rig](rigs.md)'s root, in [`Seats`](../crates/branchyard/src/seats.rs)'s serde form (`{rig, seat, delegates_to, table}`); it needs `delegation`, is refused like it without the opt-in, and each seat's `provision` is held to the rules above, so its secrets come from the server's table. `by --remote rig run` sends it. `unapproved_tools` runs a profile whose driver cannot route tool approvals, like `by --allow-unapproved-tools`, and is refused (`403 unapproved_tools_not_allowed`) unless the server allows it.

`send` takes `prompt`, `budget`, `policy`, `check`, `command`, `delegation`, `allow_delegation` and `unapproved_tools`; the branch keeps its harness, provider, recorded command, environment and envelope. A server that does not allow delegation refuses to send to a branch that was given an envelope, since its harness would get the tools. `fork` takes `prompt`, `name`, `fresh_session`, `harness`, `budget`, `policy`, `check`, `isolated`, `command`, `provider`, `delegation`, `allow_delegation` and `unapproved_tools`; without a provider it keeps its parent's. `reincarnate` (`docs/lifecycle.md#reincarnation`) takes the same fields as `fork` except `prompt` and `fresh_session` (there is no prompt to give: the engine generates a handoff brief, and the session is always fresh); without `harness` or `provision.model` it keeps its parent's, so a plain call reincarnates onto the same configuration with a fresh session. `merge` takes an optional `target`, defaulting to the branch checked out in the served repository.

`spawn` takes `prompt`, `harness`, `name`, `base`, `budget`, `policy`, `check`, `max_depth`, `deny`, `unapproved_tools` and `seat`, the flags of `by spawn`; with a `seat` and no `name`, the child is named `<parent>-<seat>` before the operation starts. It acts with the server's authority as a person, as `by spawn --parent` does locally: the parent's envelope bounds the child exactly as it bounds a local spawn, and a parent without delegation cannot spawn. The operation runs the child's first turn, waits for the parent's subtree on this server as the local command does, and its result holds the child's `inspection`. It locks the parent as well as the child, as `integrate` locks both branches, so a send to the parent or its removal is refused with `409 branch_busy` until the operation finishes. `integrate` takes `{}` and refuses (`403 denied`) a branch no other branch delegated; its result holds `merged`. The inspection, event page and children reads act with the same authority and need no opt-in.

`cancel` takes an empty object, `{}`. It is not an operation: it records a durable cancel request for the branch's running turn and each running turn delegated below it, and answers `200` with the branches that were running (an empty list when none was, and for a repeat). The engine running each turn, in the server or in another process on the repository such as a local `by run`, observes the request within 100 ms, interrupts the harness, and ends the branch `interrupted`; the operation that ran the turn then succeeds with that status. The branch's log records `cancelled by <token name> through the server`. It ignores branch locks, since the branches it is for are the ones an operation holds, and needs no idempotency key.

### Operations

Task, send, fork and merge are long operations. The server validates the request, records the operation durably, and answers `202 Accepted` with `Location: /v1/operations/{id}` before the work starts:

```json
{ "id": "op_…", "repo": "app", "kind": "task", "state": "queued",
  "branches": ["flaky"], "cursor": 41, "created_at_ms": 1790000000000 }
```

`kind` is `task`, `send`, `fork`, `merge`, `spawn` or `integrate`. `state` moves through `queued`, `running`, then `succeeded`, `failed` or `interrupted`. A finished operation adds `finished_at_ms`, `end_cursor`, and either `result` or `error` (`{"code", "message", "detail"}`). `result` has `branches` (`[BranchInfo]`); `merged` (`{"branch", "target", "previous", "commit"}`) for a merge or integration; `descendants` (`[BranchInfo]`), every branch the operation's branches delegated to, once they finished, since a task, send or fork waits for its subtree as `by run` does; and `inspection` for a spawn. A branch that ends `failed` or over budget is a *succeeded* operation whose branch status says so, exactly as the SDK returns `Ok(branch)`; an operation fails when the SDK call returns an error, such as an unknown harness or a refused merge.

`branches` are the names planned at acceptance. `cursor` is the feed position at acceptance and `end_cursor` the position once the operation's activity was ingested: to watch one operation, stream from `cursor` until the operation finishes and the stream reaches `end_cursor`, keeping entries for its branches. That is what `by --remote … run` does.

While an operation runs, the branches it works on are locked: another send, merge, fork into the same name, or removal gets `409 branch_busy`. The lock is the server's; a local `by` on the same repository does not see it.

### Idempotency

Send `Idempotency-Key: <1–255 visible ASCII characters>` on any `POST`. The key is scoped to the caller (token name) and fingerprinted with the route and the canonical request. A repeat with the same request returns the original operation, `200` if it has finished (with its result) or `202` if not, with `Idempotent-Replayed: true`, and never starts a second run; this holds across restarts. The same key with a different request gets `422 idempotency_key_reused`. Keys are kept for as long as the registry is. `branchyard-client` and `by` send a fresh key per command and retry a `POST` with the same key after a connection failure.

### Event stream

`GET /v1/repos/{repo}/events/stream?cursor=N` (or `Last-Event-ID: N`, which wins) first sends an `open` event whose `id` is the starting cursor, then one `activity` event per feed entry, then waits for more, with a comment every 15 seconds:

```text
event: open
id: 41
data: {"cursor":41,"head":57}

event: activity
id: 42
data: {"seq":42,"branch":"flaky","at_ms":1790000000123,"activity":{"harness":{"type":"tool_started","turn":1,"call_id":"c1","name":"Bash"}}}
```

The feed is the repository's own event store (see [durability](durability.md#reading-events-from-a-cursor)): each recorded event of every branch has a position, assigned in commit order, from 1. Per-branch order follows the branch's log; across branches it is the order events were recorded, not `at_ms`. Positions only grow and may skip numbers. Reconnecting with the last `id` continues with the next entry, with no gap or repeat, across server restarts. A cursor past the end gets `400 cursor_out_of_range`. Activity recorded by other processes in the same repository (a local `by run`) appears within the poll interval, 500 ms. Positions from a server of an earlier version, which kept its own copy of the feed, name different events.

### Webhooks

`by serve --webhook URL [--webhook-secret FILE] [--webhook-events KINDS] [--webhook-insecure]`, repeatable (each `--webhook-secret`/`--webhook-events` applies to the `--webhook` immediately before it), or in the JSON config:

```json
{ "webhooks": [
    { "url": "https://ops.example/hooks/branchyard", "secret_file": "/etc/branchyard/hook.key",
      "events": ["stall", "merge", "failure"] }
  ],
  "webhook_insecure": false
}
```

Without `secret`/`secret_file`, a secret is generated into `DATA-DIR/webhook-N.secret` (mode 600), the same way an omitted token is. `events` filters which kinds of activity this target receives — `status`, `stall`, `permission_wait`, `merge`, `failure` — omitted or empty means every kind; a status change is also tagged `merge` or `failure` when it settles there. `url` must be `https://` unless its host is a loopback literal (`localhost`, `127.0.0.1`, `::1`) or `--webhook-insecure`/`webhook_insecure` is given.

Each target gets every served repository's activity, delivered from the durable feed by its own cursor (`docs/durability.md#webhook-cursors`), so a restart resumes rather than replaying or skipping: at-least-once. A delivery is:

```json
POST https://ops.example/hooks/branchyard
X-Branchyard-Signature: sha256=<hex HMAC-SHA256 of the body, with the target's secret>
X-Branchyard-Delivery: 57
Content-Type: application/json

{"repo": "app", "seq": 57, "branch": "flaky", "at_ms": 1790000000123,
 "kinds": ["stall"], "activity": {"stalled": {"since_ms": 1790000000000}}}
```

`seq` (also sent as `X-Branchyard-Delivery`) is the feed position: use it as the receiver's dedupe key, since at-least-once means the same entry can arrive more than once. `activity` is the same [`RecordedEvent::activity`](../crates/branchyard/src/lib.rs) shape the SSE stream and `by log --json` use. A delivery is retried with backoff on a non-2xx response or a connection failure; after repeated failure it is logged as dead-lettered to the server's stderr and the cursor still advances, so one broken target never blocks the others or the feed.

### Errors

Every error is `{"error": {"code", "message", "detail"?}}`. Codes are stable; messages are for people. SDK errors keep the SDK's own message, so `by --remote` prints what local `by` prints.

| Code | Status | Meaning |
|---|---|---|
| `unauthorized` | 401 | Missing or wrong bearer token |
| `not_found`, `method_not_allowed` | 404, 405 | No such route or method |
| `unknown_repo`, `unknown_branch`, `unknown_operation` | 404 | No such thing |
| `invalid_request` | 400 | Malformed JSON, unknown field, empty prompt, bad budget, bad cursor |
| `unsupported_media_type` | 415 | `POST` body not declared as JSON |
| `body_too_large` | 413 | Body over `max_body_bytes`; `detail.limit` |
| `command_not_allowed` | 403 | A request `command`, or `provision.mcp_servers`, on a server that does not allow client commands |
| `secret_not_allowed` | 403 | A `provision.secrets` name the server's operator did not define; `detail.secret` |
| `provider_not_allowed` | 403 | A provider the server's operator did not allow; `detail.provider` |
| `delegation_not_allowed` | 403 | A delegation envelope, `allow_delegation`, a spawn, or a send to a branch with an envelope, on a server that does not allow delegation |
| `unapproved_tools_not_allowed` | 403 | `unapproved_tools` on a server that does not allow them |
| `idempotency_key_reused` | 422 | Same key, different request; `detail.operation` |
| `branch_busy` | 409 | Another operation holds the branch; `detail.holder` |
| `cursor_out_of_range` | 400 | Stream cursor past the feed's end; `detail.head` |
| `detached_head` | 409 | Merge without a target while the served repository's HEAD is detached |
| `shutting_down` | 503 | The server is stopping |
| `interrupted` | (operation) | The server stopped before the operation finished |
| `internal`, `git_error`, `io_error`, `state_error`, `harness_error`, `not_a_repository` | 500 | Server-side failure |
| `branch_exists`, `no_candidate`, `target_moved`, `conflict`, `dirty_target`, `already_merged`, `running`, `fenced` | 409 | SDK refusals; `target_moved` has `detail.expected`/`actual`, `conflict` has `detail.files`. `running`: another engine, such as a local `by`, runs a turn on the branch. `fenced`: the engine lost the branch's lease to another |
| `invalid_name`, `unknown_harness` | 400 | SDK refusals |
| `denied` | 403 | The envelope or authority refused a delegation operation, as `by --json` reports `denied` |
| `remote_error` | 502 | An error the engine running a delegating turn returned through its broker; `detail.kind` is its kind |
| `harness_unavailable`, `unsupported`, `check_failed`, `check_timed_out`, `check_not_started`, `invalid_candidate` | 422 | SDK refusals; check failures have `detail.output_tail` |

## Remote mode in `by`

```sh
export BRANCHYARD_REMOTE=https://by.example:8421
export BRANCHYARD_TOKEN_FILE=~/.config/branchyard/token
by --repo app run "Make the flaky parser test deterministic" --check "cargo test" --yes
by fan "..." --harness claude-code,codex --yes
by ls; by diff flaky; by merge flaky; by watch; by cancel flaky
```

Global options go before the command: `--remote URL`, `--token-file FILE`, `--repo NAME` (needed when the server serves several), `--ca-file FILE` (extra trust for `https`). Each has an environment variable: `BRANCHYARD_REMOTE`, `BRANCHYARD_TOKEN_FILE`, `BRANCHYARD_REPO`, `BRANCHYARD_CA_FILE`. Output is the local output: the CLI renders the same SDK values with the same code, and tests compare each command's output, `watch` aside, local against remote, including the `--json` output of `spawn` (with and without `--seat`), `inspect`, `events`, `children`, `integrate`, `send` and `rig run`.

Every command runs remotely, with the same flags: `--provider` and its options, `--delegate`, `--allow-delegation` and `--allow-unapproved-tools` are sent in the request, and the server refuses what its operator did not allow. `spawn` needs `--parent`, as outside a harness locally, and the child runs on the server. Differences: `--ask` is refused (pass `--yes`, or leave requests denied); without `--yes` requests are denied; worktree paths, `--substrate-key` (which must be absolute) and `--pass-env` values are the server's; `by harnesses` shows the server's `PATH`. With `--json`, a server error prints `{"error": {"kind", "message"}}` with the kind local `by` reports (the server's `git_error` is `git`, and so on), and a server that cannot be reached is `unavailable`. Interrupting `by run` stops watching, not the work; `by cancel` stops the work. `by run --delegate` follows the activity of the children the branch spawns, and prints the `delegated` table once they have finished on the server.

## Herdr

[`plugins/herdr`](../plugins/herdr/README.md) is a [Herdr](https://github.com/herdrdev/herdr) plugin built on this API and `by`. Its bridge opens the event stream, lists the branches, and applies the entries after the stream's starting cursor ([`EventStream::open`](../crates/branchyard-client/src/lib.rs)); it keeps one Herdr tab per branch running `by log --follow <branch>`, and reports each branch's state on it with `herdr pane report-agent --source custom:branchyard` (`running` is `working`, a waiting permission request `blocked`, every settled status `idle` with what happened as the message). When the stream drops it reconnects after the last cursor it applied. Herdr actions run `by merge`, `by cancel` and `by send` for the branch in the focused pane. It uses the same `BRANCHYARD_*` settings as `by --remote`. It is tested against a fake `herdr`, not yet against Herdr itself.

`by log --follow` works in both modes: locally it waits on the store; remotely it polls the branch's events by cursor every half second and keeps retrying while the server is unreachable.

## Deployment notes

- **One server per data directory** is enforced with an advisory lock on `DATA-DIR/lock`; a second server on the directory fails at start. On a network file system it is only as good as that file system's `flock`.
- **TLS.** Terminate TLS in the server (`--tls-cert`/`--tls-key`, rustls with the ring provider, HTTP/1.1) or in a reverse proxy in front of a loopback bind. A proxy must not buffer `text/event-stream` responses. Clients trust the Mozilla roots plus `--ca-file`.
- **Harnesses** run as the server's user, with its `PATH`, `HOME` and harness logins. Install and log in the harnesses as that user, or set `harness_commands`.
- **Limits.** Request bodies are bounded (`max_body_bytes`, default 1 MiB), request heads must arrive within 30 seconds, at most 256 requests are handled at once (more wait), and at most `max_running` operations run at once (more queue). There is no per-request deadline beyond those, and no rate limiting per token.
- **Health.** `GET /healthz` needs no token. Each request is logged to stderr as `request-id method path status duration`, never with headers or bodies.
- **Backups.** Branch state and event logs are each repository's `.branchyard/state.db`, with worktrees under `.branchyard/worktrees/`; server state is `DATA-DIR/state.db` (operations, idempotency keys). Both are SQLite databases in write-ahead-log mode: back them up with `sqlite3 FILE ".backup COPY"` or while the server is stopped, not by copying the file alone. With `--database`, both are in PostgreSQL instead: back it up with `pg_dump` of the schema. Either way they grow without bound for now. An `operations.jsonl` from an earlier version is imported on first start and renamed `operations.jsonl.imported`; `DATA-DIR/feeds/` is no longer used.
- **Providers and delegation.** Allow a provider only once its cluster, image or runtime is set up for this server (see [providers](providers.md)); the credentials a turn gets are the server's `pass_env` variables. Delegation runs children on the server's threads and gives harnesses the server's `by`, which must be the same version as the server.

## What is durable

| State | Where | Survives a restart |
|---|---|---|
| Branches, candidates, event logs, the activity feed | Each repository's `.branchyard/state.db`, or the database with `--database`, written by the engine in transactions | Yes |
| Operations and idempotency keys | `DATA-DIR/state.db` committed with `synchronous=FULL`, or the database's `by_operations` table committed with `synchronous_commit = on`, before `202` and at each state change | Yes; unfinished ones become `interrupted` |
| Cancel requests, `max_duration` deadlines, turn leases, journaled steps, harness process identities | Each repository's `.branchyard/state.db`, or the database | Yes |
| Webhook delivery cursors | `DATA-DIR/state.db`'s `webhook_cursors` table, or the database's `by_webhook_cursors` | Yes |
| Branch locks | Memory | No; they end with the operations |
| A turn in progress | The server process and its harness child | No: it is recovered, not continued |

If the server dies mid-turn, the operation becomes `interrupted` at the next start, and opening the repository recovers the branch: the engine kills the harness's process group if its pid and start time still match, ends the branch `interrupted` with a `recovered` event that says whether the prompt had been submitted, and never submits it again. The server also recovers every 30 seconds, which covers a local `by run` on a served repository that was killed. [Durability](durability.md) describes the leases, the journal and the recovery rules, and what is not guaranteed.

## PostgreSQL

Build with the `postgres` feature (`cargo install --locked --path crates/branchyard-cli --features postgres`, or `-p branchyard-server --features postgres`), then:

```sh
by serve --database 'postgres://branchyard@db.internal/branchyard' --repo app=/srv/app
```

Each served repository's branch records, event log and feed, leases, journaled steps, harness processes and cancels are kept in the database under the repository's served name, with [the same semantics as SQLite](durability.md#postgresql); the operation registry is the `by_operations` table. Tables are created when missing, in the connection's `search_path` schema, so one schema per server: add `?options=-csearch_path%3Dname` to the URL to choose one. Worktrees, private homes and delegation tokens stay in each repository's `.branchyard/`, and the data directory still holds the default token.

What it is not yet:

- **Several servers on one schema.** Each registry recovers the other's unfinished operations as interrupted, branch locks are in each server's memory, and nothing delivers accepted operations through a queue: design §8's PGMQ is not built. One server per schema.
- **Shared with local `by`.** A local `by` on a served repository opens its SQLite `state.db` and sees none of the server's branches; use `by --remote`. Nothing is imported from an existing `state.db` when a repository moves to the database.
- **TLS to the database.** Connections are plain; keep the database on a trusted network or a Unix socket.
- **Waits** poll every 100 ms, as SQLite's do across processes; nothing listens for `NOTIFY`.

## Security

- **No isolation by default, even remotely.** Unless a request names an allowed sandbox provider, the server runs harnesses with the local process provider: as the server's operating-system user, with its environment, home and credentials. A remote caller with a token and `--yes` (policy `allow`) can make a harness do anything that user can. Serving a repository is granting its token holders that user's authority. Run the server as a dedicated, unprivileged user on a machine you are willing to hand over. `--allow-provider` does not restrict callers to that provider; they may still ask for `local`. Neither sandbox provider is qualified yet (design §4, M2).
- **Allowed providers use the server's credentials.** A token holder chooses which of the server's variables a sandboxed turn gets through `pass_env`, and which Substrate endpoint, router and key path it uses.
- **Delegation** gives the server's harnesses the delegation tools, bounded by the envelope. As in local mode, a harness running as the server's user can read other branches' tokens in `.branchyard/`; the envelope stops honest mistakes, not a hostile harness, until harnesses run in sandboxes. `--allow-unapproved-tools` lets token holders run profiles whose tools the request's policy never sees.
- **`allow_client_commands`** additionally lets any token holder choose the executable the server launches. Leave it off outside tests; configure `harness_commands` instead.
- **Tokens are all-powerful and equal.** No scopes, no per-repository access, no expiry or rotation beyond editing the configuration and restarting. Treat each as the server user's password.
- **Plain HTTP** exposes tokens, prompts and code to anyone on the path. The server refuses it off loopback unless told `--insecure-bind`.
- **Information exposure.** Responses include server paths (worktrees, repository roots) and the server's harness availability.
- **Policy is per request.** A `deny` default is safe; rules match tool names only, as in the SDK.

## Code

| Crate | Contents |
|---|---|
| [`branchyard-server`](../crates/branchyard-server/src/lib.rs) | Configuration, authentication, the operation registry and its SQLite and PostgreSQL stores, the activity feed, routes, TLS and shutdown. Tests: [`api.rs`](../crates/branchyard-server/tests/api.rs), [`parity.rs`](../crates/branchyard-server/tests/parity.rs) (opt-ins and delegation endpoints), [`postgres.rs`](../crates/branchyard-server/tests/postgres.rs) |
| [`branchyard-client`](../crates/branchyard-client/src/lib.rs) | Wire types, a blocking HTTP/1.1 client over rustls, an SSE parser, and a reconnecting event stream |
| [`branchyard-cli`](../crates/branchyard-cli/src/main.rs) | `by serve`, remote mode and `by watch` |
| [`branchyard-herdr`](../crates/branchyard-herdr/src/main.rs) | The Herdr plugin's binary: the bridge, the branch pane, and the merge, cancel and send actions. Tests: [`bridge.rs`](../crates/branchyard-herdr/tests/bridge.rs) (against `by serve` and a fake `herdr`), [`manifest.rs`](../crates/branchyard-herdr/tests/manifest.rs) |
