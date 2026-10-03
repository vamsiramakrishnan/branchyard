# Ambient registry

Branchyard starts and depends on many things: a connector gateway, a turn's egress proxy, a server, workers, sandboxes, recipe machines, pool keepers, and soon a model gateway. Each one announces itself in a registry with what it can do and a lease. Consumers find it by capability, not by a URL in a file. When its owner stops, the lease runs out and what Branchyard started for it is reclaimed. This is the rule "Ambient, not configured" from the [roadmap](roadmap.md#direction-cowork-parity).

Status: **built and tested hermetically** (2 October 2026, branch `agent/ambient-15`). The connector gateway, egress proxies, sandboxes, recipe machines, pool keepers, `by serve` and workers register. The live catalogs read the official MCP registry and npm. See [What is not done](#what-is-not-done).

## Records

A service record (`branchyard::services::Service`) has:

| Field | What it holds |
|---|---|
| `id` | Unique in its registry. A fresh one per registration unless the registrant names one. |
| `kind` | A label: `connector_gateway`, `model_gateway`, `egress_proxy`, `server`, `worker`, `sandbox`, `recipe_machine`, `mcp_server`, `pool_keeper`, or any other (1 to 63 of `a-z 0-9 . _ - :`). |
| `capabilities` | Typed values by key: a flag, a number, text or a list of text. For example `connectors = ["github"]`, `issuer`, `models`, `labels`, `protocol`. |
| `endpoints` | A URL, a Unix socket, or "in process" for something reached only through its owner. |
| `owner` | The registrant: its own id, host and boot, pid and start time, and the branch, operation or server principal it serves. |
| `health` | `healthy`, `degraded`, `starting` or `unhealthy`. |
| `weight` | Among equally healthy records, heavier ones are picked first. |
| `reclaim` | What Branchyard started for it, and so reclaims (below). Absent for anything Branchyard did not start. |
| `state` | `live`, `left` (deregistered), `expired`, `reclaiming`, `reclaimed`. |
| lease | `lease_until_ms`, `renewed_ms`, `registered_ms`, `changed_ms`. |
| `seq` | The registry's change counter at the record's last change. |
| `note` | Why it expired, or what reclaiming it did. |

## Operations

- **Register.** A new record, or a replacement of one the same owner holds. Another owner's live record is refused (`AlreadyExists`, or `409 service_held` over HTTP). A record whose lease ran out may be taken.
- **Renew.** The owner extends the lease and may change its health. A renewal of a record that is no longer live fails; the owner registers again. `Registration` does this from a thread of its own every third of the lease (default lease 30 seconds) and deregisters when dropped.
- **Deregister.** The owner marks it `left`. Nothing is reclaimed.
- **Resolve.** Among live records of a kind whose capabilities hold what the query asks (equal values; a list holding the wanted item or items), the healthiest first (`unhealthy` never), then the heaviest, then the first by id. Every caller picks the same one.
- **Watch.** Every change takes the next value of one counter. `since(seq)` returns what changed after it; `watch` waits for a change, looking again every 100 ms.
- **Expire and reap.** `expire` marks `expired` every live record whose lease ran out, or whose owner process is known gone from this host (its pid no longer has its start time). `reap` then claims each expired record (a compare-and-set on `seq`, so two reapers never reclaim one record), reclaims it, and marks it `reclaimed`. A reclaim that fails leaves it `expired` with the reason, to try again. A reclaim left under way by a reaper that stopped is taken over after ten minutes. Records that left or were reclaimed are kept an hour, then pruned.

Times come from the caller's `Clock`: the system's, or a manual one in tests, so nothing waits on time.

## Leases and reclamation

What is reclaimed, and how. Each reuses the code that already owns the resource:

| `reclaim` | How |
|---|---|
| `process` | Only on its host, only while the pid still has the recorded start time (a reused pid is never signalled), as recovery kills a harness. With a `group`, what is left of the process group too. |
| `sandbox` | The branch's own recovery first, which brings the harness's work back and destroys the sandbox its turn journaled. Then, if the sandbox is still there and not kept paused for the branch's next turn, its provider destroys it: a recipe's recorded `destroy`, Microsandbox, or Substrate. |
| `pool_slots` | The pool's own reclaim: slots whose filler or claimer is gone, and directories with no record. |

Never reaped: a record without `reclaim`, a record that left, and anything registered over a server's API.

Who reaps:

- every `Yard::open` and `Yard::recover`, after branch recovery, when the repository has a registry file;
- `by services gc` and `Yard::reclaim_services`;
- a server, on its recovery interval, for its fleet's records and each served repository's;
- `by gateway start` before it registers, and `by gateway stop` after.

A killed owner on this host is known gone at once, so its services are reclaimed by the next of these, without waiting for the lease.

## What registers

| Kind | Who | Reclaim |
|---|---|---|
| `connector_gateway` | `by gateway start` (the supervisor, with the Anvil process it runs) | the gateway process and, when started in the background, the supervisor's process group |
| `connector_gateway` (adopted) | `by gateway start` when something it did not start already listens at the pinned URL; lease ten minutes, owned by no process | none |
| `egress_proxy` | each turn with a restricted network, while it runs; the proxy's loopback URL, or the network namespace it serves | none: it lives in the engine's process |
| `sandbox`, `recipe_machine` | each turn on a sandbox or a recipe's machine, from when it has one until it is released | `sandbox` |
| `pool_keeper` | each `PoolKeeper` (`by serve`, `by worker`) | `pool_slots` |
| `server` | `by serve`, in its fleet's registry and in each served repository's | none |
| `worker` | every process that claims queued operations: its row in the workers table, read as a service | none (its claims are reaped by the queue) |

A model gateway registers itself over HTTP (below), or with `Yard::register_service` on the same machine.

## Stores

- **Local.** One SQLite file per repository, `.branchyard/registry.db`, made 0600 (and made 0600 again if loosened), in write-ahead-log mode. Every `by` process on the machine shares it; writers take turns in `BEGIN IMMEDIATE` transactions. `BRANCHYARD_REGISTRY` names another file, to share one registry between repositories.
- **Fleet.** A server keeps its records in its operation store: SQLite in its data directory, or PostgreSQL (`by_services`, `by_service_seq`, made by the same catalog-checked, one-step-at-a-time migration as the store's other tables). Workers are not copied there: the workers table is read as `worker` records, leased until three beats after the last.

Both run the same operations (`branchyard::services::Rows`), and pass one conformance test (`branchyard::services::conformance`).

## Discovery

### `by services`

`by services [--kind K] [--all] [--json]` lists this repository's registry: kind, id, state, health, lease left, owner, endpoint and capabilities. `--all` includes records that left or were reclaimed. With `--remote URL` (or `BRANCHYARD_REMOTE`) it lists the server's fleet. `by services gc [--json]` expires and reclaims now and says what it did (with `--remote`, the `admin` scope).

### Finding a local server

`by serve` registers itself, with its URL and the repository name, in each served repository's registry. `by services --kind server` in that repository shows it. `by` does not switch to it on its own: `--remote` still says where commands go. A `by` that used whatever server it found would change what every command does without being asked.

### Over HTTP

| Route | Scope | What |
|---|---|---|
| `GET /.well-known/branchyard` | none | `{service: "branchyard", version, api: "/v1", jwks_uri, services_uri, repos, scopes, services: [{id, kind, capabilities, health}]}`. `jwks_uri` (`/.well-known/jwks.json`) appears when the server has connectors. Endpoints and owners are not public. |
| `GET /v1/services[?kind=K]` | `read` | every record, workers included, with endpoints, owners and leases |
| `POST /v1/services` | `admin` | `RegisterServiceRequest {id?, kind, capabilities, endpoints, health, weight?, ttl_seconds?}`; registering the same id again renews it |
| `DELETE /v1/services/{id}` | `admin` | deregister one this principal registered |
| `POST /v1/services/gc` | `admin` | expire and reclaim now |

The types are in `schema/contract.json` (`RegisterServiceRequest`, `ServiceList`, `WellKnown`). The Rust client has `services`, `register_service`, `deregister_service`, `reclaim_services` and `well_known`.

## The connector gateway

`[connectors] gateway` is now optional. Without it, `by gateway start` picks a free loopback port, and its supervisor registers the gateway with its issuer (the yard's), the connectors its bundles serve, its URL and `sandbox_url` when set. Consumers (`by connect`, `by gateway status`, a turn's `ANVIL_GATEWAY_URL`, `--issue` through a tracker's connector) resolve the live gateway whose `issuer` is this yard's. A configured `gateway` is an explicit pin and always wins. See [connectors](connectors.md#running-the-gateway).

## Live catalogs

`catalog/connectors.toml` and `catalog/harnesses.toml` stay the pinned baseline, vendored and checked as [vendoring](vendoring.md) says. `by catalog refresh` adds what live registries say:

- **Connectors** from the official MCP registry: `GET {registry}/v0/servers?limit=100&version=latest`, following `metadata.nextCursor` for at most `--max-pages` pages (default 20). Each server becomes an entry pinned at the version read (`source = "mcp-registry:<name>@<version>"`): a remote (`remote-mcp`, its URL, header names as credentials) or a package (`stdio-package`, `identifier@version`, variable names as credentials). An entry whose URL or package is a baseline entry's records it as `same_as`. The shape was read from the live API on 1 October 2026; tests use a local mock.
- **Harness versions** from npm: `GET {registry}/{package}/latest` for each catalog harness installed with `npm install -g`, with the release's `dist.integrity` as its pin. `by harnesses` then notes a newer release beside an installed harness, and `by harnesses update ID` names it; nothing installs it unless `--version` asks.

`--mcp-registry` (`BRANCHYARD_MCP_REGISTRY`, default `https://registry.modelcontextprotocol.io`), `--npm-registry` (`BRANCHYARD_NPM_REGISTRY`, default `https://registry.npmjs.org`) and `--only connectors|harnesses` choose what is read. `by catalog status [--json]` shows what is cached.

The cache is `$BRANCHYARD_CATALOG_DIR`, else `~/.cache/branchyard/catalog`: every response under `responses/`, the derived `connectors.json` and `harnesses.json`, and `manifest.json` with each file's SHA-256 and each response's ETag. A refresh sends `If-None-Match` for a response whose file still verifies; a `304` reuses it. Every reader verifies each file against the manifest and refuses one that does not match: `by catalog status` fails, and `by connectors catalog` prints the baseline alone with a warning. A refresh mends it.

Nothing is fetched unless asked: no command on the hot path reads the network, and no test does. The HTTP client is Branchyard's own and does not use `HTTPS_PROXY`.

## Security

- The local registry names processes, sockets and what to reclaim, so it is 0600, in the repository's `.branchyard/`. Only processes that can write it can register, and they run as you.
- A reclaim only ever stops a process on its own host whose pid still has the recorded start time, or destroys a sandbox through the branch's own recovery and provider. A record without `reclaim` is never acted on.
- On a server, only a principal with the `admin` scope registers, renews or deregisters, and only its own records (the owner is the principal). A registration over HTTP never carries a `reclaim`; one in the body is ignored. `worker` records are not registered over HTTP; workers appear by claiming work.
- `GET /.well-known/branchyard` is public and says what services exist and can do, not where they are or who runs them.
- Live catalogs are opt-in, pinned to the versions read, checksummed, and verified on every read.

## Tests

- SDK (`crates/branchyard/tests/services.rs`, 8): conformance on the SQLite file and in memory; the file's mode; four handles and four processes registering at once (each change numbered once); a killed owner's leaked process (a `sleep` in its own group) reclaimed, a reused pid never signalled, a deregistered record never reaped; a recipe machine made through `fake-ssh` destroyed through its recorded `destroy`; a gone pool keeper's slots reclaimed. Unit tests for capabilities and records (3). `tests/recipe.rs` also checks that a crashed turn's machine was registered and reaped.
- Server (`crates/branchyard-server/tests/services.rs`, 2; `tests/postgres.rs`, 1): conformance on the operation store (memory, file, three handles) and on PostgreSQL (three connections); `/.well-known/branchyard` over real HTTP without a token; a model gateway registering, found by capability, renewed, deregistered; a `reclaim` in the body dropped; `read` refused registration; the server announced in the repository's registry and deregistered on stop.
- CLI (`crates/branchyard-cli/tests/services.rs`, 3; `tests/remote.rs`, 1; argument tests): an unpinned gateway started, registered, found by `by connect` and `by gateway status`, a pin winning, its supervisor killed and the leaked gateway reclaimed by `by services gc`; a gateway listening at a pinned URL adopted and never reclaimed; `by catalog refresh` against a local mock with ETags (`304` on the second refresh), pins, `same_as`, and a corrupted cache refused, then mended; `by services` over `--remote` and finding `by serve` locally. Unit tests for the catalog mapping and npm packages (2).

## What is not done

- `by` does not use a local server it finds on its own; `--remote` still decides.
- `by services` has no `watch` command; the SDK's `watch` and `since` are there.
- A server's own connector gateway (`run_gateway`) and MCP servers Branchyard starts for a turn (`by mcp`) do not register yet.
- The live harness catalog covers npm-installed harnesses only; harnesses installed by script (`curl ... | sh`) have no registry to read.
- The catalog fetcher does not go through an HTTP proxy.
- The model gateway is built separately; it registers as `model_gateway` through the API or the SDK.
