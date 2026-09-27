# Branchyard server

`branchyard-server` (also `by serve`) serves one or more repositories, each under a name, over an authenticated HTTP JSON API with Server-Sent Events. It runs the local-mode engine in-process: the same tasks, branches, event logs, budgets, policies and validated merges as `by`, reached over the network. `by --remote URL` runs every `by` command against it, and [`branchyard-client`](../crates/branchyard-client/src/lib.rs) is the typed Rust client.

> **Status.** Tested hermetically over real HTTP on loopback against a fake ACP agent (49 new tests across the server, client and CLI). Not yet run against a real harness through the server, not load-tested, and not deployed. The provider is still the local process provider: **no isolation** (see [Security](#security)).

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
  "allow_client_commands": false
}
```

The server refuses to start with no repository, no token, a token shorter than 16 characters, or a plain-HTTP bind to anything but loopback without `--insecure-bind`. It warns when a token file, or a configuration holding inline tokens, is readable by other users.

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
| `POST /v1/repos/{repo}/branches/{b}/merge` | Validated merge of its candidate | `202` operation |
| `POST /v1/repos/{repo}/branches/{b}/cancel` | Stop its running turn and every running turn delegated below it | `{"cancelled": ["b", …]}` |
| `DELETE /v1/repos/{repo}/branches/{b}` | Remove worktree and record | `{"removed": "b"}` |
| `GET /v1/repos/{repo}/branches/{b}/diff` | Candidate diff against the base | `{"diff": "..."}` |
| `GET /v1/repos/{repo}/branches/{b}/events?cursor=N` | Recorded events after the first `N` (default 0); a branch's events are numbered from 1 | `{"events": [RecordedEvent], "cursor": M}`; pass `M` next |
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
  "budget": { "max_usd": 2.0, "max_turns": 3, "max_seconds": 1200 },
  "policy": { "mode": "deny", "rules": [{ "tool": "Read", "allow": true }, { "tool": "mcp__*", "allow": false }] },
  "check": ["cargo", "test"],
  "isolated": false,
  "command": ["/opt/claude/bin/claude"]
}
```

Only `prompt` is required. Give `harness` for one branch or `harnesses` for one branch each (`<name>-<harness>`), not both. `policy.mode` is `allow` or `deny` (the default); rules apply first, in order. There is no remote `ask`: the server has no terminal to ask on. `budget.max_seconds` applies per call, like the SDK's `max_duration`. `command` is refused (`403 command_not_allowed`) unless the server allows client commands; without it, the server uses its `harness_commands` entry for the harness, else the profile's executable on its `PATH`. One task runs one command, so harnesses with different configured commands must be separate tasks.

`send` takes `prompt`, `budget`, `policy`, `check` and `command`; the branch keeps its harness, recorded command and environment. `fork` takes `prompt`, `name`, `fresh_session`, `harness`, `budget`, `policy`, `check`, `isolated` and `command`. `merge` takes an optional `target`, defaulting to the branch checked out in the served repository.

`cancel` takes an empty object, `{}`. It is not an operation: it records a durable cancel request for the branch's running turn and each running turn delegated below it, and answers `200` with the branches that were running (an empty list when none was, and for a repeat). The engine running each turn, in the server or in another process on the repository such as a local `by run`, observes the request within 100 ms, interrupts the harness, and ends the branch `interrupted`; the operation that ran the turn then succeeds with that status. The branch's log records `cancelled by <token name> through the server`. It ignores branch locks, since the branches it is for are the ones an operation holds, and needs no idempotency key.

### Operations

Task, send, fork and merge are long operations. The server validates the request, records the operation durably, and answers `202 Accepted` with `Location: /v1/operations/{id}` before the work starts:

```json
{ "id": "op_…", "repo": "app", "kind": "task", "state": "queued",
  "branches": ["flaky"], "cursor": 41, "created_at_ms": 1790000000000 }
```

`state` moves through `queued`, `running`, then `succeeded`, `failed` or `interrupted`. A finished operation adds `finished_at_ms`, `end_cursor`, and either `result` (`{"branches": [BranchInfo]}`, plus `{"merged": {"branch", "target", "previous", "commit"}}` for a merge) or `error` (`{"code", "message", "detail"}`). A branch that ends `failed` or over budget is a *succeeded* operation whose branch status says so, exactly as the SDK returns `Ok(branch)`; an operation fails when the SDK call returns an error, such as an unknown harness or a refused merge.

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
| `command_not_allowed` | 403 | A request `command` on a server that does not allow them |
| `idempotency_key_reused` | 422 | Same key, different request; `detail.operation` |
| `branch_busy` | 409 | Another operation holds the branch; `detail.holder` |
| `cursor_out_of_range` | 400 | Stream cursor past the feed's end; `detail.head` |
| `detached_head` | 409 | Merge without a target while the served repository's HEAD is detached |
| `shutting_down` | 503 | The server is stopping |
| `interrupted` | (operation) | The server stopped before the operation finished |
| `internal`, `git_error`, `io_error`, `state_error`, `harness_error`, `not_a_repository` | 500 | Server-side failure |
| `branch_exists`, `no_candidate`, `target_moved`, `conflict`, `dirty_target`, `already_merged`, `running`, `fenced` | 409 | SDK refusals; `target_moved` has `detail.expected`/`actual`, `conflict` has `detail.files`. `running`: another engine, such as a local `by`, runs a turn on the branch. `fenced`: the engine lost the branch's lease to another |
| `invalid_name`, `unknown_harness` | 400 | SDK refusals |
| `harness_unavailable`, `unsupported`, `check_failed`, `check_timed_out`, `check_not_started`, `invalid_candidate` | 422 | SDK refusals; check failures have `detail.output_tail` |

## Remote mode in `by`

```sh
export BRANCHYARD_REMOTE=https://by.example:8421
export BRANCHYARD_TOKEN_FILE=~/.config/branchyard/token
by --repo app run "Make the flaky parser test deterministic" --check "cargo test" --yes
by fan "..." --harness claude-code,codex --yes
by ls; by diff flaky; by merge flaky; by watch; by cancel flaky
```

Global options go before the command: `--remote URL`, `--token-file FILE`, `--repo NAME` (needed when the server serves several), `--ca-file FILE` (extra trust for `https`). Each has an environment variable: `BRANCHYARD_REMOTE`, `BRANCHYARD_TOKEN_FILE`, `BRANCHYARD_REPO`, `BRANCHYARD_CA_FILE`. Output is the local output: the CLI renders the same SDK values with the same code, and a test compares each command's output, `watch` aside, local against remote. Differences: `--ask` is refused (pass `--yes`, or leave requests denied); without `--yes` requests are denied; worktree paths are the server's; `by harnesses` shows the server's `PATH`. Interrupting `by run` stops watching, not the work; `by cancel` stops the work.

## Deployment notes

- **One server per data directory** is enforced with an advisory lock on `DATA-DIR/lock`; a second server on the directory fails at start. On a network file system it is only as good as that file system's `flock`.
- **TLS.** Terminate TLS in the server (`--tls-cert`/`--tls-key`, rustls with the ring provider, HTTP/1.1) or in a reverse proxy in front of a loopback bind. A proxy must not buffer `text/event-stream` responses. Clients trust the Mozilla roots plus `--ca-file`.
- **Harnesses** run as the server's user, with its `PATH`, `HOME` and harness logins. Install and log in the harnesses as that user, or set `harness_commands`.
- **Limits.** Request bodies are bounded (`max_body_bytes`, default 1 MiB), request heads must arrive within 30 seconds, at most 256 requests are handled at once (more wait), and at most `max_running` operations run at once (more queue). There is no per-request deadline beyond those, and no rate limiting per token.
- **Health.** `GET /healthz` needs no token. Each request is logged to stderr as `request-id method path status duration`, never with headers or bodies.
- **Backups.** Branch state and event logs are each repository's `.branchyard/state.db`, with worktrees under `.branchyard/worktrees/`; server state is `DATA-DIR/state.db` (operations, idempotency keys). Both are SQLite databases in write-ahead-log mode: back them up with `sqlite3 FILE ".backup COPY"` or while the server is stopped, not by copying the file alone. Both grow without bound for now. An `operations.jsonl` from an earlier version is imported on first start and renamed `operations.jsonl.imported`; `DATA-DIR/feeds/` is no longer used.

## What is durable

| State | Where | Survives a restart |
|---|---|---|
| Branches, candidates, event logs, the activity feed | Each repository's `.branchyard/state.db`, written by the engine in transactions | Yes |
| Operations and idempotency keys | `DATA-DIR/state.db`, committed with `synchronous=FULL` before `202` and at each state change | Yes; unfinished ones become `interrupted` |
| Cancel requests, `max_duration` deadlines, turn leases, journaled steps, harness process identities | Each repository's `.branchyard/state.db` | Yes |
| Branch locks | Memory | No; they end with the operations |
| A turn in progress | The server process and its harness child | No: it is recovered, not continued |

If the server dies mid-turn, the operation becomes `interrupted` at the next start, and opening the repository recovers the branch: the engine kills the harness's process group if its pid and start time still match, ends the branch `interrupted` with a `recovered` event that says whether the prompt had been submitted, and never submits it again. The server also recovers every 30 seconds, which covers a local `by run` on a served repository that was killed. [Durability](durability.md) describes the leases, the journal and the recovery rules, and what is not guaranteed.

Not durable yet:

- **PostgreSQL.** The design's store is PostgreSQL with PGMQ (see [design §8](design.md#8-durable-execution-using-existing-queues)): accepted operations and their commands committed in one transaction, delivered through a queue with reconciliation. The engine's `Backend` trait and the registry's `OperationStore` trait in [`store.rs`](../crates/branchyard-server/src/store.rs) (`load`, and a `save` that must be durable before returning) are the seams; [durability](durability.md#postgresql) maps each operation onto PostgreSQL. Multiple replicas need that store, with branch locks moved into the database as well.

## Security

- **No isolation, even remotely.** The server runs harnesses with the local process provider: as the server's operating-system user, with its environment, home and credentials. A remote caller with a token and `--yes` (policy `allow`) can make a harness do anything that user can. Serving a repository is granting its token holders that user's authority. Run the server as a dedicated, unprivileged user on a machine you are willing to hand over; qualified sandbox providers (design §4, M2) are what change this.
- **`allow_client_commands`** additionally lets any token holder choose the executable the server launches. Leave it off outside tests; configure `harness_commands` instead.
- **Tokens are all-powerful and equal.** No scopes, no per-repository access, no expiry or rotation beyond editing the configuration and restarting. Treat each as the server user's password.
- **Plain HTTP** exposes tokens, prompts and code to anyone on the path. The server refuses it off loopback unless told `--insecure-bind`.
- **Information exposure.** Responses include server paths (worktrees, repository roots) and the server's harness availability.
- **Policy is per request.** A `deny` default is safe; rules match tool names only, as in the SDK.

## Code

| Crate | Contents |
|---|---|
| [`branchyard-server`](../crates/branchyard-server/src/lib.rs) | Configuration, authentication, the operation registry and its store, the activity feed, routes, TLS and shutdown |
| [`branchyard-client`](../crates/branchyard-client/src/lib.rs) | Wire types, a blocking HTTP/1.1 client over rustls, an SSE parser, and a reconnecting event stream |
| [`branchyard-cli`](../crates/branchyard-cli/src/main.rs) | `by serve`, remote mode and `by watch` |
