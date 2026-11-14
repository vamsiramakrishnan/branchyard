# Branchyard server

`branchyard-server` (also `by serve`) serves one or more repositories, each under a name, over an authenticated HTTP JSON API with Server-Sent Events. It runs the local-mode engine in-process: the same tasks, branches, event logs, budgets, policies and validated merges as `by`, reached over the network. `by --remote URL` runs every `by` command against it, and [`branchyard-client`](../crates/branchyard-client/src/lib.rs) is the typed Rust client.

> **Status.** Tested hermetically over real HTTP on loopback against a fake ACP agent, and on PostgreSQL 16 for the database store. Run live on 27 October 2026 with Gemini CLI and Codex on one machine, on SQLite and on PostgreSQL 16.15 with a `by worker` beside the server, through a restart and a `kill -9` of the server ([the record](#run-live-on-27-october-2026)). Not load-tested, not deployed, and not run across hosts. The default provider is the local process provider: **no isolation** (see [Security](#security)). The server's operator can allow the Microsandbox and Substrate providers, delegation and unapproved tools; [surfaces](surfaces.md) lists what works on which surface.

## Running it

```sh
cd path/to/repo
by serve                      # serves this repository as its directory name on 127.0.0.1:8421
by serve --app --knowledge     # repository-free knowledge console; no JSON configuration needed
# branchyard-server: created a token in .branchyard/server/token; clients pass --token-file with it

by --remote http://127.0.0.1:8421 --token-file .branchyard/server/token ls   # or [remote] token_file in branchyard.toml

branchyard-server token new --tenant acme --scopes read,run --repo app
# branchyard-server: token (printed once; give it to the client, never store it): <token>
# {"token_sha256": "…", "tenant": "acme", "name": "token-…", "scopes": ["read","run"], "repos": ["app"]}
# paste that object into the configuration's "credentials" array

by init server                # or: an interview that writes the configuration, below, with a
                              # hashed credential per tenant and each token in a 0600 file
by serve --config .branchyard/server.json --check   # load and validate it; serve nothing
```

[`by init server`](setup.md) asks for the listen address and TLS, SQLite or PostgreSQL, tenants and their quotas, scopes, providers, delegation, secrets by reference and a webhook, and checks what it writes with the same loader and validation `--check` runs. `--check` builds and validates the configuration as serving would and exits: it binds nothing, opens no store and connects to no database, and where serving would create a default token it only says so. It does refuse what the build cannot serve: a `database` on a build without the `postgres` feature fails with the command that adds it ([modes](modes.md#postgres)).

A server given no static credential or Google identity binding makes a token at `<data dir>/token` the first time, and every start logs that path (`token_file`) so a client knows what to pass; `by init server`'s tokens are in the `tokens/` directory beside its file instead, and its summary lists them.

Flags (`by serve --help`, `by help serve` or `branchyard-server --help`, grouped there under TLS, what requests may do, operations and webhooks; everything after `by serve` is the server's, so its `--repo` and `--token-file` are these, not `by`'s global ones):

| Flag | Meaning |
|---|---|
| `-c`, `--config FILE` | JSON configuration, below. Flags override it. `by serve` without it reads `[serve] config` from `branchyard.toml` ([setup](setup.md#configuration)) |
| `--check` | Load and validate the configuration and flags as serving would, print warnings, exit; writes nothing |
| `--listen ADDR` | `IP:port`; default `127.0.0.1:8421`. Port 0 picks one; the server prints `listening on URL` to stdout |
| `--repo NAME=PATH` | Serve the repository at `PATH` as `NAME` (repeatable). Default: the repository containing the current directory, named after its directory |
| `--data-dir DIR` | Operation registry and activity feeds. Default: `.branchyard/server` in the first repository, or the launch directory with `--knowledge` and no repositories |
| `--knowledge` | Enable managed knowledge projects without a user repository. Storage defaults to `data_dir/knowledge`; configured roots and quotas remain in effect. Disabled unless this flag or `managed_knowledge.enabled` enables it |
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
| `--label LABEL` | A label this process's worker carries (repeatable; replaces the configuration's `labels`): it claims only operations whose `require_labels` are all among its labels. See [worker labels](#worker-labels) |
| `--no-inventory` | Do not detect and advertise the harnesses installed here, nor derive `harness:<id>` labels from them (the configuration's `"inventory": false`). See [worker inventory](#worker-inventory) |
| `--public-url URL` | This server's URL as webhook senders reach it: the base of each event [trigger](triggers.md)'s webhook URL. Default `http(s)://<listen>` |
| `--allow-trigger-prechecks` | Let every served repository's triggers run a [precheck](triggers.md#prechecks) command before firing. Default: none |
| `--app` | Serve the [web companion](companion.md) at `/app/`, accept paired tokens, and send Web Push. Off by default |
| `--unclaimable-after SECS` | How long an operation that requires labels may wait queued before it says why no live worker can claim it (`waiting`). Default 60 |
| `--max-artifact-bytes N` | Largest artifact a `POST .../artifacts` upload may publish, in bytes. Default 268435456 (256 MiB) |
| `--max-running N` | Operations this process runs at once; more wait queued. A [follow-up](#scheduling) and a console cancel or steer each have one more slot of their own, so up to N + 2 run at once. Default 8 |
| `--operation-lease SECS` | How long a claim on a queued operation lasts without renewal before another worker takes it over; renewed every third of it. Default 30 |
| `--shutdown-grace SECS` | At shutdown, how long running operations may finish. Default 60 |
| `-q`, `--quiet` | Do not log requests; also lowers the default log level to `warn` (see [Logging](#deployment-notes)) unless `BRANCHYARD_LOG`/`RUST_LOG` is set |
| `--log-format FORMAT` | `pretty` or `json` (one object per line) log lines on stderr; default `BRANCHYARD_LOG_FORMAT`, else `pretty` |
| `--admin-listen ADDR` | Serve the [admin compute console](admin-console.md) (`/admin`, `/v1/admin/*`) on this address only, with `--listen`'s TLS; `--listen` then answers `404` for it. A non-loopback address needs TLS or `--insecure-bind`; not on a `by worker`. `admin_listen` in the configuration |
| `--metrics` | Serve Prometheus metrics at `GET /metrics` to a principal with the `admin` scope or the metrics token ([observability](observability.md#metrics)); off by default |
| `--metrics-addr ADDR` | Also serve `/metrics`, and nothing else, on a plain-HTTP listener of its own (a `by worker` too); implies `--metrics` |
| `--metrics-token-file FILE` | A token (first line, at least 16 characters) that reads `/metrics` and nothing else; implies `--metrics` |

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
              "max_cost_usd": 50.0, "max_artifact_bytes": 1073741824,
              "weight": 2, "max_priority": 5 }
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
  "harness_install": { "allow": false, "path": "/usr/local/bin:/usr/bin:/bin" },
  "allow_workspace_scripts": ["app"],
  "allow_repository_skills": ["app"],
  "secrets": { "ANTHROPIC_API_KEY": "ANTHROPIC_API_KEY", "CODEX_AUTH": "@/etc/branchyard/codex-auth.json" },
  "database": "postgres://branchyard@db.internal/branchyard",
  "labels": ["linux", "gpu"],
  "inventory": true,
  "unclaimable_after_seconds": 60,
  "aging_seconds": 60,
  "fair_share_window_seconds": 300,
  "metrics": { "listen": "127.0.0.1:9464", "token_file": "/etc/branchyard/metrics.token" },
  "public_url": "https://by.example.com",
  "allow_trigger_prechecks": ["app"],
  "app": { "push_subject": "mailto:ops@example.com" }
}
```

`weight`, `max_priority`, `aging_seconds` and `fair_share_window_seconds` set [scheduling](#scheduling); `metrics` turns on [metrics](observability.md#metrics). Traces are configured by the standard OpenTelemetry variables instead ([observability](observability.md#traces)).

`allow_workspace_scripts` (`true`, or a list of served repositories) lets a repository's own `[workspace]` scripts in its `branchyard.toml` run for the branches this server creates: copy, setup before the first turn, teardown on removal, with `BRANCHYARD_PORT` reserved per branch. Off by default, and no request can ask for it: every other repository's yard refuses workspace scripts outright. See [workspace](workspace.md#trust).

`allow_repository_skills` (`true`, or a list of served repositories) stages a repository's own skills, `.agents/skills/` read from each new branch's base commit, beside the built-in skills of the branches this server creates. Off by default: every other repository's branches withhold them, and each turn's `provisioned` event says why. Neither a request nor the repository's `branchyard.toml` can turn it on. See [your repository's skills](knowledge-skills.md#your-repositorys-skills).

`app` (`true`, or its settings) turns on the [web companion](companion.md#setting-it-up): the page at `/app/`, pairing links and Web Push.

`allow_trigger_prechecks` (`true`, or a list of served repositories) is the same decision for [triggers](triggers.md): a trigger's precheck command runs, as the server's user, only for those repositories, and a trigger with one is refused (`403 precheck_not_allowed`) elsewhere. `public_url` is the base of every event trigger's webhook URL, for a server behind a proxy; without `connectors`, it is also the issuer of the model gateway's tokens when a model backend is called with a token the server signs (`auth = "turn-token"`, [model gateway](model-gateway.md#a-token-branchyard-signs)).

`execution_presets` names the environments a workspace session may run in, and `require_workspace_execution_presets` makes workspace sessions name one; see [execution presets](#execution-presets).

The server refuses to start with no repository, no token or credential, an `allow_workspace_scripts`, `allow_repository_skills` or `allow_trigger_prechecks` entry naming a repository it does not serve, a `public_url` that is not `http://` or `https://`, a token shorter than 16 characters, an unknown provider or scope name, a malformed or duplicated `credentials` hash, a `by_path` that is not a file, a `database` that is not a `postgres://` URL, an [execution preset](#execution-presets) that cannot run as written or `require_workspace_execution_presets` without one, a plain-HTTP bind to anything but loopback without `--insecure-bind`, a model backend that is an AIPlex access point (`control_plane = "aiplex"`) without a `url`, or a model backend with `auth = "turn-token"` whose tokens would have an issuer AIPlex cannot fetch keys from or match (not `https://`, or `http://` off loopback; an unspecified address, which is what `listen` on `0.0.0.0` or `[::]` gives with neither `public_url` nor `connectors.issuer`; a `listen` port of 0 with neither; or another spelling than the one Branchyard takes, where the refusal says what to write). It warns when a token file, or a configuration holding inline tokens or a database password, is readable by other users; when a model backend that is an AIPlex access point has a plain `http://` `url` on a host that is not loopback (its credential travels unencrypted unless the network encrypts it); and when a signed token's `audience` is on another host than its backend's `url` ([model gateway](model-gateway.md#aiplex-as-the-backend)). A password in `--database` is visible to other users of the host in its process list; prefer the configuration file, mode 600.

SIGINT or SIGTERM starts a graceful shutdown, the same for both: no new connections or operations (`503 shutting_down`), no more operations claimed from the queue, event streams end, requests in flight finish, and running operations get the grace period. They get all but its last quarter (at least 2 seconds and at most 10, or all of a shorter grace period) to finish on their own; then each turn still running is stopped through the same durable cancel `by cancel` records (`cancelled by the server's shutdown` in its log), which interrupts the harness, lets calls in flight through the connector gateway finish recording, and ends the branch `interrupted` with a known outcome, so its operation finishes and nothing is left for recovery to call lost. Continuing such a branch is a `send`, which resumes the harness session. Operations still running after the whole grace period are recorded as `interrupted`; queued ones stay queued, and run after the restart or on another server sharing the database. A second signal stops waiting at once. The log says each step: at the signal `shutting down: 8 running operations may finish within 60s (signal again to stop waiting); 2 queued stay queued for the next start`, and as the process ends `stopped: 7 finished within the grace period; 1 stopped cleanly for the shutdown; 2 left in the queue for the next start, or another server on the database` (and `N recorded interrupted` for any that outlasted even that) (a `WARN` line when something was interrupted; `nothing is running` and `nothing was running` for an idle server). A `by --remote` command following one of those operations waits through the restart ([remote mode](#remote-mode-in-by)).

Managed console turns use their retained tenant/workspace binding for shutdown cancellation, including private knowledge task yards outside the configured repository map. A mismatched tenant or binding is refused; no registry lock is held while resolving the yard or cancelling its turn.

The grace period bounds the whole shutdown, counted from the signal: connections drain (for at most 10 seconds, and at least 1 even with `--shutdown-grace 0`) while running operations get their grace, not one after the other, so a client that never finishes its request cannot hold the process past it. A supervisor's stop timeout must be longer than `--shutdown-grace`: `docker stop` sends SIGTERM and kills 10 seconds later by default, so [the compose recipe](deploy.md#compose-server-behind-postgresql) sets `stop_grace_period: 75s` for the default 60. The handlers are installed before the server opens its repositories, so a SIGTERM during startup (which a container's PID 1 would otherwise ignore) is kept and stops the server once it serves. `tests/signals.rs` runs the `branchyard-server` binary and checks both signals stop an idle server with an open event stream at once, and that SIGTERM with `--shutdown-grace 6`, a turn that never ends and a half-sent request exits 0 within the grace period (connections no longer take 10 seconds of their own first), the turn stopped cleanly at 4 seconds: its operation succeeded, its branch `interrupted` by the shutdown, with no recovery.

## Identity and scopes

Google-only ID-token admission can also map exact verified issuer/subject identities to this principal policy. See [native identity](identity.md) for configuration, durable provenance, queued binding rechecks and the separate upstream connector account boundary.

Every route except `GET /healthz`, `GET /.well-known/jwks.json` (public keys, [connectors](#connectors)) and `POST /v1/triggers/{id}/fire` (a [trigger](triggers.md)'s webhook, authenticated by its own HMAC signature instead) needs `Authorization: Bearer <token>`, including unknown routes, so routes cannot be probed anonymously. A presented static token is **hashed (SHA-256) and compared against every configured static credential's hash** in time that depends only on lengths; the verifier holds hashes, never a plaintext token, and neither tokens nor headers are ever logged. A missing or wrong token gets `401 unauthorized` with `WWW-Authenticate: Bearer`.

**A request's identity comes only from its verified credential, never from anything the request itself says**: there is no `tenant_id` field anywhere in the wire protocol. Each configured credential names a **principal**: a `tenant` (1 to 128 characters, no `/`), a subject `name` (the caller's identity for idempotency scoping and audit), a set of **scopes**, and, optionally, its own repository allowlist narrower than its tenant's. Two ways to configure one:

- **`tokens`** (unchanged from before tenants existed): `{"name", "token"|"token_file"}`, plus optional `tenant`, `scopes` and `repos`. A `tokens` entry that gives none of those three becomes a principal in the unconfigured `default` tenant with every scope and every repository (**exactly what a single-token server did before this existed**, so old configurations keep working unchanged).
- **`credentials`**: `{"token_sha256", "tenant", "name"?, "scopes"?, "repos"?, "groups"?}`, storing the token's SHA-256 directly so the plaintext never has to enter the configuration at all. `branchyard-server token new [--tenant T] [--scopes S,...] [--repo R,...]` (also `by serve token new`) generates a fresh token (scopes `read,run,merge` unless `--scopes` names others; `admin` only when named), prints it once (give it to the client, never store it), and prints the `credentials` object to paste in. There is no hot rotation or revocation API for these: replacing or removing a hash and restarting is how a token is rotated or revoked, the same restart a `tokens` change already needed. A third kind, **paired tokens**, exists only with the [companion](companion.md) on: `by serve token new --link` prints a one-time pairing link whose code becomes a token with the scopes, tenant and repositories the link was made with, stored hashed in the server's store, expiring, and revocable at once with `by serve token revoke NAME` (`token list` shows them).

Scopes: `read` (every `GET`, including the event stream), `run` (submitting a task, send, fork, reincarnate or spawn, and acting on a running turn: cancel, steer, ask, report, escalate, answer; and acting for a branch: check, report-status), `merge` (merge, integrate), `admin` (removing a branch, and the [admin compute console](admin-console.md)). Every endpoint checks the caller's scope and returns `403 scope_required` (`detail.scope`) when it is missing; `read` is checked once for every `GET` and `HEAD` before any handler runs, so a credential with `run`, `merge` or `admin` but not `read` sees nothing. A secret may appear once across `tokens` and `credentials`: the same secret twice (two tokens, or a token and a hash) is refused at startup, naming the entries, since it could otherwise authenticate as either principal.

**Repositories belong to tenants.** A tenant's `repos` entry is its repository allowlist. A principal's own `repos`, when present, narrows that list further. `GET /v1/repos` lists the served repositories within both lists. Acting on another repository returns `403 repo_not_allowed` with `detail.repo`.

Global operation reads check the caller's tenant and current repository allowlists, including reads by ID, long polls, and reconciliation by idempotency key. An inaccessible operation returns `404 unknown_operation`, just as an unknown ID or key does. Authorization happens before a long poll waits, so a hidden running task does not reveal its lifetime. An authorized caller can read an operation through any server sharing its store, even when that server does not serve the operation's repository locally.

There is no additional submitter-only ownership for operation reads. A same-tenant reader authorized for a repository can read its operations. Operations retain the admitting principal for worker execution, while a branch records its subject and tenant for the [connector gateway](#connectors). Feed entries do not carry a separate principal field.

Workspace-bound operation reads also check current tenant, repository grants and durable workspace membership without requiring a local yard. Native branch, session and file operations still resolve their source on the serving node. Repository-hidden operations remain `404 unknown_operation` before long polling; a missing managed ownership binding or revoked workspace membership still fails closed. The HTTP operation routes recheck the captured credential after waiting and before returning metadata.

## Quotas

A tenant's `tenants.<name>` entry sets its resource ceilings, all optional (unconfigured means unlimited):

| Field | Enforces | Checked | Refusal |
|---|---|---|---|
| `max_running` | Operations of this tenant queued or running at once | In the admission's transaction, against the operation store | `429 quota_exceeded` (`detail.limit = "max_running"`); at the ceiling, one [follow-up](#scheduling) and one console control are still admitted |
| `max_branches` | Branches across this tenant's repositories, counting those its queued and running operations will create | In the admission's transaction of a task, fork, reincarnate or spawn | `429 quota_exceeded` (`"max_branches"`) |
| `max_cost_usd` | Total `cost_usd` recorded across this tenant's currently-open branches (lifetime, not a rolling window; see below) | Live, before a task/fork/reincarnate/spawn is admitted | `429 quota_exceeded` (`"max_cost_usd"`) |
| `max_artifact_bytes` | Total artifact bytes reachable across this tenant's repositories | Same as `max_cost_usd`, and on every artifact upload: refused when the upload would take the tenant past it (counted before the upload's digest is known, so re-publishing bytes already held counts too; an idempotent retry is replayed first) | `429 quota_exceeded` (`"max_artifact_bytes"`) |

A refusal's `detail` has `tenant`, `limit`, `max` and `reserved` (or `spent`). A refused admission writes nothing: no operation record, no idempotency binding, no branch lock, no queue row, so a retry with the same key once the tenant is under its ceiling is admitted normally.

**`max_running` is transactional, and holds across servers and restarts.** Every operation's record carries its tenant (`tenant`, with the admitting `principal`). [Admission](#dispatch) counts the tenant's rows in the queue (its queued and running operations, joined to their records) inside the same transaction that writes the new operation, and refuses when that count is at the ceiling. Two kinds are still admitted at the ceiling: a [follow-up](#scheduling) while the tenant has no follow-up queued or running, and a console cancel or steer while it has no other. A tenant whose triggers filled its limit can therefore queue a branch continuation instead of receiving `429`. On SQLite that transaction is `BEGIN IMMEDIATE`, so admissions take turns; on PostgreSQL an admission of a tenant with a quota first takes a transaction-scoped advisory lock on the tenant (`pg_advisory_xact_lock(hashtextextended('branchyard tenant ' || tenant, 0))`), so admissions of one tenant take turns on every server sharing the schema while other tenants' do not wait. The count is the database's, not a process's, so two servers on one database cannot both admit a tenant's last slot, and a restarted server sees the same count. Release is implicit: the transaction that records an operation's outcome deletes its queue row, so it stops counting the moment it is `succeeded`, `failed` or `interrupted`. A queued operation survives a restart and keeps its slot until it runs and finishes; one running at shutdown is recorded `interrupted` and frees its slot. Merges, integrations and checks count toward `max_running` but are not refused by it. Servers sharing a database should configure the same `tenants`: each admission checks the ceiling of the server that admits it, against the shared count.

**`max_branches` is transactional too, with one exception.** Admission records the branch names an operation will create (`creates`: the planned names that `branches` reports), and a task, fork, reincarnate or spawn is refused when the tenant's branches would exceed the ceiling: the set of branches that exist in its repositories together with those its queued and running operations will create, each counted once, plus this operation's new ones. Both are read inside the admission's transaction (first the tenant's queued and running operations, then the branches that exist), so an operation that finishes in between is counted by one or the other, never neither, and a concurrent admission of the same tenant waits its turn. What is still best-effort:

- **Branches delegated from inside a running turn** (a harness's `by spawn`, or the delegation tools) are not reserved at admission; they count once they exist, so delegation within a running operation can take a tenant past `max_branches`, bounded only by the operation's delegation envelope. `POST …/spawn` through the server is reserved like any other operation.
- **A graph proposal** (`POST …/branches/{b}/graph`) is not an operation, so its spawns are not reserved in an admission's transaction: the server counts the tenant's branches and those its unfinished operations will create, as admission does, and refuses the proposal whole with `429 quota_exceeded` when its spawns would pass `max_branches` (it also checks `max_cost_usd` and `max_artifact_bytes`). Two proposals racing past a near-limit tenant can both be applied.
- **Branches created outside the server** (a local `by` on a served repository) count once they exist.
- **Operations admitted by an earlier version** carry no `creates`; their branches count once they exist.

Removing a branch (`DELETE .../branches/{b}`, needing `admin`) frees its `max_branches` and `max_artifact_bytes` reservation immediately.

CPU and memory settings apply to an individual worker or sandbox. Tenant quotas do not aggregate those settings into CPU or memory reservations, or retain such reservations until teardown is independently verified.

`max_cost_usd` and `max_artifact_bytes` stay checked **live** against data the engine keeps durably (a tenant's open branches, their `cost_usd`, and their reachable artifacts) before a branch-creating operation is admitted, so they are exact across a restart with nothing extra to persist, but best-effort: they are not taken in the admission's transaction, two requests racing past the same near-limit tenant can both be admitted, and spend accrues while turns run. `max_cost_usd` is lifetime spend across a tenant's currently-open branches, not a rolling time window; the engine does not otherwise keep a timestamped cost ledger, and adding one only for a windowed quota was judged not worth the added durable state for this release.

**Workers act as the admitting principal.** A queued operation's record holds the principal that admitted it (tenant, subject, scopes, repository allowlist), as its credential verified. The worker that runs it (the admitting server, another server on the database, or a `by worker` process, which needs no credential of its own) runs it as that principal: before running it, the worker checks that the principal holds the operation's scope and that its tenant, under the worker's own `tenants` configuration, owns the repository, and otherwise fails the operation with `403 scope_required` or `403 repo_not_allowed`, the error the request would have got there. Running another tenant's operation reveals nothing to anyone: the operation stays visible only to its own tenant.

## Connectors

`"connectors"` in the configuration file gives every served repository's branches the connector gateway ([connectors](connectors.md)):

```json
"connectors": {
  "gateway": "https://gateway.example/mcp",
  "issuer": "https://branchyard.example:8421",
  "bundles": "/srv/connectors",
  "anvil": ["node", "/opt/anvil/packages/cli/dist/bin-anvil.js"],
  "run_gateway": true
}
```

| Key | Meaning |
|---|---|
| `gateway` | The gateway's `/mcp` URL: every token's audience, and what a harness on this host is given. Required |
| `sandbox_gateway` | The gateway as a sandboxed harness reaches it; without it a sandboxed branch with a grant fails its turn |
| `issuer` | Every token's `iss`. Default `http(s)://<listen>` when the gateway is on loopback; set it to the server's public URL. **Required** when `gateway` is not on loopback, and on a worker (see [signing on workers](#signing-on-workers)) |
| `signing_key` | The private key set, made on first use by a server (never by a worker). Default `<data_dir>/gateway/key`. Its public half is `GET /.well-known/jwks.json` and `<data_dir>/gateway/jwks.json`. Not with `signing` |
| `signing` | `{"kms_key_versions": [...], "kms_base_url": "..."}`: sign with [Cloud KMS](#signing-with-cloud-kms) instead of a key file. Not with `signing_key` |
| `audit_file` | The gateway's audit log, read into `connector_call` events by each repository's poller. Default `<data_dir>/gateway/audit.jsonl` |
| `bundles`, `anvil` | The bundle root the gateway serves, and Anvil's command (default `["anvil"]`), for packaging and for `run_gateway` |
| `run_gateway` | Run `anvil serve mcp <bundles> --fleet --http <port>` in branchyard mode beside the server, supervised and restarted, reading the public keys from the file above; stopped with the server. A worker (`by worker`) never runs it |
| `listen`, `vault_key` | The address the gateway run here binds when not loopback, and its vault key (default `<data_dir>/gateway/vault.key`, made 0600) |
| `effects_proxy`, `effects_listen`, `effects_sandbox_host` | Each turn's effect-ledger proxy in front of the gateway (default on, on `127.0.0.1`), and how a sandboxed harness reaches it ([effects](effects.md#begun-before-the-call)) |
| `sandbox_server` | This server's URL as a sandbox reaches it. When set, a sandbox whose provider can run a credential proxy fetches the turn's token from [`POST /v1/turns/token`](#turn-tokens-for-sandboxes) and the harness holds none ([connectors](connectors.md#the-credential-proxy-in-the-sandbox)). Without it every turn keeps the host's proxy |
| `sandbox_identities` | `[{"service_account": "NAME@PROJECT.iam.gserviceaccount.com", "tenant": "acme"?}]`: the Google service accounts that may fetch tokens, verified from a Google-signed ID token. `tenant` restricts an account to one tenant's turns. Needs `sandbox_server`; empty admits a turn's own secret alone. With accounts listed, a fetch must present a listed account's ID token *and* the turn's secret |
| `sandbox_audience`, `sandbox_jwks_url` | The `aud` of that ID token (default `issuer`), and where Google's signing keys are read from (default Google's) |
| `sandbox_by`, `sandbox_proxy_image` | The `by` a Microsandbox guest runs (default `by`), and the image of a GKE Agent Sandbox's `credential-proxy` sidecar (`repository@sha256:digest`, with `by` on its `PATH`; default the sandbox's own) |

### Turn tokens for sandboxes

`POST /v1/turns/token` gives a **running turn's** sandbox its gateway token. It is outside the bearer-token check, because the caller is a sandbox and authenticates itself; the credential proxy in the sandbox ([connectors](connectors.md#the-credential-proxy-in-the-sandbox)) calls it every few minutes.

```json
{"manifest": "blake3:...", "branch": "app/feat-x", "turn": 3}
```

`manifest` is the digest of the turn manifest the turn was admitted with, `branch` is `<repo>/<branch>` as tokens name it, `turn` its number. Unknown fields are refused: claims are never taken from the request. None of the three is secret (the digest is on the console's session view and in the `provisioned` event; a harness reads its own), and a Workload Identity service account is normally shared by every sandbox, so the caller proves who it is with the turn's secret **always**, and with an identity **in addition** where the server lists any:

- `X-Branchyard-Turn-Secret: <secret>`, required: the per-turn secret the engine made for this turn and gave its sandbox's proxy (a guest's environment, or the GKE sidecar's settings file in the turn's home), checked against the hash in the branch's record, which is bound to the turn's manifest and number and dropped when the turn ends. A harness in another sandbox, even with the shared service account's ID token and the turn's real coordinates, does not hold it.
- `Authorization: Bearer <Google ID token>`, required when `sandbox_identities` lists any account: an ID token for `sandbox_audience` (default `issuer`) of a listed service account, fetched by the sandbox from the metadata server (a GKE Workload Identity service account, or a GCE VM's). The server verifies the signature against Google's keys, the audience, the issuer (`accounts.google.com`), the lifetime and `email_verified`, then looks the account up. Where no account is listed, a bearer is not read.

The answer is `200` `{"token", "expires_at", "expires_in"}`: a gateway token (the same claims as the turn's own: `by_branch` `<repo>/<branch>`, `by_turn`, `by_manifest`, the recorded grant with the person's ceiling applied, the principal's `sub` and `by_tenant`) that lives five minutes. The server logs the branch, turn, manifest and who asked, never a token.

| Code | Status | When |
|---|---|---|
| `auth_required` | 401 | No `X-Branchyard-Turn-Secret` (an identity alone is not enough), or the server lists `sandbox_identities` and no bearer came with it |
| `invalid_credential` | 401 | The bearer is not a valid Google ID token for this server, or the secret is not this turn's (or this manifest's and number's) or the turn has ended |
| `identity_not_allowed` | 403 | A valid Google ID token of a service account the server does not list |
| `tenant_mismatch` | 403 | The account is restricted to another tenant than the turn's |
| `no_connector_grant` | 403 | The turn was granted no connectors |
| `invalid_request` | 400 | `branch` is not `<repo>/<branch>`, or the body is malformed or names an unknown field |
| `unknown_turn` | 404 | No such repository or branch |
| `manifest_mismatch` | 409 | The digest is not the one the turn was admitted with |
| `turn_not_running` | 409 | The branch is not running a turn (finished, failed, cancelled, queued) |
| `turn_mismatch` | 409 | `turn` is not the turn that is running |
| `token_unavailable` | 503 | The gateway is not configured, or signing failed |
| `not_found` | 404 | The server has no `connectors` |

Hermetic evidence: `crates/branchyard-server/tests/turn_token.rs` (a real server, a turn held open by a named pipe, Google's keys served by a mock and ID tokens signed with a test key). Not done: a run against Google's keys and a real metadata server, from a GKE pod or a GCE VM.

### Signing on workers

A worker (`by worker`, `by serve --worker`) signs the turn tokens of the operations it runs, so the gateway must trust the worker's signature as it trusts the server's. A worker therefore never invents a signing key and never guesses an issuer:

- `connectors.issuer` is required on a worker, and on any server whose `gateway` is not on loopback. A default taken from each process's own `listen` address would give the server and its workers different issuers, and the gateway checks one.
- A worker needs `connectors.signing` (Cloud KMS, below) or an explicit `connectors.signing_key`. With a key file it must be the server's own file, copied or mounted (a secret volume on GKE); the worker reads it and refuses to start if it is missing, instead of making a second key. Prefer KMS: nothing is copied, and a rotation reaches every process at once.

### Signing with Cloud KMS

With `connectors.signing`, the private key never leaves Cloud KMS. Make an asymmetric signing key with the algorithm `EC_SIGN_ED25519` and name its version:

```json
"connectors": {
  "gateway": "https://gateway.example/mcp",
  "issuer": "https://branchyard.example:8421",
  "bundles": "/srv/connectors",
  "signing": {
    "kms_key_versions": [
      "projects/my-project/locations/europe-west1/keyRings/branchyard/cryptoKeys/gateway/cryptoKeyVersions/2",
      "projects/my-project/locations/europe-west1/keyRings/branchyard/cryptoKeys/gateway/cryptoKeyVersions/1"
    ]
  }
}
```

- **Signing.** For each token the server posts its signing input to `https://cloudkms.googleapis.com/v1/{version}:asymmetricSign` as `{"data": base64(input)}` (Ed25519 signs the data itself, not a digest) and joins the answer into the compact JWS. The `kid` is `kms-<keyRing>-<cryptoKey>-<version>`.
- **Public keys.** `GET /.well-known/jwks.json` serves the public key of every listed version, read once from `{version}/publicKey` (PEM) and kept for the process. The gateway reads that URL (`jwks_uri`) to verify tokens.
- **Rotation.** The first version signs; all are published. To rotate, create a new version, list it first, and restart the server and its workers. Drop the old version from the list once the tokens it signed have expired (an hour at most).
- **Credentials.** Requests carry an access token of the service account the process runs as, from the metadata server (`GCE_METADATA_HOST`, else `metadata.google.internal`), with the `cloud-platform` scope: GKE with Workload Identity, Compute Engine and Cloud Run provide it. Grant that account, for the server and for every worker, `roles/cloudkms.signerVerifier` on the key (it signs and reads the public key). An account that only publishes keys, such as a gateway host that reads the JWKS some other way, needs only `roles/cloudkms.publicKeyViewer`.
- **Refused.** `signing` together with `signing_key`, an empty list, a name that is not a key version name, and the same version twice. A key that is not `EC_SIGN_ED25519` fails when the public key is first read, naming its algorithm.
- **`kms_base_url`** replaces `https://cloudkms.googleapis.com`; it exists for tests.
- **The model gateway** signs its turns' tokens with the same keys when `connectors` is set.

Status: hermetic. The signer runs against a fake Cloud KMS (a mock HTTP server holding Ed25519 keys): the sign request's body and headers, the PEM to JWK conversion, several versions, the cache, error answers, and a token verified against the JWKS. Not done: a run against real Cloud KMS, with a real `EC_SIGN_ED25519` key, the metadata server's token and the two IAM roles; a gateway verifying a KMS-signed token end to end.

A branch's token names the principal that created it (`sub` its name, `by_tenant` its tenant; forks and delegated children keep their parent's) and the branch as `<repo>/<branch>`, so one gateway and one audit log can serve every repository; each repository's poller records only its own lines. A request naming connectors on a server without `connectors` is `403 connectors_not_configured`. Requests choose grants freely within what they ask for: the gateway enforces each grant against the person's own connected accounts, and a delegated child's is narrowed to its parent's. `by connect` and `by gateway` act locally only.

### Connecting from the console

A person connects their own accounts through the server, so nobody needs a terminal on its host. The server signs the caller's connect token (`by_purpose: "connect"`, ten minutes, the subject and tenant the caller's console turns carry, so the account connected is the one their sessions use) and asks the gateway's connect routes for them. The token never leaves the server; an API key passes through once to the gateway's vault and is not kept, logged or answered back.

| Route | What it does | Scope |
|---|---|---|
| `GET /v1/connections` | `gateway` (`ready`, `not_configured`, `unreachable`) and, for each connector the gateway serves, the caller's `state` (`connected`, `needs_reconnect`, `not_connected`, `blocked` by their ceiling, `unknown` while the gateway does not answer), `accounts`, `max_mode` under their ceiling and a plain `reason` | `read` |
| `POST /v1/connections/{connector}/connect` | `{"account"?}`: `{"kind": "oauth", "url", "expires_at"}` to open in a browser, or `{"kind": "key"}` | `run` |
| `POST /v1/connections/{connector}/key` | `{"api_key", "account"?}`: stores a key for a key-based connector; answers `{"connected", "connector", "account"}` | `run` |
| `GET /v1/workspaces/{workspace}/tools` | What a session form offers: `connections` (as above), the built-in `skills` (id, description, topic, what each brings), and `helpers` (`allowed`, `reason`, `max_children`) | `read` on the workspace |

A console session start (`POST /v1/workspaces/{workspace}/sessions`) takes three more fields, all optional and all checked again at admission:

- `connectors`: grants in the object or `CONNECTOR[@ACCOUNT][:MODE[:OPS]]` form. They sit beside a `preset` (unlike `provision`, which a preset refuses) and are refused beside `provision.connectors`. Each must be served by the gateway, within the caller's ceiling and connected by the caller; a reviewer's are read only.
- `skills`: the built-in skills to stage, by id; each brings the skills it needs, and the harness's skill index lists only those ([knowledge skills](knowledge-skills.md#choosing-built-in-skills)). Unset: every built-in skill.
- `helpers`: `{"harnesses", "max_children"?}`, the harnesses the session may delegate to, one level deep, at most 16 at once (default 4). It needs `allow_delegation`; with a preset, each harness must be one the preset offers; a reviewer has none.

The session row keeps what it chose as `tools`, which the console shows beside the session.

### Tool policy

An administrator decides which tools a person may use, in the server's store (SQLite, or PostgreSQL such as AlloyDB), not in each worker's JSON. A policy has four optional parts: `connectors` (a ceiling in the `--connector` form), `skills` (built-in skill IDs), `helpers` (harness IDs a session may delegate to) and `delegation` (`false` turns helpers off). A part that is unset says nothing. An empty list allows nothing.

Policies sit at three levels, keyed by the caller's own tenant:

- `tenant:<tenant>`
- `group:<tenant>:<group>`, for callers whose principal lists the group in `groups` (a `tokens` or `credentials` entry's `groups`; a paired device carries its issuer's)
- `principal:<tenant>:<subject>`, for one person, by their stable subject

A connector entry is checked in full, not only by connector and mode. At a session start each requested entry is narrowed to the policy: it keeps only the operations the policy's globs cover, `confirm: allow` only where the policy allows it, and the account it names when the policy entry names that account or none. A connector the policy does not list, a write where it allows reads, or operations it allows none of is refused `tool_not_allowed`. The session records the narrowed entries, operations and confirmation included, and the policy recheck below holds them to the policy the same way; an entry the current policy covers only in part is refused with `detail.allowed`, what it would now allow. A session recorded before operations were kept is checked as every operation without confirmation.

A caller's effective policy is the tenant's, narrowed by the union of their group policies and their own, then by the person's own `self:` record (below), then by the server: the `ceilings` configuration and `allow_delegation`. Each step only narrows. In the union, lists pool, delegation is on when any record allows it, and a record that leaves a part unset adds nothing to it, so a group that sets only `delegation` never lifts another group's limit on skills. A skill brings the skills it needs whether or not the policy lists them. Policy applies on the next request, and every session start checks it again.

A person narrows themselves with a fourth record, `self:<tenant>:<subject>`, which they write through `GET` and `PUT /v1/me/policy` and an administrator does not (`GET /v1/admin/policies` omits it and `PUT /v1/admin/policies/self:...` is `policy_key_invalid`). It has the same parts as any policy, and it is intersected after the administrator's result, so whatever it lists the administrator's policy stays the ceiling: a skill, helper or connector the administrator withholds stays withheld, and a connector stays read only if the administrator says so. Its natural use is `off`, below.

Two parts of a policy are new: `max_helpers` (1 to 16), the most helpers a session may have at once, where two records narrow to the lesser and a group pool takes the widest; and `off`, a list of `{"kind", "id"}` switched off whatever the lists allow (`{"kind": "helper", "id": "*"}` turns every helper off). In the union of groups a tool is off only when every record that sets `off` turns it off. A start meets `off` by name: `tool_not_allowed` with `detail.off: true` and a message saying the tool is turned off for you, not that the administrator has not allowed it.

Each tenant has a **policy version**: a counter, stored in the same collection as the policies (key `version:<tenant>`, never listed as a policy and refused as a policy key), that rises by one in the same transaction as every `PUT` and `DELETE` of any of the tenant's policy records, a person's own included. A refused change does not move it. `GET /v1/admin/policies` returns it as `version`. A session start records the version it was admitted under (`policy_version` on the session). When the session's operation is claimed, and for a send as well as a start, the server compares: if the version has not moved, nothing more is checked. If it has, the session's tools (its connectors with their operations, mode, confirmation and account, its skills, or all of them when it chose none, and its helpers) are checked against the policy as it stands now. A tool no longer allowed fails the operation `policy_changed` and the session keeps the version it was admitted under; the person starts again with allowed tools. If every tool is still allowed, the session records the new version, and the turn's [manifest](connectors.md#the-turn-manifest) takes it. The check is of policy only: it does not look again at whether the person's accounts are connected.

**A claim uses the policy that admitted the session, not the claiming process's.** Several processes share one database and claim operations: `by serve` and its workers. Admission records the server-side limits it applied in the session row, as `server_ceiling`: `{"connectors": [...] | null, "allow_delegation": bool}`, the `ceilings` configuration's connector grants for the caller (`null` when none are set) and whether the server delegates at all. Whichever process claims a start or a send holds the session's tools to three things, each only narrowing the one before:

1. the stored policy as it stands now (read from the shared store; checked when the policy version has moved, as above);
2. the `server_ceiling` the session was admitted under, checked at every claim, which costs no read; and
3. the claiming process's own `ceilings` and `allow_delegation`, which can narrow the recorded ceiling and never widen it.

A worker with a looser or older configuration than the admitting server therefore cannot widen a turn: a session that breaks the ceiling it records (a row that was changed) is refused `policy_changed` whatever the worker allows. A worker whose configuration is narrower than the recorded ceiling runs a session that still fits its own, and refuses one that does not with `policy_changed`, `detail.readmit: true` and a message that asks for re-admission. A session admitted before ceilings were recorded has no `server_ceiling` and is held to the claiming process's own, as it was.

`POST /v1/workspaces/{workspace}/sessions/{session}/readmit` (the person the session belongs to, with the scope a send needs) runs the admission check again on the process that answers: the session's stored `tools` against the caller's stored policy as it stands, narrowed by that process's own ceiling. If every tool is allowed the session records that process's ceiling as its `server_ceiling`, the tenant's current `policy_version` and `readmitted_at_ms`, and answers `{id, policy_version, server_ceiling}`; the next turn's manifest takes the version. If not, `policy_changed` with the same `detail` as a refused claim, and the session is unchanged. Re-admission records one process's view: a worker whose configuration is narrower than that process's still refuses, and the way past is to run the session on a worker with the admitting configuration, or to give every process the same `ceilings`.

What still comes from the claiming process's own configuration, because it describes that machine and not the person: the harness commands and `pass_env` it offers, its sandbox providers and the paths and credentials they use, its connector gateway, packager, signing key and tool store (`connectors`, `tools`), the model gateway, its approval settings, `allow_unapproved_tools`, its `by` binary, its repositories and each tenant's repository allowlist, and the identity configuration it rechecks the admitting principal against (`iap`, `oidc_issuers`, `google_identity`, the credentials). A worker that lacks one of these fails the operation by name (a harness it cannot run, a provider it does not offer). Only the connector ceiling and delegation, the two limits on what a person's session may reach, are held to the admitting server's record. Bundles are the engine's: a turn also checks the skills and connector bundles it links or copies against the [digests its manifest recorded](connectors.md#the-turn-manifest) and fails `bundle_mismatch` when this worker holds others.

| Route | What it does | Scope |
|---|---|---|
| `GET /v1/admin/policies` | `{"policies": [{"key", "policy", "revision"}]}` for the caller's tenant | `admin` |
| `GET /v1/admin/policies/{key}` | one policy | `admin` |
| `PUT /v1/admin/policies/{key}` | `{"policy", "revision"?}`: creates (no `revision`) or replaces at the stored `revision`; `409 revision_conflict` otherwise | `admin` |
| `DELETE /v1/admin/policies/{key}?revision=N` | removes one; `revision` is optional | `admin` |
| `GET /v1/me/policy` | the caller's own record: `{"key": "self:<tenant>:<subject>", "policy", "revision"}`, an empty `policy` at revision 0 before the first save | `read` |
| `PUT /v1/me/policy` | `{"policy", "revision"?}`: creates (no `revision`, or 0) or replaces at the stored `revision`; `409 revision_conflict` otherwise. Raises the tenant's policy version | `run` |

The tenant comes from the credential, never the request: a key naming another tenant is `policy_key_invalid`.

### Tools

`GET /v1/tools?workspace=ID` (scope `read`) lists every connector, skill and helper for the caller, each with `kind`, `id`, `name`, `state`, `allowed`, `ready`, `default`, `max_mode` (connectors), `action` and `reason`. `off_by_me` is true (and `action` is `turn_on`) when the person switched the tool off in their own policy: `allowed` is then false and `state` `not_allowed`, but it is the person's switch, not the administrator's. `installable` is true for a helper whose harness an administrator can install now (below). The list also carries `max_children`, the most helpers a session may have at once for this caller (16, or the administrator's lower `max_helpers`), and `default_children`, the number a start gets when it names none. `state` is the first step the tool fails: `not_allowed` (action `ask_admin`), `not_ready` (`connect` for a connector with no live account, `install` for a helper whose harness no environment has), else `ready`, or `default` when the workspace starts sessions with it. A skill is ready once allowed (installing skills per person comes later). A helper is ready when an environment offered to the tenant can run it; `sign_in` is reserved for per-person harness sign-in. Without `workspace` nothing is `default`.

`PUT /v1/workspaces/{workspace}/tools` stores the workspace's defaults, `{"tools": [{"kind", "id", "mode"?}], "revision"}`, for its owner or an editor (`409 revision_conflict` on a stale `revision`; use `0` before the first save). `GET` on the same path returns them as `defaults`. A client sends them in `tools` when it starts a session; the server does not add them itself.

A session start takes `tools: [{"kind": "connector|skill|helper", "id", "mode"?, "account"?, "max_children"?}]`. The server expands it to `connectors` (mode `read` by default), `skills` and `helpers`. The helper limit is the helper entry with `id` `*`: `{"kind": "helper", "id": "*", "max_children": 2}` names no harness and sets `helpers.max_children` (1 to 16; default 4, or the administrator's `max_helpers` when that is lower). Alone it gives helpers that run the session's own harness. A number above the policy's `max_helpers` is refused `tool_not_allowed` with `detail.max_children` set to the most, and `max_children` on any other entry is `invalid_request`. A workspace's saved defaults may carry the same entry. Connectors and helpers are the whole choice: none listed is none given. Skills narrow: with no `skill` entries the session keeps every built-in skill its policy allows, so choosing a connector alone never removes skills. It is refused beside any of those fields (`tools_conflict`). The old fields still work. With no skill choice at all, a policy that lists skills stages the ones it allows.

#### Install a helper's harness

`POST /v1/tools/helpers/{id}/install` (scope `admin`) installs the harness behind helper `id` on **this server's own host** and answers `202` with `{"harness", "state", "host": "server", "command", "created_at_ms", "finished_at_ms"?, "problem"?, "output_tail"?}`, `state` being `queued`, `running`, `succeeded` or `failed`. `GET` on the same path returns the latest one (`404` when none was started). A worker on another machine keeps what it has: the install is per host, and `host` says `server`. When it succeeds the helper's row in `GET /v1/tools` moves from `not_ready` to `ready` and loses its `install` action.

It is closed until the operator sets `"harness_install": {"allow": true}` (`403 harness_install_disabled`), because it runs the harness catalog's install command (`npm install -g ...`, a vendor's installer script) as the server's user. The command is the first one in `catalog/harnesses.toml` whose programs are on the install `PATH` (`harness_install.path`, else the server's own), pinned to the version the harness was checked against where the installer takes one, the plan `by harnesses install` makes. Only a harness an environment offered to the caller's tenant, that runs on the server's host (a local environment), can be installed (`404 not_found`, `422 install_unsupported`); an installed one is `409 already_installed`. A second `POST` while one is running returns the running one. The command stops after 15 minutes. Each attempt is appended to `<data_dir>/harness-events.jsonl`, the file `by harnesses` keeps, as `by: "console"`. The status is held in the server process: a restart forgets it. Only an administrator installs; a person cannot install a harness for themselves, because the harness is a fact of the host and not of the person.

| Error | Status | Meaning |
|---|---|---|
| `tool_not_allowed` | 403 | Policy does not allow a tool; `detail.kind`, `detail.id` (`*` when delegation is off or for the helper limit), `detail.max_mode` for a connector allowed less, `detail.off` when the person's own policy turned it off, and `detail.max_children` for a helper limit above the administrator's |
| `harness_install_disabled` | 403 | `harness_install.allow` is not set in the server's configuration |
| `already_installed` | 409 | The helper's harness is already installed on this server |
| `install_unsupported` | 422 | The helper runs somewhere other than this server's own host, or the harness catalog has no install command the server can run |
| `tools_conflict` | 400 | `tools` beside `connectors`, `skills` or `helpers` |
| `policy_key_invalid` | 400 | A policy key that is not one of the three forms for the caller's tenant |
| `policy_changed` | 403 | The tenant's policy changed after the session was admitted, and a tool it chose is no longer allowed; same `detail` as `tool_not_allowed`. Also a claim by a process whose `ceilings` or `allow_delegation` allow less than the server that admitted the session, with `detail.readmit: true`, and a refused `readmit`. Returned in the failed operation, not on the start |

## Approvals

`"approvals"` sets the layers of [approvals](effects.md#approvals) the server holds: `admin`, the administrator's locked policy, which nothing below it can loosen and which alone may change how deletions are treated; and `people`, each principal's own policy by name. A seat's policy comes with the request (`provision.approvals`).

```json
"approvals": {
  "admin": {"rules": {"gmail:*": "ask", "Bash": "block"}, "classes": {"irreversible": "stage"}, "deletion": "ask"},
  "people": {"ana": {"rules": {"github:issues.*": "allow"}}}
}
```

## Model gateway and ceilings

`"models"` in the configuration file is `[models]` of `branchyard.toml` as JSON ([model gateway](model-gateway.md#configuration)): a branch on the gateway gets one for each turn, in the process that runs the turn. A backend's `key` names an entry of `"secrets"`, else a variable of the server's own environment. Its turns' tokens are signed with the connector gateway's key and issuer when `"connectors"` is set, else with `<data_dir>/gateway/key` and `http(s)://<listen>`; usage records name the branch `<repo>/<branch>`. A request on the gateway to a server without `"models"` backends fails its turn naming it. Its `"audit"` (`"digests"` by default, or `"contents"`) is the key of [interception records](model-gateway.md#interception-records): with `"contents"` the server writes each call's request and response, as plain text, under the served repository's `.branchyard/models/audit/`.

`"ceilings"` caps what each principal's branches may reach, whatever a request asks for: the connectors (in `--connector` form), the models (globs) and the hosts (`--network` rules). Each turn's access is its request within its principal's ceiling, and an `access` event says what the ceiling narrowed ([one scope](model-gateway.md#one-scope)).

```json
"models": {
  "backends": {"anthropic": {"api": "anthropic", "key": "anthropic"}},
  "routes": [{"model": "claude-*", "backends": ["anthropic"], "requests_per_minute": 600}],
  "budget": {"monthly_usd": 1000, "alert_at": 0.8}
},
"secrets": {"anthropic": "ANTHROPIC_API_KEY"},
"ceilings": {"ci": {"models": ["claude-haiku-*"], "network": ["api.github.com:443"]}}
```

Model calls are counted on `/metrics` as `branchyard_model_calls_total`, `branchyard_model_tokens_total` and `branchyard_model_cost_usd_total`.

## API reference

All bodies are JSON (`Content-Type: application/json` is required on `POST`, else `415`). Branches, statuses and recorded events use the SDK's own serde forms, the same values `Yard::branches` and `Branch::events` return. Every response carries `x-request-id` (the caller's, when it sends a usable one).

| Method and path | Does | Returns |
|---|---|---|
| `GET /healthz` | Liveness, no token | `ok` |
| `GET /.well-known/jwks.json` | The keys this server's tokens verify against (the connector gateway's, and an AIPlex backend's with `auth = "turn-token"`), no token; `404` with neither | a JSON Web Key Set |
| `POST /v1/turns/token` | A running turn's sandbox fetches its five-minute gateway token with the turn's secret, and its workload identity where the server lists any; no bearer token of the server's ([turn tokens for sandboxes](#turn-tokens-for-sandboxes)) | `{"token", "expires_at", "expires_in"}` |
| `GET /v1/repos` | Served repositories | `{"repos": [{"name", "root"}]}` |
| `GET /v1/harnesses` | Harness profiles, as found on the server's `PATH` | `{"harnesses": [{"harness", "profile", "default", "available", "qualification"}]}` |
| `GET /v1/inventory` | Live workers serving repositories the caller can see, this server first, with the harnesses each one's machine has ([worker inventory](#worker-inventory)); `read` scope | `{"workers": [{"id", "host", "labels", "repos", "seen_ms_ago", "this", "inventory"}]}` |
| `GET /admin`, `GET /admin/admin.js`, `GET /v1/admin/compute`, `POST /v1/admin/compute/plan` | The [admin compute console](admin-console.md): its public page, every execution preset read as compute with the live workers that can claim it, and setup plans for GCE or GKE compute; `admin` scope for both routes | the page; `{"origin", "targets", "workers"}`; `{"steps", "label", "allow_provider", "preset", "notes"}` |
| `GET /v1/problems` | What background work could not do that a person can act on, oldest first; empty when nothing needs a person ([problems](#problems)) | `{"problems": [{"kind", "key", "what", "since_ms", "failures"?, "error", "next"}]}` |
| `POST /v1/repos/{repo}/tasks` | Run a task on one branch, or one branch per harness | `202` operation |
| `GET /v1/repos/{repo}/branches` | Every branch, oldest first | `{"branches": [BranchInfo]}` |
| `GET /v1/repos/{repo}/branches/{b}` | One branch | `BranchInfo` |
| `GET /v1/repos/{repo}/operations?branch=B` | The caller's tenant's queued and running operations of the repository, oldest first, or only those working on `B` (one that will create it included), each with `requires` and, when no live worker can claim it, `waiting` | `{"operations": [operation]}` |
| `POST /v1/repos/{repo}/branches/{b}/send` | Continue its session with another prompt | `202` operation |
| `POST /v1/repos/{repo}/branches/{b}/fork` | New branch from its candidate, or from its checkpoint `at` ([checkpoints](checkpoints.md)) | `202` operation |
| `POST /v1/repos/{repo}/branches/{b}/reincarnate` | New branch from its candidate, always a fresh session, with a generated handoff brief | `202` operation |
| `POST /v1/repos/{repo}/branches/{b}/merge` | Validated merge of its candidate | `202` operation |
| `POST /v1/repos/{repo}/branches/{b}/check` | Run `b`'s check (its own, or the one it inherited) on its current work, merged into its parent's branch as integrating it would, with nothing integrated, as `by check b`; `{}`; `run` scope; holds no branch lock | `202` operation; its `result.check_report` is a `CheckReport` |
| `POST /v1/repos/{repo}/branches/{b}/cancel` | Stop its running turn and every running turn delegated below it | `{"cancelled": ["b", …]}` |
| `POST /v1/repos/{repo}/branches/{b}/steer` | Add input to its running turn without interrupting it | `Steer`: `{id, branch, by, text, requested_at_ms, state}` |
| `POST /v1/repos/{repo}/branches/{b}/discard` | Set settled `b` aside, as `by discard b`; `{reason?}`, the caller named when omitted; `run` scope; `409 running` while a turn runs, `409 branch_busy` while an operation holds it | `Inspection` |
| `POST /v1/repos/{repo}/branches/{b}/rewind` | Reset settled `b`, its worktree and candidate to checkpoint `to`, as `by rewind b --to N`; `{to}`; `run` scope; not an operation: it holds `b` in the operation store while it runs; `409 running` while a turn runs, `409 branch_busy` while an operation holds it, `403 denied` for a merged branch or over worktree changes no checkpoint has, `404 unknown_checkpoint` | `Rewound`: `{branch, from, to, commit, session}` |
| `POST /v1/repos/{repo}/wait` | Block until branches settle, as `by wait`; `{branches, any?, timeout_seconds?}`, the timeout capped at 30s on the server (`by --remote wait` asks again past it); a branch a queued or running send, task or spawn will run a turn of is pending until that turn has run; `read` scope | `Waited`: `{settled: [Inspection], pending, timed_out?}` |
| `DELETE /v1/repos/{repo}/branches/{b}` | Remove worktree and record | `{"removed": "b"}` |
| `GET /v1/repos/{repo}/branches/{b}/diff` | Candidate diff against the base | `{"diff": "..."}` |
| `GET /v1/repos/{repo}/branches/{b}/changes` | The candidate against the base file by file: each file's path, status, `+`/`−` counts and numbered hunks, binary files named and not shown, bounded with truncation markers; `read` scope ([reviewing a branch's diff](#reviewing-a-branchs-diff)) | `Changes` |
| `GET /v1/repos/{repo}/branches/{b}/reviews` | The reviews sent to `b` by `by review` and the console, each comment marked `addressed_in` the candidate that changed its lines since; `read` scope | `Reviews` |
| `POST /v1/repos/{repo}/branches/{b}/reviews` | Comments written on `b`'s diff, sent as one prompt like `by review`: `{comments: [{path, line, start_line?, side?, body}], note?, commit?, delivery?}`; `delivery` is `next_turn` (a send) or `steer`; `run` scope; `409 stale_candidate` when `commit` is not `b`'s candidate | `ReviewSent`: `{delivery, comments, prompt, operation?, steer?}` |
| `GET /v1/repos/{repo}/branches/{b}/events?cursor=N` | Recorded events after the first `N` (default 0); a branch's events are numbered from 1. A turn whose spend something measured carries its own cost as `{"turn_cost": {"cost_usd"}}`, before its closing status ([budgets](delegation.md#budgets)) | `{"events": [RecordedEvent], "cursor": M}`; pass `M` next |
| `POST /v1/repos/{repo}/branches/{b}/spawn` | A child of `b`, as `by spawn --parent b` makes one; needs `--allow-delegation` | `202` operation |
| `POST /v1/repos/{repo}/branches/{b}/integrate` | Merge delegated child `b` into the branch that delegated it, as `by integrate b` | `202` operation |
| `GET /v1/repos/{repo}/branches/{b}/inspection` | `b` as a delegating parent sees it, as `by inspect b --json` | `Inspection` |
| `GET /v1/repos/{repo}/branches/{b}/event-page?cursor=N&limit=M` | Up to `M` (default 50, at most 200) events after the first `N`, or the most recent without a cursor, as `by events b --json` | `EventPage` |
| `GET /v1/repos/{repo}/branches/{b}/timeline` | The shape of `b`'s work with none of its content: each turn's start, end, outcome, own cost and tokens, how it started (`startup`), its tool calls by time and name, the points inside it (`marks`: a steer, a permission asked and answered, a stall, a delegation, a judge's score), each with its event's number in `b`'s log, and when `b` merged, read from its whole event log ([reading a branch](#reading-a-branch)) | `Timeline` |
| `GET /v1/repos/{repo}/branches/{b}/children` | Every branch `b` delegated to, directly or below, as `by children b --json` | `Children` |
| `GET /v1/repos/{repo}/branches/{b}/graph` | `b`'s children, the dependencies among them and its graph revision, as `by graph show b --json` ([task graphs](graph.md)) | `Graph` |
| `POST /v1/repos/{repo}/branches/{b}/graph` | Apply a graph proposal `{expected_revision, edits, policy?, unapproved_tools?}` to `b`'s children, as `by graph apply --parent b`; a proposal that spawns needs `--allow-delegation`; a stale revision is `409 stale_revision` with `{expected, actual}` in `detail` | `200` `GraphApplied` |
| `GET /v1/repos/{repo}/branches/{b}/inbox` | Every message addressed to `b`, oldest first, as `by inbox --as b --json` | `Inbox` |
| `GET /v1/repos/{repo}/inbox` | The person's inbox: every question and escalation sent to `@person` by a branch nobody delegated, oldest first, as `by inbox --json` outside a harness ([delegation](delegation.md#the-persons-inbox)); `read` scope | `Inbox` |
| `POST /v1/repos/{repo}/answer` | Answer question or escalation `message_id` as whoever it was sent to, the person or a branch, as `by answer ID TEXT` outside a harness; `{message_id, text}`; `run` scope; empty text is `400 invalid_request`. An asker whose turn ended reads it at its next turn | `Message` |
| `POST /v1/repos/{repo}/branches/{b}/ask` | Ask `b`'s parent a question, as `by ask --as b`; `{text, wait_seconds?, key?}`, the wait capped at 120s on the server; a `key` makes a repeat return the first question, and the same key with other text is `403 denied` | `Asked` |
| `POST /v1/repos/{repo}/branches/{b}/report` | Report to `b`'s parent, as `by report --as b`; `{text, result?}`: a `result` is checked against `b`'s [result contract](delegation.md#result-contract) and recorded, a mismatch is `403 denied` naming the path and nothing is recorded | `Message` |
| `POST /v1/repos/{repo}/branches/{b}/escalate` | Escalate to `b`'s parent, or further up if its rig seat allows, as `by escalate --as b`; `{text}` | `Message` |
| `POST /v1/repos/{repo}/branches/{b}/report-status` | State `b`'s own status beside the protocol's, as `by report-status --as b`; `{state, summary, link?, ttl_seconds?}`; `run` scope; `400 invalid_request` with `detail.field` for a field that breaks a rule | `Reported`: `{report, escalation?}` |
| `POST /v1/repos/{repo}/branches/{b}/answer` | Answer one of `b`'s own descendants' messages, as `by answer --as b`; `{message_id, text}` | `Message` |
| `POST /v1/repos/{repo}/branches/{b}/message` | Message one of `b`'s siblings as `b`, as `by message --as b`; `{to, text, kind?, payload?, key?, deadline_ms?}`; `run` scope; a `deadline_ms` already past is `403 denied`, and a recipient with 32 undelivered peer messages refuses another the same way; a recipient that is not a sibling, an unknown kind or a payload its schema refuses is `403 denied`; empty text is `400 invalid_request` ([messaging](messaging.md)) | `Message` |
| `GET /v1/repos/{repo}/events/stream?cursor=N` | SSE of activity across branches after feed position `N`; without a cursor, from now | `text/event-stream` |
| `GET /v1/operations/{id}[?wait=SECONDS]` | An operation's status and result, on any server sharing the operation store. With `wait`, a long poll: the answer comes as the operation finishes, or once that many seconds pass (capped at 30s, like `POST …/wait`), with the operation as it then is; a `wait` that is not a number of seconds is `400 invalid_request` with `detail.field` `wait` | operation |
| `GET /v1/operations?idempotency_key=K` | The operation the caller's request with key `K` created, for a client that lost the response | operation, or `404 unknown_operation` |
| `POST /v1/repos/{repo}/branches/{b}/artifacts` | Publish the body's bytes as a new artifact of `b`, acting with the server's authority as a person, like `by artifact publish --branch`. `name`, `media_type`, repeated `label` are query parameters; refused over `--max-artifact-bytes` | `201` `ArtifactRef` |
| `GET /v1/repos/{repo}/branches/{b}/artifacts` | Every artifact `b` may read | `{"artifacts": [ArtifactRef]}` |
| `GET /v1/repos/{repo}/branches/{b}/artifacts/{id}` | Artifact `id`'s provenance, checked against `b`'s grant | `ArtifactRef` |
| `GET /v1/repos/{repo}/branches/{b}/artifacts/{id}/content` | Its bytes, with a digest header | streamed bytes |
| `GET /v1/repos/{repo}/branches/{branch}/artifacts/{id}/preview` | Derive an authorized, bounded document or table preview; see [document previews](artifact-previews.md) | preview JSON or a structured readiness error |
| `GET /v1/repos/{repo}/branches/{branch}/artifacts/{id}/preview/assets/{asset}` | Read a derived PNG after checking the original artifact's current grant | PNG bytes |
| `POST /v1/repos/{repo}/branches/{b}/artifacts/{id}/share` | Share it with `{"to": "BRANCH"}` | `{"ok": true}` |
| `POST /v1/repos/{repo}/branches/{b}/scratch` | Create `{"name": "NAME"}`, owned by `b` | `ScratchArea` |
| `GET /v1/repos/{repo}/branches/{b}/scratch` | Every scratch area `b` may reach | `{"areas": [ScratchArea]}` |
| `POST /v1/repos/{repo}/branches/{b}/scratch/{name}/share` | Share it with `{"to": "BRANCH"}` | `{"ok": true}` |
| `POST /v1/repos/{repo}/branches/{b}/scratch/{name}/lock` | Acquire its writer lock for `b` | `ScratchLock`, or `409 running` |
| `POST /v1/repos/{repo}/branches/{b}/scratch/{name}/unlock` | Release it if `b` holds it | `{"ok": true}` |
| `POST /v1/triggers` | Create a [trigger](triggers.md) (`run` on its repository). Its `deliver` makes it continue one branch run after run (`{"branch", "busy"}`), and `deliver.lifecycle` (`{"idle_minutes", "dormant_days", "delete_days"}`, each optional, at least 1, rising) parks, retires the session of, and deletes that branch as it rests ([lifecycle](triggers.md#lifecycle)); a lifecycle that breaks its rules is `400 invalid_request` naming the field | `201` `TriggerCreated` |
| `GET /v1/triggers`, `GET /v1/triggers/{t}` | The tenant's triggers, or one by name or ID | `TriggerList`, `Trigger` |
| `DELETE /v1/triggers/{t}`; `POST /v1/triggers/{t}/enable`, `/disable`, `/secret`, `/test` | Remove; enable or disable; set or generate its webhook secret; evaluate it without creating anything | `TriggerRemoved`, `Trigger`, `SecretSet`, `TriggerTest` |
| `GET /v1/triggers/{t}/runs?limit=N` | Its runs, newest first | `TriggerRuns` |
| `POST /v1/triggers/{id}/fire` | A webhook delivery from GitHub, Slack, Linear or a generic sender; no bearer token, the trigger's signature instead | `FireAck`, or Slack's `{"challenge"}` |
| `GET /app/`, `/app/{file}` | The [web companion](companion.md)'s page, no token, only with `--app` | its files, with a strict CSP |
| `POST /app/pair` | Redeem a pairing code `{code, device?}`, no token, single use, at most 10 attempts a minute | `Paired` (`{token, me}`), or `400 invalid_pairing_code`, `429 rate_limited` |
| `GET /v1/app/me` | The caller's own principal | `Me` |
| `GET /v1/app/push`; `POST`, `DELETE /v1/app/push/subscriptions`; `POST /v1/app/push/test` | The caller's own Web Push subscriptions (`read`) | `PushInfo`, `PushResult` |
| `GET /v1/repos/{repo}/scratch/{name}/lock` | Its current holder, if any (needs `read` on a repository the caller's tenant owns; not scoped to an acting branch) | `{"lock": ScratchLock?}` |
| `GET /v1/repos/{repo}/branches/{b}/plan` | `b`'s plan and its phase ([plans](plans-and-goals.md)) | `PlanInfo` |
| `POST /v1/repos/{repo}/branches/{b}/plan/approve` | Approve `b`'s plan, as proposed or `edited`, and run it as its next turn with `send`'s options; `409 no_plan` when none awaits approval | `202` operation |
| `POST /v1/repos/{repo}/branches/{b}/plan/reject` | Reject `b`'s plan with a `reason`: it ends, or with `replan` plans again | `202` operation |
| `GET /v1/repos/{repo}/knowledge?status=S` | The repository's [knowledge](knowledge.md) entries, or those of status `S` | `{"entries": [KnowledgeEntry]}` |
| `POST /v1/repos/{repo}/knowledge` | Add `{text, scope?, propose?}`, adopted by the caller unless `propose` (`run`) | `KnowledgeEntry` |
| `GET /v1/repos/{repo}/knowledge/{id}`, `DELETE …` | One entry; remove it (`run`) | `KnowledgeEntry` |
| `POST /v1/repos/{repo}/knowledge/{id}/adopt`, `/reject` (`{reason?}`), `/edit` (`{text?, path?, kind?}`, `""` clearing) | A person's decision, as the caller (`run`) | `KnowledgeEntry` |
| `GET /v1/repos/{repo}/memory?scope=repo\|mine&status=S&limit=N&after=ID` | The [memory](memory.md) rows consolidation proposed and people decided: the caller's tenant's team memory of the repository (`repo`, the default) or the caller's own (`mine`), or those of status `S` | `{"memories": [MemoryRecord]}` A page: `limit` (200 by default, 1,000 at most) rows above `after`, with `next` on the answer while more follow. |
| `GET /v1/repos/{repo}/memory/{id}`, `GET …/memory/{id}/evidence` | One row; the row and the observations it came from, each with its author. Another person's row is `denied`; an administrator reads `[private: N characters]` in its place | `MemoryRecord`; `{"memory", "observations"}` |
| `POST /v1/repos/{repo}/memory/{id}/approve`, `/reject` (`{reason?}`), `/edit` (`{title?, body?, kind?, paths?, task_kinds?}`); `DELETE /v1/repos/{repo}/memory/{id}` | A person's decision, as the caller (`run`): the team's rows need `admin` or a name in the server's `memory.approvers` (`denied` otherwise); a person's own row is theirs alone, and its approval here is `unsupported` until the registry publishes their bundle (M4) | `MemoryRecord` |
| `POST /v1/repos/{repo}/memory/{id}/promote` | Offer the caller's own row to the team: a new proposed row (`run`) | `MemoryRecord` |
| `POST /v1/repos/{repo}/memory/consolidate` (`{scope?}`), `GET /v1/repos/{repo}/memory/runs?scope=` | One deterministic consolidation now (`run`); the runs, newest first (`read`) | `Consolidated`; `{"runs": [MemoryRun]}` |
| `GET /v1/repos/{repo}/knowledge/export` | The adopted entries as an `AGENTS.md`-style file | `{"markdown", "entries"}` |
| `POST /v1/repos/{repo}/branches/{b}/distill` | Propose entries from `b` with the deterministic extractor (`run`) | `Distilled` |
| `GET /v1/repos/{repo}/approvals[?all=true]` | The [approvals](effects.md#approvals) waiting, or every one | `ApprovalList` |
| `GET /v1/repos/{repo}/decisions` | What waits for a person, in the order to take it: one decision for each branch whose [stage](concepts.md#what-a-branch-needs-from-you) is in Needs you or Ready. First each running branch parked on an [approval](effects.md#approvals) or with an unanswered question or escalation (a question to a parent counts while its asker runs, one to the person until its asker is merged or discarded, listed with the stopped ones, `nobody`, once its asker's turn ended), oldest wait first, then each branch whose plan awaits approval (`plan`) (`waits: agent`); then the ready branches (`merge`, `delivery`); then the failed, interrupted, over-budget and blocked ones (`stop`) and the stalled ones (`stall`), oldest branch first (`nobody`). A delegated child that settled while its parent still works is not listed: its parent decides. The [console](companion.md)'s Decide view | `DecisionList` |
| `GET /v1/repos/{repo}/ground` | The ground plan, which files are in whose hands: every path an open branch's candidate changed (open: not merged, not discarded, not a goal judge's `judge-…` scratch branch), sorted, each with the open branches that changed it and their status; `skipped` names an open branch whose diff could not be read, which never fails the request. Computed when asked by reading every open branch's diff, so a call costs what those reads cost; nothing is stored. The [console](companion.md)'s Ground tab and merge card | `Ground` |
| `GET /v1/projects/{project}/decisions` | What waits for a person in a project's workspaces, in the same order and shape (`DecisionList`): a session whose plan waits for approval (`waits: agent`); then each `output` waiting to be accepted (`delivery`: the file's `output` id, name, media type and size, the `session` that produced it, oldest first), which `POST /v1/workspaces/{workspace}/finalize` accepts and `GET …/artifacts/{id}/preview` shows; then each `session` that failed, was interrupted or never started (`nobody`, with its `phase` text). An output counts while its session's current turn finished with a valid deliverable result, nothing more is queued for the session and no acceptance names it, the rule `finalize` applies; a session that failed or never started counts only while no newer session exists in its workspace (starting another is how a person moves on), and a repository project lists outputs only, because its sessions are branches of its repository and that repository's decisions list them. Only workspaces the caller is a member of and that are `ready` count. Needs the `read` scope | `DecisionList` |
| `POST /v1/repos/{repo}/approvals/{id}/allow`, `/deny` (`{reason?, surface?}`) | Answer one as the caller (`run`); `surface` is `api` (default), `companion` or `watch` | `ApprovalAsk` |
| `GET /v1/repos/{repo}/effects[?branch=B]`, `GET …/effects/{id}` | The [effect ledger](effects.md), or one entry and its events | `EffectList`, `EffectDetail` |
| `POST /v1/repos/{repo}/effects/{id}/promote`, `POST …/effects/reconcile` | Perform a staged effect as the caller; reconcile the ledger (`run`) | `EffectEntry`, `Reconciled` |
| `GET /v1/repos/{repo}/branches/{b}/undo?to=N`, `POST …` (`{to, only?}`) | The undo plan for turns after `N`; perform the chosen inverses as the caller (`run`), files untouched | `UndoPlan`, `UndoReport` |

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

A task may also carry `"plan": true` (plan first, read-only, then wait for `…/plan/approve`) and `"goal": {"text", "rounds"?, "judge"?}` (a goal a judge verifies, `judge` naming one of the server's harnesses, launched with its `harness_commands` entry); see [plans and goals](plans-and-goals.md).

Only `prompt` is required. Give `harness` for one branch or `harnesses` for one branch each (`<name>-<harness>`), not both. `policy.mode` is `allow` or `deny` (the default); rules apply first, in order. There is no remote `ask`: the server has no terminal to ask on. `budget.max_seconds` applies per call, like the SDK's `max_duration`. `budget.stall_after_seconds` and `budget.stall_action` (`notify`, the default, or `interrupt`) are the SDK's `Budget::stall_after`/`stall_action` (`docs/lifecycle.md#stall-detection`); a branch's `stalled` field then reflects the same live state a local `by ls` would show. `command` is refused (`403 command_not_allowed`) unless the server allows client commands; without it, the server uses its `harness_commands` entry for the harness, else the profile's executable on its `PATH`. One task runs one command, so harnesses with different configured commands must be separate tasks.

`priority` (task, send, fork, reincarnate and spawn) is the operation's priority, an integer from -10 to 10, default 0; outside that range is `400 invalid_request`, and above the tenant's `max_priority` it is admitted at `max_priority`. A spawn that names none inherits the priority of the latest operation that worked on its parent. Merges and integrations run at 0. See [scheduling](#scheduling).

`provider` is [`branchyard::Provider`](../crates/branchyard/src/lib.rs)'s serde form, tagged by `kind`: `local`, `microsandbox` or `substrate`, with [the provider's options](providers.md). Anything but `local` is refused (`403 provider_not_allowed`) unless the operator allowed that provider. Everything in it is about the server: `pass_env` names are read from **the server's environment** at each turn, a Substrate `key` must be an absolute path to a file on the server (`400` otherwise), and a Microsandbox provider needs a server built with the `microsandbox` feature on a host with KVM.

`provision` is [`branchyard::Provisioning`](provisioning.md)'s serde form: `secrets` (names only, `[{"name": "ANTHROPIC_API_KEY"}]`; a `from` is refused with `400`, and a name the operator did not define with `403 secret_not_allowed`; values come from the server's table), `auth`, `mcp_servers` (commands the server runs: refused unless it allows client commands), `instructions`, `model`, `effort`, `telemetry` and `connectors` (grant entries, [connectors](#connectors); refused `403 connectors_not_configured` without them). It is accepted on task, send and fork requests and stored with the branch, with the server's sources.

`delegation` is an [`Envelope`](delegation.md#the-envelope), `{max_depth, max_children, harnesses}`, all three required; with it the harness gets the delegation tools, through the server's `by`, for each of its turns. `allow_delegation` adds the rule that allows the harness's shell commands running that `by` with a delegation subcommand, after the policy's own rules, like `by --allow-delegation`. Either is refused (`403 delegation_not_allowed`) unless the server allows delegation. `seats` makes the branch a [rig](rigs.md)'s root, in [`Seats`](../crates/branchyard/src/seats.rs)'s serde form (`{rig, seat, delegates_to, table}`); it needs `delegation`, is refused like it without the opt-in, and each seat's `provision` is held to the rules above, so its secrets come from the server's table. `by --remote rig run` sends it. `unapproved_tools` runs a profile whose driver cannot route tool approvals, like `by --allow-unapproved-tools`, and is refused (`403 unapproved_tools_not_allowed`) unless the server allows it.

`send` takes `prompt` or `retry: true` (the prompt of the branch's last turn that was cut off, as `by send --retry`; `400 invalid_request` with both or neither, and `403 denied`, before anything is queued, when no turn was cut off), `budget`, `policy`, `check`, `command`, `delegation`, `allow_delegation` and `unapproved_tools`; the branch keeps its harness, provider, recorded command, environment and envelope. A server that does not allow delegation refuses to send to a branch that was given an envelope, since its harness would get the tools. `fork` takes `prompt`, `name`, `fresh_session`, `at` (fork from that checkpoint, 0 being the base, as `by fork --at N`: refused `404 unknown_checkpoint` before anything is queued when the branch has no such checkpoint, and `400 invalid_request` with `fresh_session`), `harness`, `budget`, `policy`, `check`, `isolated`, `command`, `provider`, `delegation`, `allow_delegation` and `unapproved_tools`; without a provider it keeps its parent's. `reincarnate` (`docs/lifecycle.md#reincarnation`) takes the same fields as `fork` except `prompt` and `fresh_session` (there is no prompt to give: the engine generates a handoff brief, and the session is always fresh); without `harness` or `provision.model` it keeps its parent's, so a plain call reincarnates onto the same configuration with a fresh session. `merge` takes an optional `target`, defaulting to the branch checked out in the served repository.

`spawn` takes `prompt`, `harness`, `name`, `base`, `budget`, `policy`, `check`, `max_depth`, `deny`, `unapproved_tools`, `seat`, `depends_on`, `after`, `bindings`, `connectors` (narrowed to the parent's grant), `max_children`, `harnesses`, `plan`, `model` (refused `unsupported` for a harness whose driver cannot choose one; [models](delegation.md#models)) and `result` (the child's [result contract](delegation.md#result-contract), `{schema, required?}`; a schema keyword outside the contract's subset is refused `403 denied` before the operation is admitted), the flags of `by spawn`; with a `seat` and no `name`, the child is named `<parent>-<seat>` before the operation starts. It acts with the server's authority as a person, as `by spawn --parent` does locally: the parent's envelope bounds the child exactly as it bounds a local spawn, and a parent without delegation cannot spawn. The operation, queued like every other with the whole request as its description, runs the child's first turn, waits for the parent's subtree on its worker as the local command does, and its result holds the child's `inspection`; a child with `depends_on` that has not settled is created `waiting`, the operation finishes with it `waiting`, and it starts later where its prerequisite settles or is integrated. It locks the parent as well as the child, as `integrate` locks both branches, so a send to the parent or its removal is refused with `409 branch_busy` until the operation finishes. `integrate` takes `{}` and refuses (`403 denied`) a branch no other branch delegated; its result holds `merged`. `POST …/graph` is not an operation: it commits the proposal in one store transaction before it answers, starts the children that have nothing to wait for on the server's threads, and returns; their siblings' turns start the rest as they settle, under the proposal's `policy`, and the recovery interval of every server and `by worker` on the store starts any whose prerequisite settled while no engine could (after a restart, under the default policy, deny); with several on one database the store's claim lets one of them start it ([task graphs](graph.md#with-the-servers-queue)). A retry of a request that committed is refused `409 stale_revision`, so it never applies twice. The inspection, event page and children reads act with the same authority and need no opt-in. So do the messaging routes: `inbox` reads `b`'s own inbox; `ask`, `report`, `escalate` and `answer` act as `b` with the server's authority, following the same delegation-tree rules as locally ([delegation](delegation.md#inbox)), and need no opt-in either, since none of them runs a turn. `ask`'s `wait_seconds` blocks the request handler for up to that long (capped at 120s server-side; poll `inbox` for longer) using the same store wait a local `ask --wait` does.

`cancel` takes an empty object, `{}`. It is not an operation: it records a durable cancel request for the branch's running turn and each running turn delegated below it, and answers `200` with the branches that were running (an empty list when none was, and for a repeat). The engine running each turn, in the server or in another process on the repository such as a local `by run`, observes the request within 100 ms, interrupts the harness, and ends the branch `interrupted`; the operation that ran the turn then succeeds with that status. The branch's log records `cancelled by <token name> through the server`; the token a server makes for itself, and the first `--token-file`, are named `default`, which the log names `the default token` (`cancelled by the default token through the server`). It ignores branch locks, since the branches it is for are the ones an operation holds, and needs no idempotency key.

`steer` takes `{"text": "..."}`. Like `cancel`, it is not an operation and ignores branch locks: the running turn's operation holds them, and the input is for exactly that turn. It queues the input durably, bound to the branch's running turn ([durability](durability.md#steered-input)); the engine running that turn, in the server or in another process on the repository, writes it to the harness within 100 ms through the harness's own mid-turn input ([which harnesses, and when the model sees it](harness-integration.md#steering-a-running-turn)). The answer waits up to 10 seconds for that and returns the `Steer`, whose `state` is `{"state": "accepted" | "written" | "pending"}` or `{"state": "refused", "reason": "..."}` ([what each means](delegation.md#steering-a-child)), and `boundary`, where the harness delivers it within the turn. It is refused with `409 not_running` when no turn runs, and `422 unsupported`, with the reason, when the branch's harness cannot take input mid-turn; it never interrupts the turn instead. The branch's log records `steered` with `by` `<token name> through the server`. Budgets and permissions are the running turn's own. `by --remote send --steer` calls it.

`check` takes an empty object, `{}`, and runs what `by check` runs: the branch's check on its current work, the way integrating it would, in a private temporary worktree where its files as they are (uncommitted ones included) are merged into its parent's branch, with nothing integrated and no turn run. It can run for as long as an integration's check (30 minutes), so it is an operation like a merge, with an idempotency key, and the same `GET /v1/operations/{id}` finds it. Its `kind` is `check`, and its `result` has `branches` (the branch) and `check_report`, the engine's `CheckReport` as `by check --json` prints it: `{branch, check, inherited_from?, work, target?, checked?, outcome, conflicts?, output_tail}`, where `outcome` is `passed`, `failed`, `timed_out`, `not_started` or `not_run` (there is no check, or `conflicts` names the files the work conflicts with and the check did not run). A check that fails is a *succeeded* operation whose report says so, as a branch that ends `failed` is; the operation fails only when the check could not be run at all (a git error, say). `by --remote check` waits for the operation and exits 1 when the report is not a pass; interrupting it stops waiting, not the check (the operation is found again by its key, or by `GET …/operations?branch=b`). Unlike a merge it takes no branch lock, as the local command takes none: a turn, a send or a wait goes on beside it, and a wait for the branch does not wait for the check, which runs no turn. It counts toward the tenant's `max_running` and is not refused by it, runs at priority 0, and needs the `run` scope, since it runs the branch's check command on the server. A branch that does not exist is `404 unknown_branch`, before anything is queued.

`report-status` takes `{"state", "summary", "link"?, "ttl_seconds"?}` and does what `by report-status --as b` does: the engine records the report on `b` as a status report and answers `Reported`, `{report: {state, summary, link?, at_ms, ttl_seconds?, expires_at_ms?}, escalation?}`, the report as recorded and, for `blocked` and `failure`, the `Message` it escalated to `b`'s parent, unless it repeats the report before it exactly ([self-reported status](delegation.md#self-reported-status)). It is not an operation: it is quick and answers directly, as `escalate` does. The rules are the tool's: `state` is `working`, `idle`, `blocked`, `complete` or `failure`, `summary` is one line of at most 280 characters once trimmed, and `link`, when given and not blank, is an `http` or `https` URL of at most 2048 bytes, and `ttl_seconds`, when given, is a whole number of seconds from 1 to 604800 (7 days), after which the report reads as stale. A field that breaks one is `400 invalid_request` whose `detail.field` is `state`, `summary`, `link` or `ttl_seconds` (the first to fail, in that order) and whose message says the rule; a missing field or an unknown one is `400 invalid_request` with no `detail.field`, and nothing is recorded. It needs the `run` scope and a repository the caller may use, as `escalate` does, and acts as a person with the server's authority; the harness of a branch the server runs states its own status through its broker, not this route, and `inspection` shows the latest report as `reported`.

### Operations

Task, send, fork, reincarnate, merge, spawn, integrate and check are long operations. The server validates the request, admits the operation durably (see [dispatch](#dispatch)), and answers `202 Accepted` with `Location: /v1/operations/{id}` before the work starts:

```json
{ "id": "op_…", "repo": "app", "kind": "task", "state": "queued",
  "branches": ["flaky"], "cursor": 41, "created_at_ms": 1790000000000 }
```

`kind` is `task`, `send`, `fork`, `reincarnate`, `merge`, `spawn`, `integrate`, `approve_plan`, `reject_plan`, `map` or `check`. `priority` is the priority it was admitted at, left out when 0. An operation whose request named `require_labels` carries them as `requires` (sorted, once each), and, once it has waited queued longer than `unclaimable_after` and no live worker serving its repository carries them all, `waiting` says why ([worker labels](#worker-labels)). `state` moves through `queued`, `running`, then `succeeded`, `failed` or `interrupted`. A finished operation adds `finished_at_ms`, `end_cursor`, and either `result` or `error` (`{"code", "message", "detail"}`). `result` has `branches` (`[BranchInfo]`); `merged` (`{"branch", "target", "previous", "commit"}`) for a merge or integration; `descendants` (`[BranchInfo]`), every branch the operation's branches delegated to, once they finished, since a task, send or fork waits for its subtree as `by run` does; `inspection` for a spawn; and `check_report` for a check. A branch that ends `failed` or over budget is a *succeeded* operation whose branch status says so, exactly as the SDK returns `Ok(branch)`; an operation fails when the SDK call returns an error, such as an unknown harness or a refused merge.

To wait for one, ask `GET /v1/operations/{id}?wait=30` and ask again while it is not finished: the server answers as the operation reaches `succeeded`, `failed` or `interrupted`, woken at once when a worker in its own process records the outcome, and within its `poll_interval` when another worker on the store ran it. The Rust client's `Client::wait` and `wait_timeout`, Python's `client.wait`, and `by --remote` wait this way rather than polling.

`branches` are the names planned at acceptance. `cursor` is the feed position at acceptance and `end_cursor` the position once the operation's activity was ingested: to watch one operation, stream from `cursor` until the operation finishes and the stream reaches `end_cursor`, keeping entries for its branches. That is what `by --remote … run` does.

From admission until it finishes, the branches an operation works on are locked: another send, merge, fork into the same name, or removal gets `409 branch_busy`. A check is the exception: it locks nothing. The locks are rows in the operation store, taken in the admission's transaction and released in the one that records the outcome, so they hold across every server sharing the store; a removal holds its branch the same way for as long as it takes (at most 10 minutes if its server dies meanwhile). A local `by` on the same repository does not see them.

### Dispatch

Admission is a durable enqueue. In one transaction the server writes the operation's record (with its tenant and admitting principal), its idempotency binding (a unique index on the caller and key), checks the tenant's `max_running` and `max_branches` against the tenant's queued and running operations ([quotas](#quotas)), takes its branch locks, and writes a queue row holding a description of the work: the request as sent, plus what admission fixed (planned names, a merge's target, an integration's parent, a seat child's name). Only then does it answer `202`. If a quota refuses it, a branch is held, or any of those writes fails, including the queue write, the transaction rolls back and nothing of the operation remains: the client gets `500` and may retry with the same key. The description holds no secret values; a request names secrets, and the server that runs it reads them, like commands, providers and policies, from its own configuration.

A dispatcher in each server claims queue rows in [scheduling](#scheduling) order (follow-ups, then highest effective priority, then the tenant furthest below its fair share, then the oldest), for the repositories it serves, up to `--max-running` at once, plus one follow-up and one console cancel or steer on slots of their own. A claim carries a lease, renewed every third of `--operation-lease`, and a fence, its attempt number, which every later write for the operation names. The worker records the operation `running` before it calls the engine, and its outcome after, deleting the queue row and releasing the branch locks in the same transaction; a worker whose claim was taken over is refused both writes. A claim whose lease expired, or whose process is gone from this host, is claimed again by any worker: an operation still `queued` then runs, and one recorded `running` is recorded `interrupted` and never run again, since its turn may have started, and the engine recovers that turn's branch as it recovers any whose engine died.

### Scheduling

Which queued operation a worker claims next, among those of its repositories whose required labels it carries:

1. **Follow-ups first.** A follow-up is a request that continues a branch or session that already exists: `POST …/branches/{b}/send`, a plan's approval or rejection (`POST …/branches/{b}/plan/approve`, `…/plan/reject`), and a console session send. It is claimed before every normal operation, whatever their priorities, and each process keeps one slot beyond `--max-running` that only a follow-up may use, taken once the normal slots are full. So a request that sends to an existing branch goes ahead of every new launch, and has one slot per process that nothing else may use. A trigger's send to its branch, a merge, an integration and a check are normal work. Among follow-ups, the rules below decide.
2. **Then highest effective priority.** An operation's effective priority is its `priority` plus one for every `aging_seconds` (default 60) it has waited since admission, so low-priority work cannot starve: an operation at -10 outranks a fresh one at 10 after 21 minutes. `aging_seconds: 0` turns aging off.
3. **Then the tenant with the lowest usage per unit of weight.** A tenant's usage is its operations running now (claimed under a live lease, on any worker) plus its recent claims, each counting `exp(-age / fair_share_window_seconds)` (default 300 s; 0 counts only what runs), divided by the tenant's `weight` (`tenants.<name>.weight`, default 1). Two tenants with weights 2 and 1 and steady work get about two claims for every one; a tenant that has had nothing recently is served first.
4. **Then the oldest**, which also breaks a tie between tenants.

The order is the claim's own: one statement on PostgreSQL, and inside the claim's `BEGIN IMMEDIATE` transaction on SQLite, so the never-twice guarantee of [dispatch](#dispatch) is unchanged. On PostgreSQL it is:

```sql
WITH share AS (                     -- each tenant's usage per unit of weight
  SELECT t.tenant,
    (COALESCE((SELECT u.used * exp(-LEAST(GREATEST($now - u.at_ms, 0)::float8 / $window, 700))
               FROM by_tenant_usage u WHERE u.tenant = t.tenant), 0)
     + (SELECT count(*) FROM by_operation_queue r WHERE r.tenant = t.tenant
          AND r.worker IS NOT NULL AND r.lease_until > clock_timestamp()))
    / COALESCE(weight_of(t.tenant), 1) AS share
  FROM (SELECT DISTINCT tenant FROM by_operation_queue) t),
next AS (                           -- the first claimable row, locked
  SELECT q.id, q.worker AS prior FROM by_operation_queue q JOIN share s USING (tenant)
  WHERE q.repo = ANY($repos) AND (q.lease_until IS NULL OR q.lease_until <= clock_timestamp())
    AND q.requires <@ $labels AND q.lane = ANY($lanes)
  ORDER BY (q.lane = 'follow_up') DESC,
           q.priority + COALESCE(GREATEST($now - q.enqueued_ms, 0) / NULLIF($aging, 0), 0) DESC,
           s.share, q.seq
  LIMIT 1 FOR UPDATE OF q SKIP LOCKED),
claimed AS (UPDATE by_operation_queue q SET attempt = q.attempt + 1, worker = $worker, ...
            FROM next WHERE q.id = next.id RETURNING ...),
counted AS (INSERT INTO by_tenant_usage ... ON CONFLICT (tenant) DO UPDATE  -- decay, then + 1
            SET used = by_tenant_usage.used * exp(...) + 1, at_ms = GREATEST(...))
SELECT ... FROM claimed
```

(abridged from `PG_CLAIM` in [`store.rs`](../crates/branchyard-server/src/store.rs); `weight_of` is an `unnest` of the configured weights). `$lanes` is the capacity the claim is for: a normal slot takes `normal` and `follow_up` rows, the follow-up slot only `follow_up` rows, and the control slot only `control` rows (a console cancel or steer). A row another worker locked is skipped, and one it claimed and committed meanwhile fails the lease condition when PostgreSQL rechecks it on the locked row's latest version, so two workers never claim one row. The usage is a snapshot: two workers claiming at the same moment may both favour the same tenant, which the next claims even out. SQLite computes each tenant's share in Rust inside the immediate transaction, orders by its rank, and updates the usage before committing.

What it does not do: per-tenant ceilings are [quotas](#quotas) (`max_running` still bounds a tenant however low its usage), shares are per database rather than per worker, a running operation is never pre-empted (a follow-up that finds its slot taken and the normal slots full waits for one of them), and a merge, integration or check is ordinary work at priority 0. Ages are measured against the claiming worker's clock (the operation records when it was admitted), so servers' clocks should agree to well under `aging_seconds`; leases stay on the database's clock. Every claim updates its tenant's row in `by_tenant_usage` (`tenant_usage` on SQLite): one row per tenant, never more.

### Reading a branch

A client asks for as much of a branch as it is about to show, in four steps:

1. **The list.** `GET …/branches` gives each branch's status, stage, counts and cost (`BranchInfo`). A sidebar or a count needs nothing more.
2. **The shape.** `GET …/branches/{b}/timeline` gives each turn's span, outcome, cost and tokens, how it started, each tool call's time and name, and the points inside it: a person's steer, a permission asked and the answer, a stall, a delegation and a judge's score (`Timeline`). Each call and mark carries `seq`, its event's number in the branch's log, so a client that wants its content reads that one event with `…/event-page?cursor=seq-1&limit=1`. It carries no prompt, message or tool input, and it covers every turn, however long the branch has run. The console's board draws every row from it, and its [Trajectory page](companion.md#trajectory) draws a branch's waterfall and its helpers' lanes from it.
3. **The conversation.** `GET …/branches/{b}/event-page` gives the events themselves, a page at a time.
4. **The detail.** `GET …/branches/{b}/diff` and `…/inspection` give the change and the branch as its parent sees it; `…/changes` gives the change file by file, for a client that reviews it ([below](#reviewing-a-branchs-diff)).

Each step is a separate request, so a page that shows forty branches reads forty small timelines and no conversation.

A client that also follows the [event stream](#event-stream) need not read a timeline twice. The console reads each branch's timeline once and then applies the streamed events to it, by the rules `Timeline::of` reads the log with: a prompt starts a turn, a `tool_started` adds a call, a `turn_cost` sets the open turn's cost, a `permission_requested`, `decision`, steer, stall or delegation adds a mark, and a status other than running or waiting ends the turn. A call is named by its `call_id` and its `at_ms`, so an event the timeline already held (one that arrived while the timeline was read) changes nothing. A `rewound` or `recovered` event rewrites the turns, and the console reads the timeline again.

### Reviewing a branch's diff

`GET …/branches/{b}/changes` is the candidate against its base as data (`Changes`), read from the same `git diff` as `…/diff` and `by review`: `base`, the candidate's `commit` (none while there is no candidate), `files` in git's order, and the totals `additions` and `deletions`. A file has its `path` (and `old_path` when renamed), `status` (`added`, `deleted`, `modified`, `renamed`), its own counts, `binary` (named, with no hunks) and `hunks`, each with its `@@` `header`, the four numbers of it and its `lines`: `kind` (`context`, `add`, `del`, `no_newline`), `old` and `new` line numbers (an added line has no `old`, a removed one no `new`) and `text`. The answer is bounded: at most 300 files (the rest counted in `omitted_files` and the totals), 2,000 lines kept per file (`truncated` on the file), 20,000 across files (later files list no hunks) and 1,000 characters per line (cut with `…`); `truncated` says anything was left out, and the counts always cover the whole diff. At most 16 MiB of `git diff` output is read (`MAX_DIFF_BYTES`): past that the diff is cut at a whole line, `truncated` is set, and the totals then cover only what was read.

`POST …/branches/{b}/reviews` is `by review` for a client with no editor. Each comment names a `path`, a `line` (or `start_line` to `line`) and the `side` its numbers are on (`new`, the default, for added and context lines; `old` for removed ones), all on lines the diff shows, or the request is refused `400 usage` with the reason. `commit` is the candidate the numbers were read from: if the branch has moved on, `409 stale_candidate` and nothing is sent. The server writes one prompt, in `by review`'s format (`File:`, `Line:` or `Lines:`, `User comment:`, an `Overall note:` first when there is one) with the quoted lines of the diff under each comment, and delivers it: `next_turn` is a send operation, queued behind a running turn and returned as `operation` (an ID to follow at `/v1/operations/{id}`); `steer` joins the running turn without interrupting it, and is `409 not_running` when no turn runs and `409 steer_refused` when the harness did not take it. A repeat with the same `Idempotency-Key` sends once and is answered before the diff is read, so it is not refused `stale_candidate` when the turn it started made a new candidate: a `next_turn` or `steer` repeat returns the original answer (`Idempotent-Replayed: true`, the same `operation` or `steer`) without sending again; the answer is kept in the server's memory only (not across a restart, like the artifact publish route's), and a `next_turn` repeat after a restart still finds the operation by its key once the diff matches. After delivery the comments are recorded on the branch's log as an `Activity::Feedback` (source `review`, `reference` the candidate commit, each item's `path`, `line`, `start_line`, `side` and `body`, the note as an item with no path), the same record `by review` writes, so [knowledge distillation](knowledge.md) reads both alike.

`GET …/branches/{b}/reviews` reads those records back (`Reviews`): each review's `id` (its event's number in the log), `at_ms`, `commit`, `note` and `comments`. A comment on the branch's side whose review names an older candidate is `addressed_in` the current candidate when a diff between the two changes the lines it names, the idea `by pr --watch` uses for review threads; a comment on removed lines, or from a review that named no commit, is never marked, and is shown as sent. Only the latest 20 reviews are checked (`MAX_ADDRESSED_REVIEWS`, and at most 100 `git diff` runs), and a stored `path` is always a literal path (`git --literal-pathspecs`), never a pattern.

### Idempotency

Send `Idempotency-Key: <1–255 visible ASCII characters>` on any `POST`. The key is scoped to the caller (its principal's subject name, prefixed with its tenant `tenant/name` outside the `default` tenant, so two tenants' principals of one name never share a key) and fingerprinted with the route and the canonical request. A repeat with the same request returns the original operation, `200` if it has finished (with its result) or `202` if not, with `Idempotent-Replayed: true`, and never starts a second run; this holds across restarts and across servers sharing the operation store, since the key is bound in the admission's transaction. Two concurrent requests with one key meet at the store's unique index: the second waits for the first to commit, then replays it. The same key with a different request gets `422 idempotency_key_reused`. `GET /v1/operations?idempotency_key=` finds only the caller's own tenant's operation. Keys are kept for as long as the registry is. `branchyard-client` and `by` send a fresh key per command and retry a `POST` with the same key after a connection failure; a client that lost the response can also look the operation up with `GET /v1/operations?idempotency_key=` (`Client::operation_by_key`).

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

Without `secret`/`secret_file`, a secret is generated into `DATA-DIR/webhook-N.secret` (mode 600), the same way an omitted token is. `events` filters which kinds of activity this target receives (`status`, `stall`, `permission_wait`, `merge`, `failure`); omitted or empty means every kind, and a status change is also tagged `merge` or `failure` when it settles there. `url` must be `https://` unless its host is a loopback literal (`localhost`, `127.0.0.1`, `::1`) or `--webhook-insecure`/`webhook_insecure` is given.

Each target gets every served repository's activity, delivered from the durable feed by its own cursor (`docs/durability.md#webhook-cursors`), so a restart resumes rather than replaying or skipping: at-least-once. A delivery is:

```json
POST https://ops.example/hooks/branchyard
X-Branchyard-Signature: sha256=<hex HMAC-SHA256 of the body, with the target's secret>
X-Branchyard-Delivery: 57
Content-Type: application/json

{"repo": "app", "seq": 57, "branch": "flaky", "at_ms": 1790000000123,
 "kinds": ["stall"], "activity": {"stalled": {"since_ms": 1790000000000}}}
```

`seq` (also sent as `X-Branchyard-Delivery`) is the feed position: use it as the receiver's dedupe key, since at-least-once means the same entry can arrive more than once. `activity` is the same [`RecordedEvent::activity`](../crates/branchyard/src/lib.rs) shape the SSE stream and `by log --json` use. A delivery is retried on a non-2xx response or a connection failure, up to six attempts, waiting 0.5, 1, 2, 4 and 8 seconds between them (doubling, at most 30 s; [`backon`](https://docs.rs/backon)), with a warning for each failed attempt; after repeated failure it is logged as dead-lettered to the server's stderr and the cursor still advances, so one broken target never blocks the others or the feed. From the third failed attempt in a row, every retry counting, the webhook is also listed in [`GET /v1/problems`](#problems) for `admin` principals that may reach its repository, so a dead-lettered delivery always is; the next delivered attempt clears it.

### Problems

`GET /v1/problems` lists what background work could not do that a person can act on: a trigger that paused itself, a queued operation that no live worker can claim, a webhook or a push subscription whose sends keep failing, and an effect whose outcome is still unknown. It lists only things a person can act on, so a healthy server answers an empty list, and each entry ends in the one thing to do. The list is read-only. `GET /healthz` is unchanged, because a paused trigger or a failing webhook must not get a container restarted.

Where an entry comes from decides whether it outlives the process:

- **`trigger` and `operation` entries are derived from the shared store.** Each request reads the trigger store, the queue and the workers table, so these entries survive a restart and read the same on every server that shares a database.
- **`webhook`, `push` and `effect` entries are what this server process saw.** They are held in its memory and stored nowhere. They start empty when the process restarts and come back after the next failures: three more failed attempts or sends, or the next recovery pass for an effect. Two servers on one database each list their own.

```json
{"problems": [
  {"kind": "trigger", "key": "trg_…", "what": "trigger nightly-triage", "since_ms": 1790000000000,
   "failures": 3, "error": "paused after 3 failed runs in a row; the last: branch_exists: …",
   "next": "see why with `by trigger runs nightly-triage`, fix it, then `by trigger enable nightly-triage`"}
]}
```

Entries are sorted by `since_ms`, oldest first. `key` is stable within its kind. `failures` is present only for a kind that counts failures.

| Kind | Listed while | Cleared by | `since_ms` | Seen by |
|---|---|---|---|---|
| `trigger` (`key`: the trigger's ID) | It is disabled with at least its `pause_after_failures` failed runs in a row: it [paused itself](triggers.md#failures-and-pausing). A trigger that a person disabled below that count is not listed. `error` is the pause's reason | `by trigger enable`, which starts the count over, or removing the trigger | When the run that paused it finished: the newest failed run among its latest 20 | Its tenant, when the caller may reach the trigger's repository |
| `operation` (`key`: the operation's ID) | It is queued and unclaimed, requires labels, has waited `unclaimable_after` since admission, and no live worker serving its repository carries all its labels: the test that sets its `waiting` ([worker labels](#worker-labels)). `error` is that reason | A worker that carries the labels beats and claims it | Its admission | Its tenant, when the caller may reach the operation's repository |
| `webhook` (`key`: `<repo>:` and the first 12 hex digits of the SHA-256 of the webhook's id, which is its URL unless the configuration names one) | The last three or more [delivery](#webhooks) attempts to the webhook for that repository failed on this server. Every retry counts, so one dead-lettered delivery is enough. `what` names the URL's scheme, host and port and the repository, never its path or query, which some receivers treat as a credential; `failures` counts the attempts, and `error` is the last attempt's, without the URL | The next attempt that is delivered. Deliveries go on by themselves; entries dead-lettered meanwhile are not resent | The run's first failed attempt | Principals with `admin` that may reach the repository: the target is operator configuration |
| `push` (`key`: the first 12 hex digits of the SHA-256 of the subscription's endpoint) | The last three or more [sends](companion.md#notifications) to the subscription failed on this server. `what` names the device the paired token was redeemed on and its principal, or the principal. The endpoint is a capability and is never shown. `error` is the last send's | The next send that is delivered, or the subscription being dropped or removed | The run's first failed send | The subscription owner's tenant |
| `effect` (`key`: `<repo>/<effect id>`) | This server's latest [reconcile pass](effects.md#reconciliation) left the entry `unknown`. `what` names the entry's short ID, its call (such as `slack: chat_post`) and its branch, and `error` is the pass's reason, such as no lookup declared | A later pass that settles it, through its lookup or the gateway's [audit log](effects.md#the-audit-log-as-a-second-source) | The entry's last change: when it became `unknown` | Whoever may reach the repository |

`admin` never crosses tenants: a credential configured without `scopes` holds every scope, so each tenant's default token is an `admin`. A `trigger`, `operation` or `push` entry is seen only by its own tenant's principals, `admin` or not, and an `effect` entry only by principals that may reach its repository. A `webhook` entry is operator configuration, so it is seen only by principals that hold `admin` and may reach the webhook's repository. `Client::problems` (Rust) and `Client.problems` (Python) read the list, and `by doctor` shows its count and oldest entry as its `server.problems` row ([doctor](doctor.md)).

**Status, 2 November 2026.** Hermetic only: over real HTTP on loopback with the fake ACP agent, on SQLite. `a_trigger_that_paused_itself_is_a_problem_its_tenant_sees_until_it_is_enabled` (`tests/triggers.rs`) and `an_operation_no_live_worker_can_claim_is_a_problem_until_one_claims_it` (`tests/labels.rs`) cover each kind appearing, another tenant not seeing it, another tenant's `admin` principal not seeing it either, and the entry clearing; the second also checks that `/healthz` answers `200 ok` while an entry is listed. `only_a_trigger_that_paused_itself_is_a_problem` covers the trigger rule's boundary. `a_tenants_problem_is_hidden_from_another_tenants_default_scoped_admin_token` (`tests/labels.rs`) gives two tenants default tokens, both holding `admin`, and shows a problem of one listed for it and not for the other. `a_webhook_failing_three_attempts_in_a_row_is_a_problem_for_its_repositorys_admin_until_one_lands` (`tests/webhook.rs`, a local receiver answering 500) shows the three attempts of one dead-lettered delivery listed for an `admin` principal of the repository's tenant, and not for another tenant's default token (which holds `admin`) or for a principal without `admin`, and the next delivered attempt clearing it. `a_subscription_failing_three_sends_in_a_row_is_a_problem_for_its_tenant` (`tests/companion.rs`, a mock push service answering 500) shows no entry after two sends and one after the third, named by its paired device, seen by its tenant but not by another tenant, `admin` or not, the endpoint nowhere in the response, and the entry gone once the subscription is removed. `an_effect_a_recovery_pass_leaves_unknown_is_a_problem_until_it_settles` (`tests/effects.rs`, a mock gateway that never answers the call) shows the entry with the pass's reason after a recovery pass, and gone after the gateway's audit log settles it. Unit tests in `problems.rs` cover the three-failure threshold and each visibility rule with and without `admin`. Not done: a run on PostgreSQL, two servers on one database, a restart that empties the in-memory kinds, a real push service, or a live server.

### Errors

Every error is `{"error": {"code", "message", "detail"?}}`. Codes are stable; messages are for people. SDK errors keep the SDK's own message, so `by --remote` prints what local `by` prints.

| Code | Status | Meaning |
|---|---|---|
| `unauthorized` | 401 | Missing or wrong bearer token |
| `not_found`, `method_not_allowed` | 404, 405 | No such route or method |
| `unknown_repo`, `unknown_branch`, `unknown_operation`, `unknown_trigger` | 404 | No such thing, or another tenant's (indistinguishable from unknown). `unknown_repo` lists the repositories the caller may use (`detail.served`, sorted, beside `detail.repo`; never another tenant's) and ends with how to serve another: `by serve --repo NAME=PATH` |
| `trigger_exists` | 409 | The tenant has a trigger of that name |
| `invalid_pairing_code`, `rate_limited` | 400, 429 | A pairing code that is unknown, used or expired; too many redemption attempts (`Retry-After`) |
| `push_not_enabled` | 403 | A push subscription on a server whose `app.push` is off |
| `precheck_not_allowed` | 403 | A trigger precheck on a repository whose prechecks the operator did not allow |
| `invalid_signature`, `stale_delivery` | 401 | A trigger webhook whose signature does not match its secret, or whose signed timestamp is outside the replay window |
| `scope_required` | 403 | The caller's principal lacks a scope this endpoint needs; `detail.scope` |
| `repo_not_allowed` | 403 | The repository is outside the caller's tenant, or its own narrower allowlist; `detail.repo` |
| `quota_exceeded` | 429 | A tenant quota (`docs/server.md#quotas`) is at its configured limit; `detail.tenant`, `detail.limit`, `detail.max`, and `detail.reserved` or `detail.spent` |
| `invalid_request` | 400 | Malformed JSON, unknown field, empty prompt, bad budget, bad cursor; for `report-status`, a `state`, `summary`, `link` or `ttl_seconds` that breaks a rule, named in `detail.field` |
| `unsupported_media_type` | 415 | `POST` body not declared as JSON |
| `body_too_large` | 413 | Body over `max_body_bytes`, or an artifact upload over `max_artifact_bytes`; `detail.limit` |
| `command_not_allowed` | 403 | A request `command`, or `provision.mcp_servers`, on a server that does not allow client commands |
| `connectors_not_configured` | 403 | `provision.connectors`, a session's `connectors`, or a connect route, on a server without `connectors` |
| `auth_required`, `invalid_credential`, `identity_not_allowed`, `tenant_mismatch`, `no_connector_grant`, `unknown_turn`, `manifest_mismatch`, `turn_not_running`, `turn_mismatch`, `token_unavailable` | 401, 401, 403, 403, 403, 404, 409, 409, 409, 503 | `POST /v1/turns/token`; see [turn tokens for sandboxes](#turn-tokens-for-sandboxes) |
| `connectors_conflict` | 400 | A session start with both `connectors` and `provision.connectors` |
| `connector_not_served` | 422 | A connector the gateway does not serve; `detail.connector` |
| `tool_not_allowed`, `tools_conflict`, `policy_key_invalid`, `policy_changed` | 403, 400, 400, 403 | See [tool policy](#tool-policy) |
| `identity_binding_revoked` | 403 | A queued operation's admitting identity no longer passes its provider's bindings, or, for groups read from Cloud Identity, no longer does with its groups read again; see [fresh groups](identity.md#fresh-groups-on-a-recheck) |
| `identity_unavailable` | 503 | The sign-in dependency (a key set, or the Cloud Identity group lookup of a sign-in or of a recheck) did not answer; the server does not guess |
| `connector_over_ceiling` | 403 | A connector, or a write, beyond the caller's ceiling; `detail.connector`, and `detail.max_mode` when reads are allowed |
| `connector_not_connected` | 409 | A session's connector the caller has not connected, or must reconnect; `detail.connector`, `detail.account`, `detail.action` (`connect`) |
| `gateway_unreachable`, `gateway_refused`, `gateway_unexpected` | 502 | The connector gateway did not answer, refused a connect call (`detail.status`, `detail.code`), or answered something else |
| `skill_unknown` | 422 | A session's `skills` names one that is not built in; the message lists those that are |
| `reviewer_read_only` | 422 | A reviewer session asking for a connector write or helpers |
| `secret_not_allowed` | 403 | A `provision.secrets` name the server's operator did not define; `detail.secret` |
| `provider_not_allowed` | 403 | A provider the server's operator did not allow; `detail.provider` |
| `preset_required` | 400 | A workspace session start without `preset` on a server with `require_workspace_execution_presets` |
| `unknown_preset` | 404 | No [execution preset](#execution-presets) of that ID lists the caller's tenant; `detail.preset` |
| `preset_conflict` | 400 | A workspace session start with `preset` and `provider` or `provision` |
| `preset_choice_unavailable` | 422 | A harness, model or effort the selected preset does not offer; `detail.field` and `detail.allowed` |
| `delegation_not_allowed` | 403 | A delegation envelope, `allow_delegation`, a spawn, or a send to a branch with an envelope, on a server that does not allow delegation |
| `unapproved_tools_not_allowed` | 403 | `unapproved_tools` on a server that does not allow them |
| `idempotency_key_reused` | 422 | Same key, different request; `detail.operation` |
| `branch_busy` | 409 | Another operation holds the branch; `detail.holder` |
| `stale_candidate` | 409 | A review's `commit` is not the branch's candidate any more; read `…/changes` again |
| `unknown_checkpoint`, `checkpoint_unavailable` | 404, 409 | A rewind or fork `at` a checkpoint the branch does not have, or one whose ref no longer names its commit |
| `cursor_out_of_range` | 400 | Stream cursor past the feed's end; `detail.head` |
| `detached_head` | 409 | Merge without a target while the served repository's HEAD is detached |
| `shutting_down` | 503 | The server is stopping |
| `sync_lease_held` | (operation) | With `sync`: another runner holds the lease on one of the operation's branches; the message names it ([sync](sync.md#servers)) |
| `sync_unavailable` | (operation) | With `sync`: the remote could not grant the lease, so the operation did not run |
| `sync_lease_lost` | (operation) | With `sync`: while the operation ran, another runner took over the lease on one of its branches; the run was cancelled and its result not accepted, and the message names the new holder ([sync](sync.md#servers)) |
| `interrupted` | (operation) | The server stopped before the operation finished, or its worker's claim expired after it started |
| `internal`, `git_error`, `io_error`, `state_error`, `harness_error`, `not_a_repository` | 500 | Server-side failure |
| `branch_exists`, `no_candidate`, `target_moved`, `conflict`, `dirty_target`, `already_merged`, `running`, `fenced` | 409 | SDK refusals; `target_moved` has `detail.expected`/`actual`, `conflict` has `detail.files`. `running`: another engine, such as a local `by`, runs a turn on the branch. `fenced`: the engine lost the branch's lease to another |
| `invalid_name`, `unknown_harness` | 400 | SDK refusals |
| `denied` | 403 | The envelope or authority refused a delegation operation, as `by --json` reports `denied` |
| `remote_error` | 502 | An error the engine running a delegating turn returned through its broker; `detail.kind` is its kind |
| `harness_unavailable`, `unsupported`, `config_error`, `check_failed`, `check_timed_out`, `check_not_started`, `invalid_candidate` | 422 | SDK refusals, and a session's `helpers` naming a harness this server does not know (`detail.harness`) (`config_error`: a configuration section that describes nothing usable); `harness_unavailable` has `detail.harness` and `detail.cause` (`not_on_path`, `not_executable`, `no_command` or `no_candidate`), and the refusal of a profile that routes no tool approvals is `unsupported` with `detail` `{"unsupported": "unrouted_approvals", "profile"}`, and the refusal of a profile that cannot keep a restricting policy against its harness's own permissions is `unsupported` with `detail` `{"unsupported": "unbound_policy", "profile", "reason", "private_home_binds"}` ([composing](harness-integration.md#composing-with-a-harnesss-own-permissions)); check failures have `detail.output_tail`, and an integration's `check_failed`, `check_timed_out` and `check_not_started` have `detail.checks`, every check of the branches integrated as `{check, branches, outcome}` (`passed`, `failed`, `timed_out`, `not_started` or `not_run`) |

## Published schema

Workspace requests and responses use `workspace.<Type>` keys in the published
contract, such as `workspace.CreateProjectRequest` and `workspace.Message`.
This keeps conversation messages separate from the existing harness inbox
`Message` type. `OperationView` describes the flat operation response with its
optional `workspace_id` and `session_id`; ordinary clients can still read the
base `Operation` fields.

[`schema/contract.json`](../schema/contract.json) is the JSON Schema for every request and response type above (`crate::api` and `crate::storage_api` in `branchyard-client`), including `GraphRequest` and the `Graph` and `GraphApplied` answers of `…/graph` (their `GraphEdit`, `SpawnSpec`, `Dependency`, `After` and `Binding`, and `BranchStatus`'s `waiting` and `blocked` states), generated from their Rust definitions with [`schemars`](https://docs.rs/schemars) rather than hand-maintained: `#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]` on the wire types (and the branch, delegation and provisioning types they reference), a `schema` feature per crate that pulls it in, and `cargo run -p branchyard-client --features schema --example generate_contract` to print it. `crates/branchyard-client/tests/contract.rs` regenerates it in-process and diffs against the checked-in file, so a changed wire type without a regenerated schema fails `cargo test -p branchyard-client --features schema` in CI. `tests/test_schema_validation.py` validates this document's own worked examples (the ones in this page), and [task graphs](graph.md)' proposal example with a `Graph` holding `waiting` and `blocked` children, against it with a small stdlib-only structural validator, as a sanity check on the schema itself.

[`schema/server.config.json`](../schema/server.config.json) is the JSON Schema for the configuration file above (`config::FileConfig` and its nested `FileToken`, `FileCredential`, `FileTenantPolicy`, `FileWebhook` and `FileTls`, in `branchyard-server`), generated and checked the same way, behind `branchyard-server`'s own `schema` feature: `cargo run -p branchyard-server --features schema --example generate_server_config_schema` regenerates it, `crates/branchyard-server/tests/server_config_schema.rs` (`cargo test -p branchyard-server --features schema`) checks it is fresh, and `tests/test_schema_validation.py` validates [`deploy/config.example.json`](../deploy/config.example.json) against it. Point an editor's JSON Schema support (e.g. VS Code's `json.schemas`, or a `"$schema"` key in the configuration file itself, which is accepted and otherwise ignored by the loader) at it for completion and inline docs while editing a configuration file.

## Remote mode in `by`

```sh
export BRANCHYARD_REMOTE=https://by.example:8421
export BRANCHYARD_TOKEN_FILE=~/.config/branchyard/token
by --repo app run "Make the flaky parser test deterministic" --check "cargo test" --yes
by fan "..." --harness claude-code,codex --yes
by ls; by diff flaky; by merge flaky; by watch; by send flaky --steer "also run the linter"; by cancel flaky
```

`--remote` takes `http://`, `https://`, `unix:/path/to/socket` (a server started with `--listen-unix PATH`, which serves plain HTTP on a mode-0600 socket in a directory only its owner may enter), or `ssh://[user@]host[:port]/path`, which starts such a server on the host and forwards its socket ([remote over ssh](remote-ssh.md)).

Global options go before the command: `--remote URL`, `--token-file FILE`, `--repo NAME` (needed when the server serves several), `--ca-file FILE` (extra trust for `https`). Each has an environment variable: `BRANCHYARD_REMOTE`, `BRANCHYARD_TOKEN_FILE`, `BRANCHYARD_REPO`, `BRANCHYARD_CA_FILE`. Output is the local output: the CLI renders the same SDK values with the same code, and tests compare each command's output, `watch` aside, local against remote, including the `--json` output of `spawn` (with and without `--seat`), `inspect`, `events`, `children`, `graph show`, `graph apply`, `integrate`, `send`, `rig run`, `ask`, `report`, `escalate`, `answer`, `inbox`, `check` and `report-status`.

Every command runs remotely, with the same flags: `--provider` and its options, `--delegate`, `--allow-delegation` and `--allow-unapproved-tools` are sent in the request, and the server refuses what its operator did not allow. `spawn` needs `--parent`, as outside a harness locally, and the child runs on the server. Differences: `--ask` is refused (pass `--yes`, or leave requests denied); without `--yes` requests are denied; worktree paths, `--substrate-key` (which must be absolute) and `--pass-env` values are the server's; `by harnesses` shows local profiles and executable operator-configured commands. With `--json`, a server error prints `{"error": {"kind", "message"}}` with the kind local `by` reports (the server's `git_error` is `git`, and so on), and a server that cannot be reached is `remote_unreachable`. Interrupting `by run` stops watching, not the work; `by cancel` stops the work. `by run --delegate` follows the activity of the children the branch spawns, and prints the `delegated` table once they have finished on the server.

A command following an operation (`run`, `fan`, `send`, `fork`, `reincarnate`, `map`, `spawn`, `rig run` and the plan decisions) keeps following it when the server stops answering: a restart, a deploy, a `kill -9`. It says so once, `by: cannot reach http://127.0.0.1:8421: Connection refused (os error 111); the server keeps operation op_…, and this waits for it to answer again (Ctrl-C stops watching, not the work)`, asks again with the event stream's backoff (250 ms, doubling to 5 s) for as long as it takes, says `by: http://127.0.0.1:8421 answers again`, and ends as it would have: with the branch's card, or with the operation's error when the server could not finish it (a turn running when its server died is `interrupted`, and its branch's log says whether `by send NAME --retry` runs the prompt). A server answering `502`, `503` (`shutting_down`) or `504` is asked again the same way; any other refusal ends the command. Only the operation's own requests wait: a command started while the server is down still fails with `cannot reach`.

`by ls` has no row for a branch a queued operation will create, so it names them under the table: `queued on the server, not started yet: f5, f8 (by show f5 shows its operation)`. Work on a branch carries the branch's own placement: a `send`, `fork`, `reincarnate` or plan decision on a branch with a private home announces `harnesses run as the server's user with a private HOME under .branchyard/homes/`, as the local commands do, whatever its own flags say.

## Herdr

[`plugins/herdr`](../plugins/herdr/README.md) is a [Herdr](https://github.com/herdrdev/herdr) plugin built on this API and `by`. Its bridge opens the event stream, lists the branches, and applies the entries after the stream's starting cursor ([`EventStream::open`](../crates/branchyard-client/src/lib.rs)); it keeps one Herdr tab per branch running `by log --follow <branch>`, and reports each branch's state on it with `herdr pane report-agent --source custom:branchyard` (`running` is `working`, a waiting permission request `blocked`, every settled status `idle` with what happened as the message). When the stream drops it reconnects after the last cursor it applied. Herdr actions run `by merge`, `by cancel` and `by send` for the branch in the focused pane. It uses the same `BRANCHYARD_*` settings as `by --remote`. It is tested against a fake `herdr`, not yet against Herdr itself.

`by log --follow` works in both modes: locally it waits on the store; remotely it reads the branch's events, opens the event stream, and reads the branch's events again from where they ended, once, then prints the branch's entries from the stream, dropping those the second read already showed: they come first on the stream and in the same order, so nothing is missed or repeated, however busy the branch. When the stream drops it reconnects after the last entry it read; while the server is unreachable it says so once, keeps retrying, and says when it is back. `by watch --once` remotely opens the stream from the start, reads the history up to the feed's head as the stream's `open` event gives it, then lists the branches: the frame is complete without waiting.

## Deployment notes

- **One server per data directory** is enforced with an advisory lock on `DATA-DIR/lock`; a second server on the directory fails at start. On a network file system it is only as good as that file system's `flock`. With `--database` there is no lock: the database holds operations, and several servers may share it.
- **TLS.** Terminate TLS in the server (`--tls-cert`/`--tls-key`, rustls with the ring provider, HTTP/1.1) or in a reverse proxy in front of a loopback bind. A proxy must not buffer `text/event-stream` responses. Clients trust the Mozilla roots plus `--ca-file`.
- **Harnesses** run as the server's user, with its `PATH`, `HOME` and harness logins. Install and log in the harnesses as that user, or set `harness_commands`.
- **Limits.** Request bodies are bounded (`max_body_bytes`, default 1 MiB), request heads must arrive within 30 seconds, at most 256 requests are handled at once (more wait), and at most `max_running` operations run at once, plus one follow-up and one console cancel or steer (more queue). There is no per-request deadline beyond those, and no rate limiting per token.
- **Health.** `GET /healthz` needs no token. Every request is given an ID (the caller's `x-request-id`, if it looks safe to reuse, else a fresh one), echoed on the response and attached to a [`tracing`](https://docs.rs/tracing) span (`tower-http`'s `TraceLayer`); with request logging on (the default; off with `--quiet`, which also drops the level to `warn`) each request logs one `INFO` line to stderr with its ID, method, path, status and latency, never headers or bodies.
- **Metrics and traces.** `--metrics` (or `--metrics-addr`) serves Prometheus metrics, and `OTEL_EXPORTER_OTLP_ENDPOINT` exports OpenTelemetry traces and metrics over OTLP (HTTP or gRPC); the standard `OTEL_*` variables configure both, for the server, a worker and any `by` command; see [observability](observability.md).
- **Logging.** `branchyard-server`, `by serve` and `branchyard-herdr` log to stderr with [`tracing`](https://docs.rs/tracing)/[`tracing-subscriber`](https://docs.rs/tracing-subscriber). The level is `BRANCHYARD_LOG`, else `RUST_LOG` (same [`EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/struct.EnvFilter.html) syntax, e.g. `branchyard_server=debug,warn`), else `info` (`warn` with `--quiet`). The format is `--log-format json|pretty` (on `branchyard-server`, `by serve`, `by worker` and `branchyard-herdr`), else `BRANCHYARD_LOG_FORMAT` (`json` or `pretty`), else `pretty`; `json` writes one JSON object per line for a log collector. The flag wins over the variable. A client that closes its connection mid-answer (every `by --remote run` closes the event stream it read once its work is done) and PostgreSQL's notices (`relation "…" already exists, skipping`, one per table at every start) are `debug` lines; a PostgreSQL `WARNING` is a `warn` line. Other `by` commands (`by ls`, `by merge`, ...) also install a subscriber, at `warn` unless `BRANCHYARD_LOG` or `RUST_LOG` says otherwise (not under the live `by watch` screen): it shows the failures Branchyard survived on purpose, such as a cleanup that did not complete, a lock whose holder panicked, or a thread that panicked, each as one line (`by: warning: <step>: <error>`); see [survived failures](observability.md#survived-failures).
- **Backups.** Branch state and event logs are each repository's `.branchyard/state.db`, with worktrees under `.branchyard/worktrees/`; server state is `DATA-DIR/state.db` (operations, idempotency keys, the dispatch queue, branch locks). Both are SQLite databases in write-ahead-log mode: back them up with `sqlite3 FILE ".backup COPY"` or while the server is stopped, not by copying the file alone. With `--database`, both are in PostgreSQL instead: back it up with `pg_dump` of the schema. Either way they grow without bound for now. An `operations.jsonl` from an earlier version is imported on first start and renamed `operations.jsonl.imported`; `DATA-DIR/feeds/` is no longer used.
- **Providers and delegation.** Allow a provider only once its cluster, image or runtime is set up for this server (see [providers](providers.md)); the credentials a turn gets are the server's `pass_env` variables. Delegation runs children on the server's threads and gives harnesses the server's `by`, which must be the same version as the server.

## What is durable

| State | Where | Survives a restart |
|---|---|---|
| Branches, candidates, event logs, the activity feed | Each repository's `.branchyard/state.db`, or the database with `--database`, written by the engine in transactions | Yes |
| Operations (with their tenant and admitting principal), idempotency keys and the dispatch queue | `DATA-DIR/state.db` committed with `synchronous=FULL`, or the database's `by_operations` and `by_operation_queue` tables committed with `synchronous_commit = on`, in one transaction before `202`, and at each state change | Yes; queued ones run after the restart and keep counting toward their tenant's `max_running`, running ones become `interrupted` |
| Cancel requests, `max_duration` deadlines, turn leases, journaled steps, harness process identities | Each repository's `.branchyard/state.db`, or the database | Yes |
| Triggers and their runs | `DATA-DIR/state.db`'s `triggers` and `trigger_runs`, or the database's `by_triggers` and `by_trigger_runs` | Yes; a run recorded but not fired fires after the restart ([triggers](triggers.md#what-is-durable)) |
| Webhook delivery cursors | `DATA-DIR/state.db`'s `webhook_cursors` table, or the database's `by_webhook_cursors` | Yes |
| Each queued operation's priority, tenant and admission time; each tenant's decayed recent claims | The queue's `priority`, `tenant` and `enqueued_ms` columns; `tenant_usage` (`by_tenant_usage`) | Yes ([scheduling](#scheduling)) |
| Metrics counters, unexported spans | The server process | No: counters restart at zero, as Prometheus expects |
| Branch locks | The same store's `branch_locks` (`by_branch_locks`), with the operation's admission and outcome | Yes, with their operations; a removal's expires after 10 minutes |
| A turn in progress | The server process and its harness child | No: it is recovered, not continued |

If the server dies before a worker starts an operation, the operation stays queued and runs once, after the restart or on another server. If it dies mid-turn, the operation becomes `interrupted` when its claim is next taken (at once when a server on the same data directory restarts, otherwise once the lease expires), and opening the repository recovers the branch: the engine kills the harness's process group if its pid and start time still match, ends the branch `interrupted` with a `recovered` event that says whether the prompt had been submitted, and never submits it again. The server, and each `by worker`, also recovers every 30 seconds (`Config::recover_interval`), which covers a local `by run` on a served repository that was killed, and on the same tick starts waiting dependents whose prerequisites settled while no engine could (`Yard::resume_graph`; [task graphs](graph.md#with-the-servers-queue)). [Durability](durability.md) describes the leases, the journal and the recovery rules, and what is not guaranteed.

## PostgreSQL

Build with the `postgres` feature (`cargo install --locked --path crates/branchyard-cli --features postgres`, or `-p branchyard-server --features postgres`), then:

```sh
by serve --database 'postgres://branchyard@db.internal/branchyard' --repo app=/srv/app
```

Each served repository's branch records, event log and feed, leases, journaled steps, harness processes, cancels and steered input are kept in the database under the repository's served name, with [the same semantics as SQLite](durability.md#postgresql); the operation registry is the `by_operations`, `by_operation_queue` and `by_branch_locks` tables. Tables are created when missing, in the connection's `search_path` schema: add `?options=-csearch_path%3Dname` to the URL to choose one. Worktrees, private homes and delegation tokens stay in each repository's `.branchyard/`, and the data directory still holds the default token (a `--worker`, which serves no requests, creates none).

### AlloyDB

AlloyDB speaks the PostgreSQL protocol, so the server uses it as any `--database`. Two ways to connect.

**Through the AlloyDB Auth Proxy (recommended on GKE).** Run the proxy beside the server with `--auto-iam-authn`, and point the URL at its loopback port. The proxy encrypts the connection and logs in with the pod's service account, so the server needs no password, no CA file and no TLS setting:

```json
{ "database": "postgres://my-sa%40my-project.iam@127.0.0.1:5432/branchyard" }
```

**Directly, with TLS and IAM login.** Connect to the instance's private IP with `sslmode=require`, trust the instance's server CA, and let the server fetch a Google access token as the password:

```json
{
  "database": "postgres://my-sa@10.20.0.5/branchyard?sslmode=require",
  "database_ca_file": "/etc/branchyard/alloydb-server-ca.pem",
  "database_auth": "gcp_iam"
}
```

- **`sslmode=require`** connects over TLS (rustls) and checks the server's certificate against the public roots plus `database_ca_file`. `disable` and `prefer` (the default) stay in the clear. Download the instance's server CA certificate as a PEM file from the AlloyDB console, or from the instance's connection info in the AlloyDB API. A file that is unreadable or holds no certificate stops the connection with an error naming it.
- **`database_ca_file`** is a PEM file of one or more certificates, relative to the configuration file. It adds to the public roots and replaces none.
- **`database_auth`** is `"password"` (the default: the URL's password, if any) or `"gcp_iam"`. With `gcp_iam` the server asks the metadata server (`GCE_METADATA_HOST`, else `metadata.google.internal`) for an access token of the service account it runs as, at every new connection, and uses it as the password. It keeps the token until five minutes before it expires. GKE with Workload Identity, Compute Engine and Cloud Run provide the metadata server.
- **The database user** is an IAM database user. For a service account, it is the account's email without `.gserviceaccount.com`, such as `my-sa@my-project.iam`; write the `@` in the URL as `%40`. Create the user in AlloyDB and grant it the `roles/alloydb.client` and `roles/alloydb.databaseUser` roles.
- **Refused at start.** `gcp_iam` with a password in the URL (the token replaces it), and `gcp_iam` without `sslmode=require` unless every host is loopback (the Auth Proxy) or a Unix socket, so a token never crosses the network in the clear. `database_ca_file` and `database_auth` without a `database` are refused too.
- **Workers.** `by worker` and `by serve --worker` read the same configuration file, so they log in the same way.

Status: hermetic only. The token source runs against a mock metadata server, the configuration checks and the CA loading against local files, and no test connects to AlloyDB, Cloud SQL or any TLS-terminating PostgreSQL. Not done: a run against a real AlloyDB instance, with the Auth Proxy or directly; IAM login with a real token; a handshake with a real server CA.

### Several servers on one database

Servers, and `by worker` processes, started with the same `--database` share its operations, queue and branch locks:

```sh
by serve  --database "$DB" --repo app=/srv/app --listen 10.0.0.5:8421 --tls-cert … --tls-key …
by serve  --database "$DB" --repo app=/srv/app --listen 10.0.0.5:8422 --tls-cert … --tls-key …
by worker --database "$DB" --repo app=/srv/app --max-running 16
```

Any of them accepts an operation, any claims it, and each answers for every operation. A claim is a single statement on `by_operation_queue` that picks a row in [scheduling](#scheduling) order with `FOR UPDATE SKIP LOCKED`, takes it and counts it toward its tenant's usage, so two workers never claim one row; leases are measured by the database's clock. An idle dispatcher looks for work every `poll_interval` (500 ms), and at once for what its own server admitted. The queue is plain tables so that it needs no extension; PGMQ could replace `by_operation_queue` without changing the admission transaction's shape.

Limits:

- **Same repositories, same checkouts.** Servers sharing a database must serve the same repositories under the same names, at paths that are the same checkout: the same host, or one shared file system. Worktrees and delegation tokens live in the checkout, and the engine's recovery kills a dead engine's harness only on its own host (elsewhere it waits for the turn's lease to expire).
- **Same configuration.** An operation runs with its worker's `harness_commands`, secrets, allow flags and `tenants`; a worker that would refuse the request fails the operation with the error the request would have got from it. It runs as the principal that admitted it, recorded with the operation, so a `by worker` needs no `tokens` or `credentials`. Quotas are counted in the shared database ([quotas](#quotas)); give every server the same `tenants`.
- **Webhooks** are delivered by every server that configures them, each from the shared cursor: configure them on one.
- **Companion push** is sent once: each server claims the feed entries it read from the shared cursor (a compare-and-set) before sending ([companion](companion.md#notifications)). Servers that all send push need the same `vapid_key`.
- **Throughput.** Each server uses one database connection for its registry; claims poll rather than `LISTEN`.
- **No automatic failover of a running turn.** A turn whose server died is recovered as interrupted, never resumed elsewhere.

### Worker labels

Workers differ: one has a GPU, one runs on Linux with KVM for Microsandbox, one sits next to a large cache. A worker carries **labels**, and an operation may **require** labels; a worker claims only operations whose required labels it all carries:

```sh
by worker --database "$DB" --repo app=/srv/app --label gpu --label linux
by run --remote https://by.internal "train the model" --require-label gpu
```

- **Where labels come from.** `--label` (repeatable) on `by serve`, `by worker` or `branchyard-server`, or `labels` in the configuration file; the flags replace the file's. A configured label is 1 to 63 of `a-z`, `0-9`, `.`, `_`, `-` and `:`, starting with a letter or digit. Workers also advertise `worker-host:<host-and-boot>` from the native process identity and the `harness:<id>` labels their [inventory](#worker-inventory) derives. Workspace runtime and PR jobs require their execution host's automatic label, even when no custom labels are configured.
- **Where requirements come from.** `require_labels` in a task (one branch or a fan), send, fork, reincarnate or spawn request, and `--require-label` on `by run`, `fan`, `send`, `fork` and `reincarnate` with `--remote` (in local mode the flag is refused: there are no workers to choose among). Admission checks them (`400 invalid_request` for a malformed one) and records them on the operation and its queue row. A merge or integration requires none.
- **Claims.** The claim's query takes the first claimable row in [scheduling](#scheduling) order among those whose required labels are a subset of the worker's: on SQLite every element of the row's JSON array must be among the worker's; on PostgreSQL `requires <@ $labels` inside the same `FOR UPDATE SKIP LOCKED` subquery, so two workers racing still never claim one row. Queue rows from before the column existed require nothing.
- **Liveness and why it waits.** Each dispatcher records its worker (id, host, pid, labels, repositories) every 5 seconds in a `workers` table (`by_workers` on PostgreSQL) and deletes its row at shutdown; a worker unseen for 15 seconds is not counted. An operation that requires labels and has waited longer than `unclaimable_after` (default 60 s) is reported, on every read (`GET /v1/operations/{id}`, `?idempotency_key=`, `GET /v1/repos/{repo}/operations`), with `waiting`: `no live worker carries the labels it requires (gpu, linux); live workers serving app: w_… on host [linux]`. Nothing is written: as soon as a worker with the labels beats, the reason goes away, and that worker claims it. A waiting `by run --remote` prints it once on standard error; `by show BRANCH --remote` for a branch that does not exist yet shows the queued operation that will create it, with `requires` and `waiting`.
- **Warm pools.** A repository's `[workspace.pool]` may name labels too: only a `by serve` or `by worker` carrying all of them keeps that pool filled ([warm pools](pools.md)).
- **Not yet.** A rig seat or a fleet entry cannot carry labels of its own: a seat's spawn through the API takes the spawn request's `require_labels`, and a fleet routes through whatever request it makes. Labels route among workers; they are not an authorization boundary (any principal may require any label), and they do not reserve capacity.

### Worker inventory

Each dispatcher (`by serve`, `by worker`, `branchyard-server`) detects the harnesses on its machine when it starts and every five minutes, on a thread of its own, and records the inventory with its beat: the `inventory` column of `workers` (`by_workers` on PostgreSQL), JSON, added by the migrations (a SQLite file inside its `BEGIN IMMEDIATE`, PostgreSQL as one more catalog-checked step). From it the worker derives `harness:<id>` for every harness that can run there (installed, on `PATH`, not logged out, not at a usage limit), adds them to its labels and claims with them. `by serve` and `by worker` also advertise each Claude Code and Codex login's quota, as `by usage` reads it.

A task (one branch or a fan) that runs its harnesses by name (no request `command`, no `harness_commands` entry, the local provider) gets `harness:<id>` added to its `requires` at admission when a live worker serving the repository advertises an inventory in which all of them can run; otherwise nothing is added, so work is never held back because no worker advertises. A trigger's routed task consults the same inventories. `GET /v1/inventory` and `by harnesses --remote URL` show them; `"inventory": false` or `--no-inventory` turns it off. Workers never install anything. See [harness lifecycle](harness-lifecycle.md#workers).

What it is not yet:

- **Shared with local `by`.** A local `by` on a served repository opens its SQLite `state.db` and sees none of the server's branches; use `by --remote`. Nothing is imported from an existing `state.db` when a repository moves to the database.
- **TLS for `sslmode=prefer`.** `sslmode=require` connects over TLS ([AlloyDB](#alloydb)); `disable` and `prefer` (the default) connect in the clear, so keep such a database on a trusted network or a Unix socket.
- **Waits** poll every 100 ms, as SQLite's do across processes; nothing listens for `NOTIFY`. A long poll of an operation another server's worker runs sees it finish within `poll_interval`.

### Service registry

The operation store also keeps the fleet's [service registry](registry.md): `services` in SQLite, `by_services` and `by_service_seq` in PostgreSQL (catalog-checked steps, like the store's other tables). Workers are the workers table's rows, read as `worker` services. Each server registers itself (`server`: its URL, repositories, labels, whether it has connectors) there and in each served repository's own registry, so a `by` on that machine finds it with `by services --kind server`; it deregisters when it stops. Every recovery interval it expires and reclaims records whose lease ran out.

- `GET /.well-known/branchyard` (no token): the server, its API, its JWKS (`/.well-known/jwks.json`, with connectors or a backend with `auth = "turn-token"`), and the kinds, capabilities and health of its live services, without endpoints or owners.
- `GET /v1/services[?kind=K]` (`read`): every record with endpoints, owners and leases. `by --remote URL services`.
- `POST /v1/services` (`admin`): register or renew (`RegisterServiceRequest`); a model gateway registers itself this way. Never with anything to reclaim. `DELETE /v1/services/{id}` and `POST /v1/services/gc` (`admin`).

## Sync

With `sync` in the configuration file, a server or worker replicates its repositories' branches to durable storage ([sync](sync.md)):

```json
"sync": { "remote": "s3://tasks/team?endpoint=https://minio.internal:9000", "encrypt": "passphrase", "passphrase_file": "/etc/branchyard/sync.pass", "interval": "30s", "lease_seconds": 60 }
```

- Before an operation on existing branches it pulls each branch's task (creating or fast-forwarding its refs; a clean worktree follows) and takes the task's lease, renewed while the operation runs and registered in the repository's service registry as `sync_lease`. Another runner's lease fails the operation with `sync_lease_held`; a remote that cannot grant one, with `sync_unavailable`. A lease lost while the operation runs cancels its turns and fails it with `sync_lease_lost`, and the task is not pushed again until this server holds its lease once more.
- A replicator per repository pushes changed refs every `interval` (checkpoints as turns end), from `<data_dir>/sync/<repo>.db`; an operation's branches are queued when it ends. At shutdown the replicators stop and push what is queued once more.
- `/metrics` adds the `branchyard_sync_*` families ([observability](observability.md#metrics)).

The keys are `[sync]`'s ([sync](sync.md#quick-start)) and `lease_seconds` (default 60), in `schema/server.config.json`. A remote is opened once per repository at start; a wrong passphrase or KMS key stops the server from starting.

## Security

- **No isolation by default, even remotely.** Unless a request names an allowed sandbox provider, the server runs harnesses with the local process provider: as the server's operating-system user, with its environment, home and credentials. A remote caller with a token and `--yes` (policy `allow`) can make a harness do anything that user can. Serving a repository is granting its token holders that user's authority. Run the server as a dedicated, unprivileged user on a machine you are willing to hand over. `--allow-provider` does not restrict callers to that provider; they may still ask for `local`. Neither sandbox provider is qualified yet (design §4, M2).
- **Allowed providers use the server's credentials.** A token holder chooses which of the server's variables a sandboxed turn gets through `pass_env`, and which Substrate endpoint, router and key path it uses. For workspace sessions, `require_workspace_execution_presets` takes that choice away: a session names an [execution preset](#execution-presets), whose provider, paths and secrets the operator chose. Direct tasks still accept `provider`.
- **Delegation** gives the server's harnesses the delegation tools, bounded by the envelope. As in local mode, a harness running as the server's user can read other branches' tokens in `.branchyard/`; the envelope stops honest mistakes, not a hostile harness, until harnesses run in sandboxes. `--allow-unapproved-tools` lets token holders run profiles whose tools the request's policy never sees.
- **`allow_client_commands`** also lets any token holder choose the executable the server launches. Leave it off outside tests; configure `harness_commands` instead.
- **The web companion** ([its security model](companion.md#security-model)) is off unless `--app`; its page is static, adds no authority beyond the caller's token, and is served with a strict Content Security Policy.
- **Scopes and tenants bound what a credential can reach (`read`/`run`/`merge`/`admin`, a tenant's repositories, quotas), but not what it does within reach.** A `run`-scoped credential on an allowed repository can still make a harness do anything the server's operating-system user can, exactly as before: scopes are not sandboxing. Configured credentials still have no expiry beyond a hash the operator removes, and no hot rotation or revocation (replacing a hash or `tokens` entry and restarting is how one is rotated or revoked); paired tokens expire and can be revoked immediately ([companion](companion.md)).
- **Plain HTTP** exposes tokens, prompts and code to anyone on the path. The server refuses it off loopback unless told `--insecure-bind`.
- **Information exposure.** Responses include server paths (worktrees, repository roots) and the server's harness availability. `GET /.well-known/branchyard` is public: it names the served repositories and the kinds and capabilities of live services, not their endpoints or owners.
- **Service registrations** need the `admin` scope, are owned by the registering principal, and never carry anything for the server to reclaim.
- **Policy is per request.** A `deny` default is safe; rules match a tool name, or a tool and what it touches (`Bash(git *)`, `Edit(src/**)`; [egress](egress.md#rules-that-name-arguments-and-paths)), as in the SDK.

## Run live on 27 October 2026

One Linux container (four cores, shared with other work), `by` built from `main` at f0f7bff and then with the changes this run led to (below). The server ran in a private `HOME` whose Codex `config.toml` selected a `[model_providers.zai]` (`base_url = "https://api.z.ai/api/v1"`, `wire_api = "responses"`, `env_key = "ZAI_KEY"`), with `GEMINI_API_KEY` and `GEMINI_CLI_TRUST_WORKSPACE=true` in its environment; the client ran from a directory of its own with no harness keys, passing the token file `by serve` had made. Gemini CLI 0.65.0 on `gemini-2.5-flash-lite`, codex-cli 0.164.0 on GLM-4.6 through Z.ai, PostgreSQL 16.15 in a private cluster on a Unix socket; 29 turns started, of which 24 reached a model (five were recovered before their prompt was sent).

- **Start.** `cd app && by serve` printed `listening on http://127.0.0.1:8421` and logged where its token was. `by --remote http://127.0.0.1:8421 --token-file token harnesses` listed the server's harnesses under `worker w_… on vm/… (this server), serving app`; Claude Code read `logged in (oauth_token)` from the container's own credentials, which no run used.
- **One branch at a time.** `by run "Create a file named hello.txt …" --harness gemini-cli --model gemini-2.5-flash-lite --yes` made a branch that failed at once, `could not provision gemini-cli-acp: Gemini CLI's settings.json for gemini-cli is written into its home, which is yours in local mode; run it --isolated or in a sandbox provider` (the home is the server's; a local run says the same). With `--isolated` it ended `ready` in 10 seconds, and on Codex (no `--model`: the server's `config.toml` chose GLM-4.6) in 28. `show`, `diff`, `log`, `send` (9 and 13 seconds), `merge` (`merged hello-codex into main (6f69004e70..70cd220714)`) and `rm` printed what they print locally. `by cancel slow` during a `sleep 90` answered `asked slow to stop` in 0.05 seconds; the run ended `turn 1 interrupted` half a second later, the `sleep` gone.
- **Ten at once on SQLite, then SIGTERM.** Ten `by --remote run`s, each a one-line prompt to create a file (five on Gemini CLI with `--isolated`, five on Codex), started together: eight ran (`--max-running` 8) and two queued, which `by ls` did not show. A SIGTERM to the server 13 seconds in: the eight finished within the grace period (13 to 19 seconds each; the process exited 7.6 seconds after the signal), the two queued ran after the restart (8 and 10 seconds, starting 2.5 seconds after it listened, once its harness inventory was detected), and all ten ended `ready` with their file, 39 seconds from the first event to the last. Every client, though, ended at the signal with `by: cannot reach http://127.0.0.1:8421: Connection reset by peer (os error 104)` and `next: start a server with by serve on that host, or fix the URL, token or CA file`, and the server's log said nothing of what it did with the ten.
- **Ten at once on PostgreSQL, then `kill -9`.** The same ten against `by serve --database 'postgres://postgres@%2F…%2Frun:5544/branchyard'` and a `by worker` on the same database, with the changes below: all ten ran at once, eight on the server and two on the worker. `kill -9` of the server 10 seconds in, and a restart a second later: each client said `by: cannot reach http://127.0.0.1:8431: the connection closed before a message arrived; the server keeps operation op_…, and this waits for it to answer again (Ctrl-C stops watching, not the work)`, then `by: http://127.0.0.1:8431 answers again`. The worker's two ended `ready` (9 and 13 seconds) and their clients printed the card. The server's eight were recovered as the restarted server opened the repository, 1.3 to 2.0 seconds after the kill, each `interrupted`: `recovered: its engine (pid 25647) is no longer running before the prompt was submitted; the turn never ran: by send pgfan-f1 --retry runs its prompt` on Gemini CLI, and on Codex `… the prompt had been submitted and the turn's outcome is unknown. It was not submitted again`, with the file already in the candidate; their clients ended with `the worker running this operation stopped before it finished; a turn it left running is recovered as interrupted, and nothing is run again`. No harness process was left. `by send pgfan-f1 --retry --yes` ended `ready` in 6 seconds, and `by merge pgfan-f6` merged an interrupted branch's candidate.
- **SIGTERM again, with the changes.** `--max-running 1`, a Gemini CLI turn running `sleep 20` and a second run queued: `by ls` ended `queued on the server, not started yet: next2 (by show next2 shows its operation)`. At the signal the server logged `shutting down: 1 running operation may finish within 60s (signal again to stop waiting); 1 queued stay queued for the next start`, and 23 seconds later `stopped: 1 finished within the grace period; 1 left in the queue for the next start, or another server on the database`. Both clients waited through the restart and printed their cards (`status ready`); the first showed the rest of its turn, recorded during the grace period (`▸ echo "done" > slow2.txt`), from the event stream once the server was back. A `send` to the isolated branch then announced `harnesses run as the server's user with a private HOME under .branchyard/homes/`.

What the run changed, each with a test: a `by --remote` command following an operation waits through a restart of its server instead of ending with `cannot reach` (`a_run_follows_its_operation_across_a_server_restart`, and the unit tests in `remote.rs`); the server's log says at a shutdown what runs, what stays queued and what became of them, and its token line read `wrote a secret file created a token in …` (`the_log_says_where_the_token_is_and_what_a_shutdown_did`); a client closing its connection, which logged `WARN best-effort step failed: serve a connection: connection closed before message completed` each time a `by --remote run` finished, and PostgreSQL's notices, of which every start after the first logged 31 (`INFO NOTICE: relation "by_meta" already exists, skipping`, …), are debug lines (`a_restarted_server_logs_no_notices_from_postgres`, `notice_tests` in `pg.rs`); a cancel through the default token, logged `cancelled by default through the server`, reads `cancelled by the default token through the server` (`by_cancel_stops_a_turn_on_the_server`); a `send`, `fork` or plan decision on an isolated branch, which announced `in the server's own HOME; no sandbox`, names its private home (`a_send_to_an_isolated_branch_says_it_keeps_its_private_home`); `by ls` names the branches queued work will create (`work_requiring_a_label_no_worker_carries_says_why_in_by_show`).

Not done: Claude Code was not run through the server (Codex was the second harness, so the container's own Claude login stayed out); no server on another host, no TLS, no `ssh://` remote, no load beyond ten turns, no live `by watch` screen (only `--once`); a refused `--model` on Gemini CLI without `--isolated` still leaves a failed branch whose card offers only `by log` and `by rm`, and an operation a dead server ran still ends its client with no `next:` line; `by wait`, `merge` and `integrate` against a server do not wait through a restart; the permission lines still print raw JSON (`asks acp:edit: {"toolCallId":…`).

## Live validation

Live on 28 October 2026: a dedicated `by` 0.0.1 server on GCE, using loopback HTTP on port 8789, ran Antigravity `agy` 1.4.8 through write, check, diff and merge. The client used the server's generated token file; the server supplied `GEMINI_API_KEY` through its secret registry and fixed the harness executable with `--harness-command`. The server shut down after the run. Record: `logs/agy-backends/exec-http` in the [remote validation record](remote-validation-2026-10-28.md). Not done: TLS, multiple tenants, distributed workers, or sandbox providers through this HTTP server in this live run.

## Code

| Crate | Contents |
|---|---|
| [`branchyard-server`](../crates/branchyard-server/src/lib.rs) | Configuration, authentication, the operation registry and its SQLite and PostgreSQL stores, the activity feed, routes, TLS and shutdown; [`storage_routes.rs`](../crates/branchyard-server/src/storage_routes.rs) for artifacts and scratch areas. [`metrics.rs`](../crates/branchyard-server/src/metrics.rs), [`telemetry.rs`](../crates/branchyard-server/src/telemetry.rs) and [`observe.rs`](../crates/branchyard-server/src/observe.rs) for [observability](observability.md). Tests: [`api.rs`](../crates/branchyard-server/tests/api.rs), [`observability.rs`](../crates/branchyard-server/tests/observability.rs), [`parity.rs`](../crates/branchyard-server/tests/parity.rs) (opt-ins and delegation endpoints), [`postgres.rs`](../crates/branchyard-server/tests/postgres.rs), [`storage.rs`](../crates/branchyard-server/tests/storage.rs) Triggers and schedules: cron, interval and signed webhooks from GitHub, Slack, Linear or any JSON sender, each event or due time fired once through admission ([triggers](triggers.md)). |
| [`branchyard-client`](../crates/branchyard-client/src/lib.rs) | Wire types, a blocking HTTP/1.1 client over rustls, an SSE parser, and a reconnecting event stream |
| [`branchyard-cli`](../crates/branchyard-cli/src/main.rs) | `by serve`, remote mode and `by watch` |
| [`branchyard-herdr`](../crates/branchyard-herdr/src/main.rs) | The Herdr plugin's binary: the bridge, the branch pane, and the merge, cancel and send actions. Tests: [`bridge.rs`](../crates/branchyard-herdr/tests/bridge.rs) (against `by serve` and a fake `herdr`), [`manifest.rs`](../crates/branchyard-herdr/tests/manifest.rs) |

### Console metadata transactions

Console project, workspace, session, source and collaboration records live in the
operation store's SQL database. They contain allocation intent and native task or
attempt bindings. Conversation text and branch state remain in native records.
SQLite and PostgreSQL use the same revision checks and immutable tenant ownership.
There is no local metadata fallback when PostgreSQL is configured.

`Registry::submit_console` admits a metadata batch and an operation through one
transaction, including the existing idempotency binding, quota check, branch
locks and queue row. A duplicate operation key returns its original operation
without applying the retry's metadata. A failed metadata revision check rolls
back the entire admission. `Registry::console_mutate` applies synchronous
metadata batches with the same compare-and-set rules. Creation requires absence
and revision one; replacement requires the current tenant and revision and
increments that revision by one. A conflicting edit returns `revision_conflict`.

Workspace quota reservations can use a tenant-owned counter record in the same
batch as allocation intent and queue admission. Concurrent reservations must
compare and increment that counter, so requests for different projects cannot
both consume the last workspace allowance.

The worker rechecks workspace access with the operation's recorded principal
immediately before execution. Removing membership after admission therefore
prevents queued work from running. Operator credentials do not implicitly bypass
workspace membership. Infrastructure credentials, upstream connector grants and
workspace roles remain separate permission checks.

### Console membership on legacy resource URLs

Workspace membership is additional to authenticated scope, tenant policy and
repository grants. An `admin` scope does not bypass membership. Owners and editors
can run sessions; reviewers and viewers can read them. Reviewers can comment when
their credential also permits the mutation scope. Repository-restricted callers
cannot allocate managed knowledge sources outside their explicit grants.

A bound branch and its descendants retain workspace access checks through the
older branch, artifact, operation and task-record URLs. Incomplete bindings fail
closed. Managed private yards are accessible through workspace routes only; an
opaque managed repository name does not grant legacy repository access. Aggregate
legacy repository routes currently require membership in every workspace bound
to that repository, including workspaces owned by another tenant. This conservative
rule avoids exposing mixed lists or event feeds until per-record filtering is
available. Use the workspace routes for individually authorized access.

Long-lived legacy event streams recheck membership every second and close after
revocation. Operation reads recheck access after a long poll completes. Queue
execution must also authorize the admitted principal against current membership.
These checks never grant upstream connector, IAM or workload identity privileges.

Authenticated paired delegation preserves the issuing subject and verified origin
separately from the acting device credential name. The server's delegated-link
helper refuses a different tenant, broader scopes or broader repository grants.
Operator-issued CLI links without an authenticated issuing identity create explicit
standalone principals; their generated names do not assert a shared human identity.

Console queue records retain the admitted credential's SHA-256 reference and
expiry beside the stable subject identity. They contain no bearer token. Device
credentials keep the issuing subject for idempotency and session ownership;
credential references identify the exact device grant independently.

Admission also creates a resource binding keyed by the generated operation ID.
Initial session metadata stores that `operation_id`; workspace allocation intent
stores its initialization operation internally. Views can therefore distinguish
queued, failed or interrupted allocation from an existing native conversation,
without copying execution state into console metadata.

### Durable workspace console

Projects choose `source: {"kind":"knowledge"}` or
`source: {"kind":"repository","repo":"registered-name"}` and a
`result_mode` of `code`, `artifacts`, or `mixed`. Knowledge projects do not need
GitHub, a registered repository, or a Git diff. A repository source must already
be registered and granted to the caller. Neither source accepts a host path.

The operator enables repository-free work in the server configuration:

```json
{
  "managed_knowledge": {
    "enabled": true,
    "root": "/var/lib/branchyard/knowledge",
    "max_workspaces": 100,
    "max_source_bytes": 67108864,
    "max_upload_bytes": 8388608
  }
}
```

It is disabled by default. An omitted root uses `data_dir/knowledge`. The root
must be absolute and owned by the operator. A knowledge-only server needs no
`repos` entry and needs an explicit data directory. Workers that claim managed
work must enable this storage and mount the same root. Their queue routing name
is `__console_managed__`; a repository-restricted principal or tenant needs an
explicit grant for that name. Membership does not grant storage, execution scopes,
connector permissions, or upstream credentials. Configured repositories remain
immutable grants. Configured PostgreSQL stays authoritative for console and
native execution state; an unavailable backend is an error.

Every mutation takes `Idempotency-Key`. Workspace edits take the workspace's
current `revision` and return `revision_conflict` on concurrent changes. Reload
workspace detail before another revision-dependent edit. Stable workspace and
session IDs are independent of titles, device names, branch execution state, and
native conversation identifiers. A workspace points at one native task; a named
session points at one isolated attempt of that task. Session starts return the
existing Operation envelope with additional `workspace_id` and `session_id`
fields. Poll the existing operation route to determine its outcome.

| Route | Behavior |
|---|---|
| `GET /v1/projects` | List projects permitted by tenant and source policy |
| `POST /v1/projects` | Create `{name,source,result_mode}` |
| `GET /v1/projects/{project}` | Read one project |
| `GET /v1/workspaces?project=ID` | List accessible workspaces |
| `POST /v1/workspaces` | Reserve `{project_id,title}` and enqueue native task initialization |
| `GET /v1/workspaces/{workspace}` | Read revision, task binding, sessions, source provenance, members, accepted outputs, and capabilities |
| `GET /v1/workspaces/{workspace}/sessions` | Derive named session views from native attempts and operations |
| `GET /v1/execution-presets?workspace=ID` | List the [execution presets](#execution-presets) offered to the caller's tenant, with readiness and, with a workspace, `can_start` |
| `GET /v1/workspaces/{workspace}/tools` | The connections, built-in skills and helpers a session here may choose ([connecting from the console](#connecting-from-the-console)) |
| `GET /v1/connections`, `POST /v1/connections/{connector}/connect`, `POST /v1/connections/{connector}/key` | The caller's connections, and connecting one ([connecting from the console](#connecting-from-the-console)) |
| `POST /v1/workspaces/{workspace}/sessions` | Start `{name,prompt,harness,preset?,model?,effort?,result_contract?,access,sources?,artifacts?}` |
| `GET /v1/workspaces/{workspace}/sessions/{session}/messages?after=CURSOR` | Read native prompt, output, and steering text with a cursor |
| `POST /v1/workspaces/{workspace}/sessions/{session}/messages` | Continue `{prompt,action:"send"|"resume"|"steer"}` in the same native attempt |
| `POST /v1/workspaces/{workspace}/sessions/{session}/cancel` | Admit attributed native cancellation |
| `POST /v1/workspaces/{workspace}/sessions/{session}/readmit` | Run the tool admission check again from the session's stored tools and record this server's ceiling and the current policy version, or `policy_changed`; see [tool policy](#tool-policy) |
| `GET /v1/workspaces/{workspace}/sessions/{session}/runtime` | Inspect the owning provider, retained allocation, capabilities and terminal readiness |
| `POST /v1/workspaces/{workspace}/sessions/{session}/runtime/actions/{action}` | Admit a supported resource lifecycle action into the durable queue |
| `POST /v1/workspaces/{workspace}/sessions/{session}/terminals` | Open an explicitly permitted idle writer terminal with a real provider PTY |
| `GET /v1/workspaces/{workspace}/sessions/{session}/terminals/{terminal}/output` | Read bounded terminal replay after a sequence cursor |
| `POST /v1/workspaces/{workspace}/sessions/{session}/terminals/{terminal}/input` | Send sequenced input without replaying duplicate writes |
| `POST /v1/workspaces/{workspace}/sessions/{session}/terminals/{terminal}/resize` | Resize the existing provider PTY |
| `POST /v1/workspaces/{workspace}/sessions/{session}/terminals/{terminal}/close` | Close the handle and terminate its shell process group |
| `POST /v1/workspaces/{workspace}/actions/{action}` | Archive or restore at `{revision}`; unsupported provider actions return structured capability reasons |
| `GET /v1/workspaces/{workspace}/sources` | List immutable original source metadata |
| `POST /v1/workspaces/{workspace}/sources` | Upload original bytes with `X-Source-Title-Encoded` or `X-Source-Title`, `Content-Type`, and `X-Workspace-Revision` |
| `GET /v1/workspaces/{workspace}/sources/{source}/content` | Download digest-verified original bytes as an attachment named by the title |
| `GET /v1/workspaces/{workspace}/sources/{source}/preview` | Render a bounded original source after checking current membership |
| `GET /v1/workspaces/{workspace}/sources/{source}/preview/assets/{asset}` | Read a derived page image under the same source grant |
| `GET /v1/workspaces/{workspace}/artifacts` | List native immutable artifacts owned by this workspace |
| `GET /v1/workspaces/{workspace}/artifacts/{artifact}/content` | Download a permitted digest-verified native artifact |
| `GET /v1/workspaces/{workspace}/artifacts/{artifact}/preview` | Render a bounded artifact owned by this workspace |
| `GET /v1/workspaces/{workspace}/artifacts/{artifact}/preview/assets/{asset}` | Read a derived PNG after rechecking the artifact publisher and membership |
| `POST /v1/workspaces/{workspace}/artifacts/{artifact}/frame` | Mint the short-lived path `/app/frame/{ticket}` that serves an HTML output for the console's sandboxed iframe, see [document previews](artifact-previews.md#run-an-html-output) |
| `POST /v1/workspaces/{workspace}/finalize` | Accept `{revision,artifacts:[IDs]}` independently of code integration |
| `GET /v1/workspaces/{workspace}/members` | List roles |
| `GET /v1/workspaces/{workspace}/members/{subject}` | Read one verified subject's membership |
| `POST /v1/workspaces/{workspace}/members/{subject}` | Owner with `admin` scope grants `{revision,role}` within the recipient's existing policy |
| `DELETE /v1/workspaces/{workspace}/members/{subject}` | Owner removes membership at `{revision}`; the final owner cannot be removed |
| `GET /v1/workspaces/{workspace}/comments` | List attributed review comments |
| `POST /v1/workspaces/{workspace}/comments` | Add `{revision,text}` attributed to verified subject and acting credential |
| `GET /v1/workspaces/{workspace}/presence` | Read unexpired advisory presence |
| `POST /v1/workspaces/{workspace}/presence` | Refresh only the caller's presence with `{ttl_seconds}` between 1 and 300 |

Uploads are bounded originals stored durably in the same SQL backend with digest,
media type, title, uploader, and workspace provenance. A session's `sources` list
selects explicit source IDs. Before its first turn, the engine copies those
originals into its isolated worktree through the existing workspace-copy feature;
source copies are excluded from candidate snapshots. Originals remain unchanged.
A reviewer also selects frozen artifact IDs and runs under the
intersection of the original policy and the engine's read-only policy. Separate
sessions retain separate native conversations; continuation never substitutes a
fresh native session.

A source's title travels in one of two headers. `X-Source-Title` holds visible
ASCII, 1 to 512 bytes. `X-Source-Title-Encoded` holds any title as an
[RFC 8187](https://www.rfc-editor.org/rfc/rfc8187) extended value: `UTF-8`, an
optional language tag of letters, digits and `-` that is ignored, and the
percent-encoded UTF-8 title, as in
`UTF-8''%C3%89tude%20de%20march%C3%A9.xlsx`. When the encoded header is present,
the server reads only it and ignores `X-Source-Title`. An invalid encoded title is
refused and never replaced by the ASCII one, so a client sends an ASCII copy in
`X-Source-Title` for older servers. The encoded value is at most 4 KiB, and after
the charset and language it holds only `attr-char` and `%` followed by two hex
digits. It must decode to valid UTF-8 of 1 to 512 bytes, with no control
character (Unicode category Cc, C1 included) and no bidirectional override or
isolate (U+202A to U+202E, U+2066 to U+2069). Any other value is
`400 invalid_source_title`, and the message names the rule it broke. The title is
stored as decoded, without normalisation, and the upload's idempotency
fingerprint covers the decoded title. A source or artifact download sends
`Content-Disposition: attachment; filename="<fallback>"; filename*=UTF-8''<encoded>`
([RFC 6266](https://www.rfc-editor.org/rfc/rfc6266)). Both parameters name the
title's or artifact name's last segment after any `/` or `\`; a name that ends
in one of them is sent as a bare `attachment`. The quoted fallback has `_` in
place of each non-ASCII character, control, `"` and `\`.

Finalization requires immutable artifacts from completed workspace producers with
a valid deliverable contract and valid current-turn output. An earlier turn's
artifacts cannot satisfy a later turn. Acceptance records the checked workspace
revision, producer, turn, result event, digest, verified accepting subject, and
acting credential. It changes no Git integration target. Mixed work can accept
artifacts and integrate code independently.

Task identity and initialization intent are reserved in the same SQL transaction
as the durable queue entry. Initialization resumes the exact reserved location
under the core task reservation API. A crashed model turn remains interrupted;
no recovery resubmits it automatically. `restore` can explicitly reconcile an
interrupted allocation. Archive refuses active or queued work and retains
sources, native records, and accepted artifacts. Provider sleep/wake, terminal,
proxy, preview rendering, and pull-request services attach to the task/provider
binding; absent capabilities produce errors with reasons. No separate retention
or cleanup controller is introduced.

**Status, 29 October 2026.** Hermetic checks passed for native reservation and
pre-submit cancellation, server and client unit tests that need no sockets, and
the knowledge workflow through source copies, native fake-agent turns, immutable
artifact acceptance, collaboration, and revoked access. Socket-free workflow tests
seed inert native port reservations; the fake agent never listens on them.
PostgreSQL and HTTP integration tests compile, and targeted clippy, rustdoc, and
static checks pass. Actual commands and run logs are in `.console-checks/` in the
foundation worktree. Not done: socket-bound test execution, PostgreSQL execution,
live harness/model runs, cloud services, provider lifecycle/terminal/preview/PR
qualification, generated facts/schema integration, or full workspace gates.

Console operations retain the ordinary operation envelope and add `workspace_id`
and, for session commands, `session_id`. These bindings are available on the
initial response, `GET /v1/operations/{id}` and idempotency-key recovery. The
explicit operation ownership record is committed with queue admission, so lookup
does not depend on a branch having been created or retained.

Control commands use the same durable SQL queue and claim fences. Each worker
can dispatch one short cancel/steer operation beyond its normal model-turn slots,
so a full turn pool cannot prevent cancellation. A configured tenant allows one
extra control admission above its normal turn ceiling; controls reserve no
branch. Ordinary admission conservatively counts a short control until it
finishes. There is no second queue.

Resource reads never initialize a task. An allocation that failed or was
interrupted is inspectable through its original operation; a revision-checked
`restore` queues initialization at the same reserved identity and location.
Lifecycle completion verifies the operation that owns the current allocation
intent before committing metadata. Registered repository sessions retain the
existing sync leases and lease-loss cancellation behavior. A server configured
with sync currently refuses a managed-source turn with `sync_source_unsupported`
until its managed source has a supported replication mapping.

Legacy artifact reads check both the reader branch and the immutable artifact's
publisher. A native artifact sharing grant does not grant membership in the
publisher's workspace. A legacy artifact collection is denied if any listed
publisher is inaccessible; workspace artifact collections filter publishers to
their owning workspace. Console session metadata or native task ownership cannot
fall back to an unbound legacy grant when its atomic resource binding is missing.

A native session retains the upstream identity with which it was created.
Continuation and steering require that same verified subject, including a paired
device delegated from that subject. Other workspace editors can read, comment,
cancel permitted work, or start their own isolated session. Membership therefore
does not silently delegate another person's connector account. Worker identity
and upstream connector consent remain independent of workspace membership.

Cancellation admitted before the first turn records durable cancellation intent
on the named session. Its queued start checks that intent before submission; the
native submit boundary also honors cancellation arriving during preparation.
No uncertain model turn is retried. Conversation mutations advance the workspace
revision, and acceptance refuses a producer with pending work, verifies original
artifact bytes, and commits only at the checked revision.

Original byte records are separate from source metadata within the same SQL
store. Upload commits both records, the byte quota reservation, and idempotency
receipt together. Listing a workspace therefore reads source metadata without
loading every original file; downloads check the original's tenant, workspace,
and digest binding before returning bytes.

Session starts also accept the existing `policy` and `provision` structures.
The default permission policy remains deny. Callers explicitly select permitted
tool rules or a preset; the console never substitutes an allow-all policy.
Provisioning uses operator-defined secret names, login choices, and connector
grants through the same validation as task creation. Client-supplied commands or
MCP executables do not gain additional permission through the console. Top-level
model/effort choices cannot conflict with provisioning choices, and source-copy
instructions are added without replacing the selected native authentication.

Console session creation accepts optional `policy` and `provision` objects using
the existing task request contracts. Omitted policy remains deny by default.
An explicit policy can permit the selected source reads and tools needed for a
session; reviewer constraints still intersect it with read-only access.
Provisioning selects operator-defined secret names, login references and supported
model settings. A request cannot select a secret's environment variable or host
file source, and the existing operator controls still reject unapproved MCP
executables and connector grants. No broader tools or secrets are granted merely
because a request comes through the console.

Workspace lifecycle locks use a reserved `workspace:<id>` resource key in the
existing SQL lock table. The colon cannot occur in a native Git branch name.
Allocation operations have no invented branch; their explicit operation binding
provides `workspace_id`. Lifecycle readiness consults those bindings as well as
real session branches when checking for queued work.

Reviewer session input can be immutable workspace artifact IDs or an explicit
`candidate` commit from the workspace's native task. The server validates the
exact candidate against native attempt records and fixes the review checkout to
that commit. A later writer turn cannot change the review input. Mutable branch
names, arbitrary Git references and candidates from another task are rejected.
The reviewer still runs with the intersection of its requested policy and
read-only constraints; selecting a candidate grants no integration permission.

### Execution presets

An execution preset is a named environment the operator writes into the configuration. A workspace session start names its ID; the server supplies the provider, its paths and secrets, and the worker labels. A preset grants nothing. It resolves to the start's existing `provider`, provisioned secrets and `require_labels`, which then pass the same checks as any start: `allow_providers`, `allow_unapproved_tools`, provisioning, membership and roles, quotas and scheduling. A direct task (`POST /v1/repos/{repo}/tasks`, `by run --remote --env ID`) may name one too: see [direct tasks on a preset](#direct-tasks-on-a-preset). Native delegation is unchanged.

Native `container` and `agent_sandbox` targets require an operator preset even
when their kinds are allowed. A direct provider request is refused with
`403 preset_required`. Admission and execution compare the provider with the
private resolved preset; continuations compare the branch's recorded provider.
Raw send, fork, reincarnate, spawn and graph mutation routes cannot inherit a
native target. The worker's provider admission also applies before graph
recovery or an automatic wake can claim a native turn. Default recovery options
cannot stand in for a resolved preset.
See [cloud execution](cloud-execution.md) for GCE and GKE provider examples.

#### Direct tasks on a preset

A task request may carry `preset`, an ID from `GET /v1/execution-presets` (`by run --remote --env ID`, `by fan --remote --env ID`):

- The server resolves it for the caller's tenant at admission, and again for the admitting principal when a worker runs the operation, so a preset removed or narrowed while the task waits fails it rather than running it elsewhere. The queued request records the ID; it never supplies a stored resolution as execution authority.
- The preset supplies the provider, its secrets (in a private home, as `isolated`), its worker labels (added to the request's `require_labels`, and so to the queue row) and, when one harness runs, its default model.
- A request with `preset` may not carry `provider`, `provision` or `command` (`400 preset_conflict`). It runs only harnesses the preset offers (`422 preset_choice_unavailable`), its first when it names none. An ID the tenant is not offered is `404 unknown_preset`.
- `allow_providers`, quotas, scheduling and the other admission checks apply as to any task.

Direct preset tasks use the same configured-value projection as preset sessions. It covers option-building and planning errors, execution errors, and the operation's serialized error and result, including failed branches and descendants inside a successful result. HTTP status and public error codes stay intact. The private operation record keeps the projection captured at admission and adds the executing worker's resolved values before execution, so queued responses, operation reads, lists and idempotent retries remain protected after configuration changes or removal. The snapshot grants no permission and contains no secret values.

The worker also records each created branch's native task ID. Existing branch, event, inspection, task-record, wait and feed projections use that association, including when execution fails without an operation result. Reusing a branch name or worktree path does not attach the old task's projection to a new task. Worker authorization and preparation refusals before execution, including authorization revoked while queued, record an empty feed interval ending at the admission cursor; they cannot claim later activity from unrelated tasks. If an interrupted operation has no captured task identity, the server withholds a branch response it cannot safely associate. Existing tenant, repository and workspace authorization still runs before a caller can read an operation or branch.

Not yet: a send, fork, reincarnation, spawn or plan decision on such a branch with a native provider is refused as before (`by fork`/`by reincarnate --env` are refused by the CLI); a map does not take a preset. The exclusions listed under [execution preset projection](#execution-presets) also apply to direct tasks.


```json
"require_workspace_execution_presets": true,
"execution_presets": [
  { "id": "local", "label": "This server",
    "description": "Runs on the Branchyard server with the harnesses installed here.",
    "provider": { "kind": "local" },
    "harnesses": [
      { "harness": "antigravity", "models": ["gemini-3.8-flash-low"] },
      { "harness": "claude-code" }
    ],
    "secrets": ["GEMINI_API_KEY"] },
  { "id": "substrate-gke", "label": "Substrate on GKE",
    "description": "Runs each session in its own Substrate actor on the team's GKE cluster.",
    "provider": { "kind": "substrate", "endpoint": "https://substrate-control.internal.example",
      "router": "https://substrate-router.internal.example", "template": "branchyard-harnesses",
      "key": "substrate/bridge.key", "api_token_file": "substrate/api.token", "ca": "substrate/ca.pem" },
    "harnesses": [ { "harness": "antigravity", "models": ["gemini-3.8-flash-low"] } ],
    "secrets": ["GEMINI_API_KEY"],
    "tenants": ["research"],
    "require_labels": ["substrate-gke"] }
]
```

| Field | Meaning |
|---|---|
| `id` | Stable and unique: 1 to 64 of `a-z`, `0-9` and `-` |
| `label`, `description` | Plain language for the console |
| `provider` | A request's `provider`: `local`, `substrate` or `microsandbox`. Its kind must be in `allow_providers` (`local` always is); `recipe` is refused. Relative paths resolve against the configuration file's directory. A `microsandbox` preset on a build without Microsandbox gets the `--check` warning that `allow_providers` gets |
| `harnesses` | Harness or profile IDs a session may choose, matched exactly: `claude-code` does not admit `claude-code-acp`. Each has an optional `models` allowlist and `default_model`, by default the first of `models`. Without `models`, a session may not choose a model |
| `secrets` | Names in `secrets`, added to the session's provisioning. A worker resolves them from its own table, so workers need the same names |
| `tenants` | Tenants that may use the preset. Absent: every tenant |
| `require_labels` | Worker labels every turn of the session requires, so a Substrate session runs only on workers that hold its files |

`by serve --check` validates presets after the flags merge `allow_providers` and `secrets`. With no `execution_presets` at all, the server offers one implicit `local` preset (`"origin": "default"`) whose harnesses are the ones it finds, with no secrets. When presets are configured and none lists the caller's tenant, the list is empty.

`GET /v1/execution-presets?workspace=ID` needs `read`. With `workspace`, the caller must be a member, and each preset's `can_start` says whether the caller's role there can start a writer or reviewer session on one of its harnesses. Without `workspace`, presets are filtered by tenant and `can_start` is absent. A preset for another tenant is absent, not refused. Each harness lists `models`, `default_model`, `tool_approvals`, `writer` (false without tool approvals unless the operator allows unapproved tools), `reviewer` (false without tool approvals), `effort` (whether its provisioner takes one), `credentials` (`configured` when the preset supplies a secret the harness reads as its key, else `none`) and readiness:

- `environment`: `ready` for local. For Substrate, this server loads the bridge key and PEM files and opens no socket; the result is cached for at most 60 seconds and `checked_ms_ago` gives its age. For Microsandbox, `ready` when this build runs it.
- `reachability`: Substrate and native container/Agent Sandbox targets, always `unknown`. The runtime is contacted when a session starts.
- `executable`, per harness: for local, the PATH check of `GET /v1/harnesses`, cached like `environment`. For a remote environment, `unknown`.
- `auth`, per harness: always `unknown` until a session verifies it. Console sessions do not use the server's own login.

Each readiness value is `{state, scope, reason, action, checked_ms_ago?}`. `state` is `ready`, `not_ready` or `unknown`. `scope` is `api_host`, the server answering, which may not be the worker that runs the session, or `environment`. `reason` and `action` are fixed text, never an error message, path, endpoint or secret name. Readiness never refuses a start. Discovery opens no provider, contacts no environment, runs no harness and calls no model.

A session start runs these steps after workspace authorization, the state check and the idempotent replay. The replay's fingerprint is the request body, so it covers the preset ID, not what the preset resolved to.

1. With `require_workspace_execution_presets`, a start without `preset` is `400 preset_required`.
2. The preset must exist and list the caller's tenant; otherwise `404 unknown_preset`.
3. A start with `preset` may not carry `provider` or `provision`: `400 preset_conflict`. Model and effort go in the top-level fields.
4. `harness` must be listed, `model` among its `models` (or absent when none are listed), and `effort` only for a harness that takes one; otherwise `422 preset_choice_unavailable` with `detail.field` and `detail.allowed`. A start that names no model runs the harness's `default_model`.
5. The server sets the start's provider from the preset, adds its secrets and labels, and checks the result as any start.

The session record keeps the resolved provider, labels and model, and the configured values its errors must not show. Its JSON gains `preset`. Every send and resume requires the same labels and runs on the provider the native branch recorded. A steer only delivers input to the running turn, so it keeps the label-free claim of control operations. A session on a remote preset never runs locally: a failed environment fails the run. Removing a preset, or a tenant from one, stops new sessions, not existing ones.

For a session started with a preset, the server projects the text callers read of it, whoever they are. The projection replaces the preset's configured paths, as written and as resolved, its endpoints (as a URL, an origin, `host:port` and a host), passed variable names, and secret names and sources with fixed placeholders such as `[preset substrate-gke: bridge key]`, where each stands as a whole word. It applies to:

- session JSON (`status_detail`, `error`, `result_error`) and the session's messages;
- the `error` and `result` of its operations: `GET /v1/operations/{id}`, idempotency-key recovery, admission responses and their replays, and the unfinished operations `GET /v1/repos/{repo}/operations` lists;
- a repository workspace's legacy answers about its branches: the branch, its inspection, events, event page, children and graph, the branch list, the wait and discard answers, task records with their attempts, and each entry of the repository feed (`GET /v1/repos/{repo}/events/stream`);
- every JSON error on its console routes (`/v1/workspaces/{workspace}/sessions/{session}/...`) and on its legacy branch routes (`/v1/repos/{repo}/branches/{branch}/...`);
- runtime inspection (`GET .../runtime`), the PR read (`GET .../pr`) and a terminal page's `error`. A terminal page's `data` is the terminal's own output and is never rewritten.

A branch takes the projection of the nearest preset task or session among itself and its ancestors. A child or fork of a session's branch shows through that session's preset, and a session without a preset between them ends the walk. An operation merges its private direct-task projection with the projections of every branch it names. Resources outside those associations read exactly as before.

Not projected: the answers of a legacy branch's write routes that return no operation (cancel, steer, graph proposals, rewind and undo), the repository's effects, approvals and map records, content routes (diff, artifacts, scratch areas, inbox and plan), push notifications, trigger run details, operator-configured webhooks and OTLP export.

**Status, 31 October 2026.** Hermetic only: in-process HTTP routes, in-memory queues, the native engine with the fake ACP agent, and Substrate environments that are never reached (a missing bridge key, and a cluster at the always-refused `127.0.0.1:1`), in `cargo test -p branchyard-server --lib presets` (12 tests, passed). They cover configuration validation, discovery and readiness, tenant and role filtering, each refusal, continuation labels, no local fallback, and the projection in a knowledge and a repository workspace: session, operation and message views, the runtime and PR reads, a viewer's legacy branch, event, list, wait, task-record and feed answers, and sessions without a preset, which read byte for byte as the engine serializes them. The nearest-session walk runs over real spawned branches with crafted session rows. The middleware and the operation list are tested with crafted responses and operations, since no hermetic failure produces a synchronous error that names a configured value, or an unfinished operation with an error. The manager MCP's `start_session` passes `preset`: `cargo test -p branchyard-cli --test workspace_manager` (1 test, passed) drives `by mcp manager` over stdio against `by serve` with a required local preset and the fake ACP agent, and `cargo test -p branchyard-mcp` (41 tests, passed) covers its schema and forwarding. Not done: a run against a real Substrate cluster or GKE, a Microsandbox preset, workers holding preset labels on other hosts, a real failure on a child of a preset session's branch, and the console UI.
