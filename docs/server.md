# Branchyard server

`branchyard-server` (also `by serve`) serves one or more repositories, each under a name, over an authenticated HTTP JSON API with Server-Sent Events. It runs the local-mode engine in-process: the same tasks, branches, event logs, budgets, policies and validated merges as `by`, reached over the network. `by --remote URL` runs every `by` command against it, and [`branchyard-client`](../crates/branchyard-client/src/lib.rs) is the typed Rust client.

> **Status.** Tested hermetically over real HTTP on loopback against a fake ACP agent, and on PostgreSQL 16 for the database store. Not yet run against a real harness through the server, not load-tested, and not deployed. The default provider is the local process provider: **no isolation** (see [Security](#security)). The server's operator can allow the Microsandbox and Substrate providers, delegation and unapproved tools; [surfaces](surfaces.md) lists what works on which surface.

## Running it

```sh
cd path/to/repo
by serve                      # serves this repository as its directory name on 127.0.0.1:8421
# branchyard-server: created a token in .branchyard/server/token; clients pass --token-file with it

by --remote http://127.0.0.1:8421 --token-file .branchyard/server/token ls

branchyard-server token new --tenant acme --scopes read,run --repo app
# branchyard-server: token (printed once; give it to the client, never store it): <token>
# {"token_sha256": "…", "tenant": "acme", "name": "token-…", "scopes": ["read","run"], "repos": ["app"]}
# paste that object into the configuration's "credentials" array
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
| `--database URL` | Keep branch state, operations, their queue and branch locks in PostgreSQL (`postgres://user@host/db`) instead of SQLite; several servers may share it. Needs a build with the `postgres` feature; see [PostgreSQL](#postgresql) |
| `--worker` | Only run operations queued in `--database`: no listener, no webhooks. `by worker` is `by serve --worker`; see [several servers](#several-servers-on-one-database) |
| `--max-artifact-bytes N` | Largest artifact a `POST .../artifacts` upload may publish, in bytes. Default 268435456 (256 MiB) |
| `--max-running N` | Operations this process runs at once; more wait queued. Default 8 |
| `--operation-lease SECS` | How long a claim on a queued operation lasts without renewal before another worker takes it over; renewed every third of it. Default 30 |
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
    { "name": "alice", "token": "at-least-16-characters" },
    { "name": "acme-ci", "token_file": "/etc/branchyard/acme-ci.token",
      "tenant": "acme", "scopes": ["read", "run"], "repos": ["app"] }
  ],
  "credentials": [
    { "token_sha256": "…64 lowercase hex characters, from `branchyard-server token new`…",
      "tenant": "acme", "name": "acme-readonly", "scopes": ["read"] }
  ],
  "tenants": {
    "acme": { "repos": ["app"], "max_running": 4, "max_branches": 20,
              "max_cost_usd": 50.0, "max_artifact_bytes": 1073741824 }
  },
  "tls": { "cert": "/etc/branchyard/cert.pem", "key": "/etc/branchyard/key.pem" },
  "max_body_bytes": 1048576,
  "max_artifact_bytes": 268435456,
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

The server refuses to start with no repository, no token or credential, a token shorter than 16 characters, an unknown provider or scope name, a malformed or duplicated `credentials` hash, a `by_path` that is not a file, a `database` that is not a `postgres://` URL, or a plain-HTTP bind to anything but loopback without `--insecure-bind`. It warns when a token file, or a configuration holding inline tokens or a database password, is readable by other users. A password in `--database` is visible to other users of the host in its process list; prefer the configuration file, mode 600.

SIGINT or SIGTERM starts a graceful shutdown: no new connections or operations (`503 shutting_down`), no more operations claimed from the queue, event streams end, requests in flight finish, and running operations get the grace period. Operations still running after it are recorded as `interrupted`; queued ones stay queued, and run after the restart or on another server sharing the database. A second signal stops waiting at once.

## Identity and scopes

Every route except `GET /healthz` needs `Authorization: Bearer <token>`, including unknown routes, so routes cannot be probed anonymously. The presented token is **hashed (SHA-256) and compared against every configured credential's hash** in time that depends only on lengths; the verifier holds hashes, never a plaintext token, and neither tokens nor headers are ever logged. A missing or wrong token gets `401 unauthorized` with `WWW-Authenticate: Bearer`.

**A request's identity comes only from its verified credential, never from anything the request itself says** — there is no `tenant_id` field anywhere in the wire protocol. Each configured credential names a **principal**: a `tenant` (1 to 128 characters, no `/`), a subject `name` (the caller's identity for idempotency scoping and audit), a set of **scopes**, and, optionally, its own repository allowlist narrower than its tenant's. Two ways to configure one:

- **`tokens`** (unchanged from before tenants existed): `{"name", "token"|"token_file"}`, plus optional `tenant`, `scopes` and `repos`. A `tokens` entry that gives none of those three becomes a principal in the unconfigured `default` tenant with every scope and every repository — **exactly what a single-token server did before this existed**, so old configurations keep working unchanged.
- **`credentials`**: `{"token_sha256", "tenant", "name"?, "scopes"?, "repos"?}` — the token's SHA-256 directly, so the plaintext never has to enter the configuration at all. `branchyard-server token new [--tenant T] [--scopes S,...] [--repo R,...]` (also `by serve token new`) generates a fresh token, prints it once (give it to the client, never store it), and prints the `credentials` object to paste in. There is no hot rotation or revocation API: replacing or removing a hash and restarting is how a token is rotated or revoked, the same restart a `tokens` change already needed.

Scopes: `read` (every `GET`, including the event stream), `run` (submitting a task, send, fork, reincarnate or spawn, and acting on a running turn: cancel, steer, ask, report, escalate, answer), `merge` (merge, integrate), `admin` (removing a branch). Every endpoint checks the caller's scope and returns `403 scope_required` (`detail.scope`) when it is missing.

**Repositories belong to tenants.** A tenant's `repos` (in `tenants`) is the simplest sound model for isolation: it is the tenant's repository allowlist, and a principal's own `repos`, if given, only narrows it further (the intersection). `GET /v1/repos` lists only what the caller's tenant (and its own allowlist) can see; acting on any other repository is `403 repo_not_allowed` (`detail.repo`). `GET /v1/operations/{id}` refuses another tenant's operation with `404 unknown_operation`, indistinguishable from an ID that never existed, so a principal cannot even tell that another tenant's operation exists. This is the whole isolation model: there is no per-branch or per-operation ownership beyond the repository it is in, since a tenant that cannot reach a repository cannot reach anything inside it either. (Operations record the admitting principal — tenant, subject, scopes and repositories — for this check, for `GET /v1/operations?idempotency_key=`, and for the worker that runs them; branches and feed entries do not carry a principal field of their own in this release — an out-of-scope decision documented here rather than left implicit — since the repository-ownership boundary above already makes them unreachable across tenants without it.)

## Quotas

A tenant's `tenants.<name>` entry sets its resource ceilings, all optional (unconfigured means unlimited):

| Field | Enforces | Checked | Refusal |
|---|---|---|---|
| `max_running` | Operations of this tenant queued or running at once | In the admission's transaction, against the operation store | `429 quota_exceeded` (`detail.limit = "max_running"`) |
| `max_branches` | Branches across this tenant's repositories, counting those its queued and running operations will create | In the admission's transaction of a task, fork, reincarnate or spawn | `429 quota_exceeded` (`"max_branches"`) |
| `max_cost_usd` | Total `cost_usd` recorded across this tenant's currently-open branches (lifetime, not a rolling window — see below) | Live, before a task/fork/reincarnate/spawn is admitted | `429 quota_exceeded` (`"max_cost_usd"`) |
| `max_artifact_bytes` | Total artifact bytes reachable across this tenant's repositories | Same as `max_cost_usd` | `429 quota_exceeded` (`"max_artifact_bytes"`) |

A refusal's `detail` has `tenant`, `limit`, `max` and `reserved` (or `spent`). A refused admission writes nothing: no operation record, no idempotency binding, no branch lock, no queue row, so a retry with the same key once the tenant is under its ceiling is admitted normally.

**`max_running` is transactional, and holds across servers and restarts.** Every operation's record carries its tenant (`tenant`, with the admitting `principal`). [Admission](#dispatch) counts the tenant's rows in the queue — its queued and running operations, joined to their records — inside the same transaction that writes the new operation, and refuses when that count is at the ceiling. On SQLite that transaction is `BEGIN IMMEDIATE`, so admissions take turns; on PostgreSQL an admission of a tenant with a quota first takes a transaction-scoped advisory lock on the tenant (`pg_advisory_xact_lock(hashtextextended('branchyard tenant ' || tenant, 0))`), so admissions of one tenant take turns on every server sharing the schema while other tenants' do not wait. The count is the database's, not a process's, so two servers on one database cannot both admit a tenant's last slot, and a restarted server sees the same count. Release is implicit: the transaction that records an operation's outcome deletes its queue row, so it stops counting the moment it is `succeeded`, `failed` or `interrupted`. A queued operation survives a restart and keeps its slot until it runs and finishes; one running at shutdown is recorded `interrupted` and frees its slot. Merges and integrations count toward `max_running` but are not refused by it. Servers sharing a database should configure the same `tenants`: each admission checks the ceiling of the server that admits it, against the shared count.

**`max_branches` is transactional too, with one exception.** Admission records the branch names an operation will create (`creates`: the planned names that `branches` reports), and a task, fork, reincarnate or spawn is refused when the tenant's branches would exceed the ceiling: the set of branches that exist in its repositories together with those its queued and running operations will create, each counted once, plus this operation's new ones. Both are read inside the admission's transaction — first the tenant's queued and running operations, then the branches that exist — so an operation that finishes in between is counted by one or the other, never neither, and a concurrent admission of the same tenant waits its turn. What is still best-effort:

- **Branches delegated from inside a running turn** (a harness's `by spawn`, or the delegation tools) are not reserved at admission; they count once they exist, so delegation within a running operation can take a tenant past `max_branches`, bounded only by the operation's delegation envelope. `POST …/spawn` through the server is reserved like any other operation.
- **Branches created outside the server** (a local `by` on a served repository) count once they exist.
- **Operations admitted by an earlier version** carry no `creates`; their branches count once they exist.

Removing a branch (`DELETE .../branches/{b}`, needing `admin`) frees its `max_branches` and `max_artifact_bytes` reservation immediately.

`max_cost_usd` and `max_artifact_bytes` stay checked **live** against data the engine keeps durably — a tenant's open branches, their `cost_usd`, and their reachable artifacts — before a branch-creating operation is admitted, so they are exact across a restart with nothing extra to persist, but best-effort: they are not taken in the admission's transaction, two requests racing past the same near-limit tenant can both be admitted, and spend accrues while turns run. `max_cost_usd` is lifetime spend across a tenant's currently-open branches, not a rolling time window — the engine does not otherwise keep a timestamped cost ledger, and adding one only for a windowed quota was judged not worth the added durable state for this release.

**Workers act as the admitting principal.** A queued operation's record holds the principal that admitted it (tenant, subject, scopes, repository allowlist), as its credential verified. The worker that runs it — the admitting server, another server on the database, or a `by worker` process, which needs no credential of its own — runs it as that principal: before running it, the worker checks that the principal holds the operation's scope and that its tenant, under the worker's own `tenants` configuration, owns the repository, and otherwise fails the operation with `403 scope_required` or `403 repo_not_allowed`, the error the request would have got there. Running another tenant's operation reveals nothing to anyone: the operation stays visible only to its own tenant.

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
| `POST /v1/repos/{repo}/branches/{b}/steer` | Add input to its running turn without interrupting it | `Steer`: `{id, branch, by, text, requested_at_ms, state}` |
| `DELETE /v1/repos/{repo}/branches/{b}` | Remove worktree and record | `{"removed": "b"}` |
| `GET /v1/repos/{repo}/branches/{b}/diff` | Candidate diff against the base | `{"diff": "..."}` |
| `GET /v1/repos/{repo}/branches/{b}/events?cursor=N` | Recorded events after the first `N` (default 0); a branch's events are numbered from 1 | `{"events": [RecordedEvent], "cursor": M}`; pass `M` next |
| `POST /v1/repos/{repo}/branches/{b}/spawn` | A child of `b`, as `by spawn --parent b` makes one; needs `--allow-delegation` | `202` operation |
| `POST /v1/repos/{repo}/branches/{b}/integrate` | Merge delegated child `b` into the branch that delegated it, as `by integrate b` | `202` operation |
| `GET /v1/repos/{repo}/branches/{b}/inspection` | `b` as a delegating parent sees it, as `by inspect b --json` | `Inspection` |
| `GET /v1/repos/{repo}/branches/{b}/event-page?cursor=N&limit=M` | Up to `M` (default 50, at most 200) events after the first `N`, or the most recent without a cursor, as `by events b --json` | `EventPage` |
| `GET /v1/repos/{repo}/branches/{b}/children` | Every branch `b` delegated to, directly or below, as `by children b --json` | `Children` |
| `GET /v1/repos/{repo}/branches/{b}/graph` | `b`'s children, the dependencies among them and its graph revision, as `by graph show b --json` ([task graphs](graph.md)) | `Graph` |
| `POST /v1/repos/{repo}/branches/{b}/graph` | Apply a graph proposal `{expected_revision, edits, policy?, unapproved_tools?}` to `b`'s children, as `by graph apply --parent b`; a proposal that spawns needs `--allow-delegation`; a stale revision is `409 stale_revision` with `{expected, actual}` in `detail` | `200` `GraphApplied` |
| `GET /v1/repos/{repo}/branches/{b}/inbox` | Every message addressed to `b`, oldest first, as `by inbox --as b --json` | `Inbox` |
| `POST /v1/repos/{repo}/branches/{b}/ask` | Ask `b`'s parent a question, as `by ask --as b`; `{text, wait_seconds}`, the latter optional and capped at 120s on the server | `Asked` |
| `POST /v1/repos/{repo}/branches/{b}/report` | Report to `b`'s parent, as `by report --as b`; `{text}` | `Message` |
| `POST /v1/repos/{repo}/branches/{b}/escalate` | Escalate to `b`'s parent, or further up if its rig seat allows, as `by escalate --as b`; `{text}` | `Message` |
| `POST /v1/repos/{repo}/branches/{b}/answer` | Answer one of `b`'s own descendants' messages, as `by answer --as b`; `{message_id, text}` | `Message` |
| `GET /v1/repos/{repo}/events/stream?cursor=N` | SSE of activity across branches after feed position `N`; without a cursor, from now | `text/event-stream` |
| `GET /v1/operations/{id}` | An operation's status and result, on any server sharing the operation store | operation |
| `GET /v1/operations?idempotency_key=K` | The operation the caller's request with key `K` created, for a client that lost the response | operation, or `404 unknown_operation` |
| `POST /v1/repos/{repo}/branches/{b}/artifacts` | Publish the body's bytes as a new artifact of `b`, acting with the server's authority as a person, like `by artifact publish --branch`. `name`, `media_type`, repeated `label` are query parameters; refused over `--max-artifact-bytes` | `201` `ArtifactRef` |
| `GET /v1/repos/{repo}/branches/{b}/artifacts` | Every artifact `b` may read | `{"artifacts": [ArtifactRef]}` |
| `GET /v1/repos/{repo}/branches/{b}/artifacts/{id}` | Artifact `id`'s provenance, checked against `b`'s grant | `ArtifactRef` |
| `GET /v1/repos/{repo}/branches/{b}/artifacts/{id}/content` | Its bytes, with a digest header | streamed bytes |
| `POST /v1/repos/{repo}/branches/{b}/artifacts/{id}/share` | Share it with `{"to": "BRANCH"}` | `{"ok": true}` |
| `POST /v1/repos/{repo}/branches/{b}/scratch` | Create `{"name": "NAME"}`, owned by `b` | `ScratchArea` |
| `GET /v1/repos/{repo}/branches/{b}/scratch` | Every scratch area `b` may reach | `{"areas": [ScratchArea]}` |
| `POST /v1/repos/{repo}/branches/{b}/scratch/{name}/share` | Share it with `{"to": "BRANCH"}` | `{"ok": true}` |
| `POST /v1/repos/{repo}/branches/{b}/scratch/{name}/lock` | Acquire its writer lock for `b` | `ScratchLock`, or `409 running` |
| `POST /v1/repos/{repo}/branches/{b}/scratch/{name}/unlock` | Release it if `b` holds it | `{"ok": true}` |
| `GET /v1/repos/{repo}/scratch/{name}/lock` | Its current holder, if any (not access controlled) | `{"lock": ScratchLock?}` |

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

`spawn` takes `prompt`, `harness`, `name`, `base`, `budget`, `policy`, `check`, `max_depth`, `deny`, `unapproved_tools`, `seat`, `depends_on`, `after` and `bindings`, the flags of `by spawn`; with a `seat` and no `name`, the child is named `<parent>-<seat>` before the operation starts. It acts with the server's authority as a person, as `by spawn --parent` does locally: the parent's envelope bounds the child exactly as it bounds a local spawn, and a parent without delegation cannot spawn. The operation, queued like every other with the whole request as its description, runs the child's first turn, waits for the parent's subtree on its worker as the local command does, and its result holds the child's `inspection`; a child with `depends_on` that has not settled is created `waiting`, the operation finishes with it `waiting`, and it starts later where its prerequisite settles or is integrated. It locks the parent as well as the child, as `integrate` locks both branches, so a send to the parent or its removal is refused with `409 branch_busy` until the operation finishes. `integrate` takes `{}` and refuses (`403 denied`) a branch no other branch delegated; its result holds `merged`. `POST …/graph` is not an operation: it commits the proposal in one store transaction before it answers, starts the children that have nothing to wait for on the server's threads, and returns; their siblings' turns start the rest as they settle, under the proposal's `policy`, and the recovery interval of every server and `by worker` on the store starts any whose prerequisite settled while no engine could (after a restart, under the default policy, deny); with several on one database the store's claim lets one of them start it ([task graphs](graph.md#with-the-servers-queue)). A retry of a request that committed is refused `409 stale_revision`, so it never applies twice. The inspection, event page and children reads act with the same authority and need no opt-in. So do the messaging routes: `inbox` reads `b`'s own inbox; `ask`, `report`, `escalate` and `answer` act as `b` with the server's authority, following the same delegation-tree rules as locally ([delegation](delegation.md#inbox)), and need no opt-in either, since none of them runs a turn. `ask`'s `wait_seconds` blocks the request handler for up to that long (capped at 120s server-side; poll `inbox` for longer) using the same store wait a local `ask --wait` does.

`cancel` takes an empty object, `{}`. It is not an operation: it records a durable cancel request for the branch's running turn and each running turn delegated below it, and answers `200` with the branches that were running (an empty list when none was, and for a repeat). The engine running each turn, in the server or in another process on the repository such as a local `by run`, observes the request within 100 ms, interrupts the harness, and ends the branch `interrupted`; the operation that ran the turn then succeeds with that status. The branch's log records `cancelled by <token name> through the server`. It ignores branch locks, since the branches it is for are the ones an operation holds, and needs no idempotency key.

`steer` takes `{"text": "..."}`. Like `cancel`, it is not an operation and ignores branch locks: the running turn's operation holds them, and the input is for exactly that turn. It queues the input durably, bound to the branch's running turn ([durability](durability.md#steered-input)); the engine running that turn, in the server or in another process on the repository, writes it to the harness within 100 ms through the harness's own mid-turn input ([which harnesses, and when the model sees it](harness-integration.md#steering-a-running-turn)). The answer waits up to 10 seconds for that and returns the `Steer`, whose `state` is `{"state": "delivered" | "accepted" | "pending"}` or `{"state": "refused", "reason": "..."}`. It is refused with `409 not_running` when no turn runs, and `422 unsupported`, with the reason, when the branch's harness cannot take input mid-turn; it never interrupts the turn instead. The branch's log records `steered` with `by` `<token name> through the server`. Budgets and permissions are the running turn's own. `by --remote send --steer` calls it.

### Operations

Task, send, fork, reincarnate, merge, spawn and integrate are long operations. The server validates the request, admits the operation durably (see [dispatch](#dispatch)), and answers `202 Accepted` with `Location: /v1/operations/{id}` before the work starts:

```json
{ "id": "op_…", "repo": "app", "kind": "task", "state": "queued",
  "branches": ["flaky"], "cursor": 41, "created_at_ms": 1790000000000 }
```

`kind` is `task`, `send`, `fork`, `reincarnate`, `merge`, `spawn` or `integrate`. `state` moves through `queued`, `running`, then `succeeded`, `failed` or `interrupted`. A finished operation adds `finished_at_ms`, `end_cursor`, and either `result` or `error` (`{"code", "message", "detail"}`). `result` has `branches` (`[BranchInfo]`); `merged` (`{"branch", "target", "previous", "commit"}`) for a merge or integration; `descendants` (`[BranchInfo]`), every branch the operation's branches delegated to, once they finished, since a task, send or fork waits for its subtree as `by run` does; and `inspection` for a spawn. A branch that ends `failed` or over budget is a *succeeded* operation whose branch status says so, exactly as the SDK returns `Ok(branch)`; an operation fails when the SDK call returns an error, such as an unknown harness or a refused merge.

`branches` are the names planned at acceptance. `cursor` is the feed position at acceptance and `end_cursor` the position once the operation's activity was ingested: to watch one operation, stream from `cursor` until the operation finishes and the stream reaches `end_cursor`, keeping entries for its branches. That is what `by --remote … run` does.

From admission until it finishes, the branches an operation works on are locked: another send, merge, fork into the same name, or removal gets `409 branch_busy`. The locks are rows in the operation store, taken in the admission's transaction and released in the one that records the outcome, so they hold across every server sharing the store; a removal holds its branch the same way for as long as it takes (at most 10 minutes if its server dies meanwhile). A local `by` on the same repository does not see them.

### Dispatch

Admission is a durable enqueue. In one transaction the server writes the operation's record (with its tenant and admitting principal), its idempotency binding (a unique index on the caller and key), checks the tenant's `max_running` and `max_branches` against the tenant's queued and running operations ([quotas](#quotas)), takes its branch locks, and writes a queue row holding a description of the work: the request as sent, plus what admission fixed (planned names, a merge's target, an integration's parent, a seat child's name). Only then does it answer `202`. If a quota refuses it, a branch is held, or any of those writes fails, including the queue write, the transaction rolls back and nothing of the operation remains: the client gets `500` and may retry with the same key. The description holds no secret values; a request names secrets, and the server that runs it reads them, like commands, providers and policies, from its own configuration.

A dispatcher in each server claims queue rows oldest first, for the repositories it serves, up to `--max-running` at once. A claim carries a lease, renewed every third of `--operation-lease`, and a fence, its attempt number, which every later write for the operation names. The worker records the operation `running` before it calls the engine, and its outcome after, deleting the queue row and releasing the branch locks in the same transaction; a worker whose claim was taken over is refused both writes. A claim whose lease expired, or whose process is gone from this host, is claimed again by any worker: an operation still `queued` then runs, and one recorded `running` is recorded `interrupted` and never run again, since its turn may have started, and the engine recovers that turn's branch as it recovers any whose engine died.

### Idempotency

Send `Idempotency-Key: <1–255 visible ASCII characters>` on any `POST`. The key is scoped to the caller — its principal's subject name, prefixed with its tenant (`tenant/name`) outside the `default` tenant, so two tenants' principals of one name never share a key — and fingerprinted with the route and the canonical request. A repeat with the same request returns the original operation, `200` if it has finished (with its result) or `202` if not, with `Idempotent-Replayed: true`, and never starts a second run; this holds across restarts and across servers sharing the operation store, since the key is bound in the admission's transaction. Two concurrent requests with one key meet at the store's unique index: the second waits for the first to commit, then replays it. The same key with a different request gets `422 idempotency_key_reused`. `GET /v1/operations?idempotency_key=` finds only the caller's own tenant's operation. Keys are kept for as long as the registry is. `branchyard-client` and `by` send a fresh key per command and retry a `POST` with the same key after a connection failure; a client that lost the response can also look the operation up with `GET /v1/operations?idempotency_key=` (`Client::operation_by_key`).

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
| `unknown_repo`, `unknown_branch`, `unknown_operation` | 404 | No such thing, or another tenant's (indistinguishable from unknown) |
| `scope_required` | 403 | The caller's principal lacks a scope this endpoint needs; `detail.scope` |
| `repo_not_allowed` | 403 | The repository is outside the caller's tenant, or its own narrower allowlist; `detail.repo` |
| `quota_exceeded` | 429 | A tenant quota (`docs/server.md#quotas`) is at its configured limit; `detail.tenant`, `detail.limit`, `detail.max`, and `detail.reserved` or `detail.spent` |
| `invalid_request` | 400 | Malformed JSON, unknown field, empty prompt, bad budget, bad cursor |
| `unsupported_media_type` | 415 | `POST` body not declared as JSON |
| `body_too_large` | 413 | Body over `max_body_bytes`, or an artifact upload over `max_artifact_bytes`; `detail.limit` |
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
| `interrupted` | (operation) | The server stopped before the operation finished, or its worker's claim expired after it started |
| `internal`, `git_error`, `io_error`, `state_error`, `harness_error`, `not_a_repository` | 500 | Server-side failure |
| `branch_exists`, `no_candidate`, `target_moved`, `conflict`, `dirty_target`, `already_merged`, `running`, `fenced` | 409 | SDK refusals; `target_moved` has `detail.expected`/`actual`, `conflict` has `detail.files`. `running`: another engine, such as a local `by`, runs a turn on the branch. `fenced`: the engine lost the branch's lease to another |
| `invalid_name`, `unknown_harness` | 400 | SDK refusals |
| `denied` | 403 | The envelope or authority refused a delegation operation, as `by --json` reports `denied` |
| `remote_error` | 502 | An error the engine running a delegating turn returned through its broker; `detail.kind` is its kind |
| `harness_unavailable`, `unsupported`, `check_failed`, `check_timed_out`, `check_not_started`, `invalid_candidate` | 422 | SDK refusals; check failures have `detail.output_tail` |

## Published schema

[`schema/contract.json`](../schema/contract.json) is the JSON Schema for every request and response type above (`crate::api` and `crate::storage_api` in `branchyard-client`), including `GraphRequest` and the `Graph` and `GraphApplied` answers of `…/graph` (their `GraphEdit`, `SpawnSpec`, `Dependency`, `After` and `Binding`, and `BranchStatus`'s `waiting` and `blocked` states), generated from their Rust definitions with [`schemars`](https://docs.rs/schemars) rather than hand-maintained: `#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]` on the wire types (and the branch, delegation and provisioning types they reference), a `schema` feature per crate that pulls it in, and `cargo run -p branchyard-client --features schema --example generate_contract` to print it. `crates/branchyard-client/tests/contract.rs` regenerates it in-process and diffs against the checked-in file, so a changed wire type without a regenerated schema fails `cargo test -p branchyard-client --features schema` in CI. `tests/test_schema_validation.py` validates this document's own worked examples (the ones in this page), and [task graphs](graph.md)' proposal example with a `Graph` holding `waiting` and `blocked` children, against it with a small stdlib-only structural validator, as a sanity check on the schema itself.

## Remote mode in `by`

```sh
export BRANCHYARD_REMOTE=https://by.example:8421
export BRANCHYARD_TOKEN_FILE=~/.config/branchyard/token
by --repo app run "Make the flaky parser test deterministic" --check "cargo test" --yes
by fan "..." --harness claude-code,codex --yes
by ls; by diff flaky; by merge flaky; by watch; by send flaky --steer "also run the linter"; by cancel flaky
```

Global options go before the command: `--remote URL`, `--token-file FILE`, `--repo NAME` (needed when the server serves several), `--ca-file FILE` (extra trust for `https`). Each has an environment variable: `BRANCHYARD_REMOTE`, `BRANCHYARD_TOKEN_FILE`, `BRANCHYARD_REPO`, `BRANCHYARD_CA_FILE`. Output is the local output: the CLI renders the same SDK values with the same code, and tests compare each command's output, `watch` aside, local against remote, including the `--json` output of `spawn` (with and without `--seat`), `inspect`, `events`, `children`, `graph show`, `graph apply`, `integrate`, `send`, `rig run`, `ask`, `report`, `escalate`, `answer` and `inbox`.

Every command runs remotely, with the same flags: `--provider` and its options, `--delegate`, `--allow-delegation` and `--allow-unapproved-tools` are sent in the request, and the server refuses what its operator did not allow. `spawn` needs `--parent`, as outside a harness locally, and the child runs on the server. Differences: `--ask` is refused (pass `--yes`, or leave requests denied); without `--yes` requests are denied; worktree paths, `--substrate-key` (which must be absolute) and `--pass-env` values are the server's; `by harnesses` shows the server's `PATH`. With `--json`, a server error prints `{"error": {"kind", "message"}}` with the kind local `by` reports (the server's `git_error` is `git`, and so on), and a server that cannot be reached is `unavailable`. Interrupting `by run` stops watching, not the work; `by cancel` stops the work. `by run --delegate` follows the activity of the children the branch spawns, and prints the `delegated` table once they have finished on the server.

## Herdr

[`plugins/herdr`](../plugins/herdr/README.md) is a [Herdr](https://github.com/herdrdev/herdr) plugin built on this API and `by`. Its bridge opens the event stream, lists the branches, and applies the entries after the stream's starting cursor ([`EventStream::open`](../crates/branchyard-client/src/lib.rs)); it keeps one Herdr tab per branch running `by log --follow <branch>`, and reports each branch's state on it with `herdr pane report-agent --source custom:branchyard` (`running` is `working`, a waiting permission request `blocked`, every settled status `idle` with what happened as the message). When the stream drops it reconnects after the last cursor it applied. Herdr actions run `by merge`, `by cancel` and `by send` for the branch in the focused pane. It uses the same `BRANCHYARD_*` settings as `by --remote`. It is tested against a fake `herdr`, not yet against Herdr itself.

`by log --follow` works in both modes: locally it waits on the store; remotely it polls the branch's events by cursor every half second and keeps retrying while the server is unreachable.

## Deployment notes

- **One server per data directory** is enforced with an advisory lock on `DATA-DIR/lock`; a second server on the directory fails at start. On a network file system it is only as good as that file system's `flock`. With `--database` there is no lock: the database holds operations, and several servers may share it.
- **TLS.** Terminate TLS in the server (`--tls-cert`/`--tls-key`, rustls with the ring provider, HTTP/1.1) or in a reverse proxy in front of a loopback bind. A proxy must not buffer `text/event-stream` responses. Clients trust the Mozilla roots plus `--ca-file`.
- **Harnesses** run as the server's user, with its `PATH`, `HOME` and harness logins. Install and log in the harnesses as that user, or set `harness_commands`.
- **Limits.** Request bodies are bounded (`max_body_bytes`, default 1 MiB), request heads must arrive within 30 seconds, at most 256 requests are handled at once (more wait), and at most `max_running` operations run at once (more queue). There is no per-request deadline beyond those, and no rate limiting per token.
- **Health.** `GET /healthz` needs no token. Each request is logged to stderr as `request-id method path status duration`, never with headers or bodies.
- **Backups.** Branch state and event logs are each repository's `.branchyard/state.db`, with worktrees under `.branchyard/worktrees/`; server state is `DATA-DIR/state.db` (operations, idempotency keys, the dispatch queue, branch locks). Both are SQLite databases in write-ahead-log mode: back them up with `sqlite3 FILE ".backup COPY"` or while the server is stopped, not by copying the file alone. With `--database`, both are in PostgreSQL instead: back it up with `pg_dump` of the schema. Either way they grow without bound for now. An `operations.jsonl` from an earlier version is imported on first start and renamed `operations.jsonl.imported`; `DATA-DIR/feeds/` is no longer used.
- **Providers and delegation.** Allow a provider only once its cluster, image or runtime is set up for this server (see [providers](providers.md)); the credentials a turn gets are the server's `pass_env` variables. Delegation runs children on the server's threads and gives harnesses the server's `by`, which must be the same version as the server.

## What is durable

| State | Where | Survives a restart |
|---|---|---|
| Branches, candidates, event logs, the activity feed | Each repository's `.branchyard/state.db`, or the database with `--database`, written by the engine in transactions | Yes |
| Operations (with their tenant and admitting principal), idempotency keys and the dispatch queue | `DATA-DIR/state.db` committed with `synchronous=FULL`, or the database's `by_operations` and `by_operation_queue` tables committed with `synchronous_commit = on`, in one transaction before `202`, and at each state change | Yes; queued ones run after the restart and keep counting toward their tenant's `max_running`, running ones become `interrupted` |
| Cancel requests, `max_duration` deadlines, turn leases, journaled steps, harness process identities | Each repository's `.branchyard/state.db`, or the database | Yes |
| Webhook delivery cursors | `DATA-DIR/state.db`'s `webhook_cursors` table, or the database's `by_webhook_cursors` | Yes |
| Branch locks | The same store's `branch_locks` (`by_branch_locks`), with the operation's admission and outcome | Yes, with their operations; a removal's expires after 10 minutes |
| A turn in progress | The server process and its harness child | No: it is recovered, not continued |

If the server dies before a worker starts an operation, the operation stays queued and runs once, after the restart or on another server. If it dies mid-turn, the operation becomes `interrupted` when its claim is next taken (at once when a server on the same data directory restarts, otherwise once the lease expires), and opening the repository recovers the branch: the engine kills the harness's process group if its pid and start time still match, ends the branch `interrupted` with a `recovered` event that says whether the prompt had been submitted, and never submits it again. The server, and each `by worker`, also recovers every 30 seconds (`Config::recover_interval`), which covers a local `by run` on a served repository that was killed, and on the same tick starts waiting dependents whose prerequisites settled while no engine could (`Yard::resume_graph`; [task graphs](graph.md#with-the-servers-queue)). [Durability](durability.md) describes the leases, the journal and the recovery rules, and what is not guaranteed.

## PostgreSQL

Build with the `postgres` feature (`cargo install --locked --path crates/branchyard-cli --features postgres`, or `-p branchyard-server --features postgres`), then:

```sh
by serve --database 'postgres://branchyard@db.internal/branchyard' --repo app=/srv/app
```

Each served repository's branch records, event log and feed, leases, journaled steps, harness processes, cancels and steered input are kept in the database under the repository's served name, with [the same semantics as SQLite](durability.md#postgresql); the operation registry is the `by_operations`, `by_operation_queue` and `by_branch_locks` tables. Tables are created when missing, in the connection's `search_path` schema: add `?options=-csearch_path%3Dname` to the URL to choose one. Worktrees, private homes and delegation tokens stay in each repository's `.branchyard/`, and the data directory still holds the default token (a `--worker`, which serves no requests, creates none).

### Several servers on one database

Servers, and `by worker` processes, started with the same `--database` share its operations, queue and branch locks:

```sh
by serve  --database "$DB" --repo app=/srv/app --listen 10.0.0.5:8421 --tls-cert … --tls-key …
by serve  --database "$DB" --repo app=/srv/app --listen 10.0.0.5:8422 --tls-cert … --tls-key …
by worker --database "$DB" --repo app=/srv/app --max-running 16
```

Any of them accepts an operation, any claims it, and each answers for every operation. A claim is a single `UPDATE … WHERE id = (SELECT … FOR UPDATE SKIP LOCKED LIMIT 1)` on `by_operation_queue`, so two workers never claim one row; leases are measured by the database's clock. An idle dispatcher looks for work every `poll_interval` (500 ms), and at once for what its own server admitted. The queue is plain tables so that it needs no extension; PGMQ could replace `by_operation_queue` without changing the admission transaction's shape.

Limits:

- **Same repositories, same checkouts.** Servers sharing a database must serve the same repositories under the same names, at paths that are the same checkout: the same host, or one shared file system. Worktrees and delegation tokens live in the checkout, and the engine's recovery kills a dead engine's harness only on its own host (elsewhere it waits for the turn's lease to expire).
- **Same configuration.** An operation runs with its worker's `harness_commands`, secrets, allow flags and `tenants`; a worker that would refuse the request fails the operation with the error the request would have got from it. It runs as the principal that admitted it, recorded with the operation, so a `by worker` needs no `tokens` or `credentials`. Quotas are counted in the shared database ([quotas](#quotas)); give every server the same `tenants`.
- **Webhooks** are delivered by every server that configures them, each from the shared cursor: configure them on one.
- **Throughput.** Each server uses one database connection for its registry; claims poll rather than `LISTEN`.
- **No automatic failover of a running turn.** A turn whose server died is recovered as interrupted, never resumed elsewhere.

What it is not yet:

- **Shared with local `by`.** A local `by` on a served repository opens its SQLite `state.db` and sees none of the server's branches; use `by --remote`. Nothing is imported from an existing `state.db` when a repository moves to the database.
- **TLS to the database.** Connections are plain; keep the database on a trusted network or a Unix socket.
- **Waits** poll every 100 ms, as SQLite's do across processes; nothing listens for `NOTIFY`.

## Security

- **No isolation by default, even remotely.** Unless a request names an allowed sandbox provider, the server runs harnesses with the local process provider: as the server's operating-system user, with its environment, home and credentials. A remote caller with a token and `--yes` (policy `allow`) can make a harness do anything that user can. Serving a repository is granting its token holders that user's authority. Run the server as a dedicated, unprivileged user on a machine you are willing to hand over. `--allow-provider` does not restrict callers to that provider; they may still ask for `local`. Neither sandbox provider is qualified yet (design §4, M2).
- **Allowed providers use the server's credentials.** A token holder chooses which of the server's variables a sandboxed turn gets through `pass_env`, and which Substrate endpoint, router and key path it uses.
- **Delegation** gives the server's harnesses the delegation tools, bounded by the envelope. As in local mode, a harness running as the server's user can read other branches' tokens in `.branchyard/`; the envelope stops honest mistakes, not a hostile harness, until harnesses run in sandboxes. `--allow-unapproved-tools` lets token holders run profiles whose tools the request's policy never sees.
- **`allow_client_commands`** additionally lets any token holder choose the executable the server launches. Leave it off outside tests; configure `harness_commands` instead.
- **Scopes and tenants bound what a credential can reach (`read`/`run`/`merge`/`admin`, a tenant's repositories, quotas), but not what it does within reach.** A `run`-scoped credential on an allowed repository can still make a harness do anything the server's operating-system user can, exactly as before: scopes are not sandboxing. There is still no expiry beyond a `credentials` hash the operator removes, and no hot rotation or revocation API — replacing a hash (or a `tokens` entry) and restarting is how a token is rotated or revoked.
- **Plain HTTP** exposes tokens, prompts and code to anyone on the path. The server refuses it off loopback unless told `--insecure-bind`.
- **Information exposure.** Responses include server paths (worktrees, repository roots) and the server's harness availability.
- **Policy is per request.** A `deny` default is safe; rules match tool names only, as in the SDK.

## Code

| Crate | Contents |
|---|---|
| [`branchyard-server`](../crates/branchyard-server/src/lib.rs) | Configuration, authentication, the operation registry and its SQLite and PostgreSQL stores, the activity feed, routes, TLS and shutdown; [`storage_routes.rs`](../crates/branchyard-server/src/storage_routes.rs) for artifacts and scratch areas. Tests: [`api.rs`](../crates/branchyard-server/tests/api.rs), [`parity.rs`](../crates/branchyard-server/tests/parity.rs) (opt-ins and delegation endpoints), [`postgres.rs`](../crates/branchyard-server/tests/postgres.rs), [`storage.rs`](../crates/branchyard-server/tests/storage.rs) |
| [`branchyard-client`](../crates/branchyard-client/src/lib.rs) | Wire types, a blocking HTTP/1.1 client over rustls, an SSE parser, and a reconnecting event stream |
| [`branchyard-cli`](../crates/branchyard-cli/src/main.rs) | `by serve`, remote mode and `by watch` |
| [`branchyard-herdr`](../crates/branchyard-herdr/src/main.rs) | The Herdr plugin's binary: the bridge, the branch pane, and the merge, cancel and send actions. Tests: [`bridge.rs`](../crates/branchyard-herdr/tests/bridge.rs) (against `by serve` and a fake `herdr`), [`manifest.rs`](../crates/branchyard-herdr/tests/manifest.rs) |
