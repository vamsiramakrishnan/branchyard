# Admin compute console

Written 8 November 2026.

The admin compute console is a page and two routes on the Branchyard server
for the operator who sets up compute. The operator plans GCE or GKE compute
there, publishes it as an [execution preset](server.md#execution-presets), and
sees which live workers can run each preset. Developers do not open this page.
They point `by` at the same server with `--remote` ([remote mode](server.md#remote-mode-in-by))
and their turns run on the compute the operator set up.

## The model: a control plane and clients

| Role | Uses | Owns |
|---|---|---|
| Operator | `https://SERVER/admin`, `by compute gce`, `tools/agent_sandbox_gke.py`, the server configuration | cloud projects, clusters, VMs, runtimes, images, `allow_providers`, `execution_presets`, tokens |
| Server and workers | `by serve`, `by worker`, a shared `--database` | the queue, worker labels, the kubeconfig or Docker/KVM runtime each worker dispatches to |
| Developer | `by --remote https://SERVER ...`, the workspace console | branches and sessions; never cloud credentials |

Local `by` keeps working with no server. The console only adds a shared
control plane for teams.

This is closer to how a team runs agents than each developer provisioning a VM
from a laptop. Cloud credentials stay with the operator, isolation is chosen
once per environment and enforced by the server (`allow_providers`, presets,
tenants), and developers see named environments, not kubeconfigs. The cost is
an always-on server and a network path from developers to it.

## Open the console

Serve the server as usual, then open `/admin` on its address (or on `--admin-listen`'s) and paste a token
with the `admin` scope. The page has no data of its own. It keeps the token in
the browser tab's session storage and sends it only to this server's API. The
page and its script are public. Their data routes are not:

| Method and path | Scope | Does |
|---|---|---|
| `GET /admin`, `GET /admin/admin.js` | none | the console's page and script, with a `script-src 'self'` content security policy |
| `GET /v1/admin/compute` | `admin` | every execution preset read as a compute target, and every live worker with the targets it can claim |
| `POST /v1/admin/compute/plan` | `admin` | the commands, worker label and preset for new GCE or GKE compute |

## Keep it apart from developers

Two settings keep developers out of the console:

- **Admin tokens are asked for by name.** `branchyard-server token new` (and
  `by serve token new`, `--link` included) gives `read,run,merge` unless
  `--scopes` names others. An operator's token is
  `token new --name ops --scopes read,admin`. Credentials already in a
  configuration keep the scopes they list, and a `tokens` entry without
  `scopes` still has all four, as before.
- **Its own listener.** `--admin-listen ADDR` (or `admin_listen` in the
  configuration) serves `/admin`, `/v1/admin/*` and `/healthz` on `ADDR` only, with
  the main listener's TLS, authentication, limits and request metrics. The main
  listener then answers `404` for the console and its routes, and the admin
  listener answers `404` for everything else. Put `ADDR` on a private network,
  or behind an identity-aware proxy. A non-loopback address needs TLS or
  `--insecure-bind`, a worker takes none, and it may not equal `--listen`.

```sh
by serve --listen 0.0.0.0:8421 --tls-cert cert.pem --tls-key key.pem --admin-listen 10.0.0.5:8422
```

## Compute targets

`GET /v1/admin/compute` answers `{"origin", "targets", "workers"}`. `origin` is
`configured`, or `default` when no `execution_presets` exist and the server offers
its implicit local preset. Each target is one preset. `runs_on`, `isolation`, `network` and `namespace`
are what the provider reports of itself (`Provider::compute`), so a new
provider describes its own compute:

| Field | Meaning |
|---|---|
| `preset`, `label` | the preset's ID and name |
| `provider` | the provider kind, as `allow_providers` names it |
| `runs_on` | `worker` (on the claiming worker's machine), `kubernetes` (an Agent Sandbox the worker creates on GKE) or `substrate` |
| `isolation` | `process`, `gvisor`, `runc`, `microsandbox`, `kata`, `substrate` or `recipe` |
| `network`, `namespace`, `image` | the native provider's network choice (`deny` or `unrestricted`) and Kubernetes namespace, `null` for other providers; the image, when it takes one |
| `harnesses`, `require_labels`, `tenants` | from the preset |
| `allowed` | whether the provider kind is in `allow_providers` (`local` always is) |
| `workers` | live workers whose labels include every required label, as the queue's claim decides |
| `state`, `next` | `ready`, `no_worker` or `provider_not_allowed`, and fixed text saying what to do |

`ready` means a worker can claim the preset's turns. It does not mean the
runtime, image or kubeconfig works on that worker. Those are checked when a
turn starts, as [discovery](server.md#execution-presets) already reports.

## Plan new compute

`POST /v1/admin/compute/plan` takes `id`, `platform` (`gce` or `gke`), `runtime`,
`project`, `zone`, `name` (VM or cluster) and `version` (a Branchyard release).
It also takes `image` (pinned `@sha256:`), `harnesses` and `tunnel_through_iap`
(GCE). A GKE plan also needs `gke_version`, `service_account` and `master_cidr`
(a private `/28`). Unknown fields are refused. A request that cannot be planned
is `422 invalid_plan` with the reason.

| Platform | Runtime | Setup command | Preset provider |
|---|---|---|---|
| `gke` | `gvisor` | `python3 tools/agent_sandbox_gke.py ...` | `agent_sandbox`, namespace `branchyard-gvisor`, runtime `gvisor` |
| `gke` | `kata` | the same helper | `agent_sandbox`, namespace `branchyard-microvm`, runtime `microvm` (GKE's managed Kata with Cloud Hypervisor) |
| `gce` | `gvisor` | `by compute gce ... --gvisor` | `container`, runtime `gvisor` |
| `gce` | `microsandbox` | `by compute gce ... --backend=microsandbox` | `microsandbox` |
| `gce` | `process` | `by compute gce ...` | `local` |

The answer lists `steps` as argv arrays, each with `on`: `operator` (a machine
with the cloud credentials) or `worker` (the machine that dispatches the
target's turns). It also returns the worker `label` (`compute:ID`), the
`allow_provider` to add, the `preset` entry to add to `execution_presets`, and
fixed `notes`. Every preset network is `deny`.

The route runs nothing. The operator reviews the helper's offline plan, applies
it with their own credentials, adds the preset and the provider to the
configuration, and starts the dispatching worker with that configuration:
`by worker` (or `by serve` on GKE) `--config SERVER_CONFIG --allow-provider KIND --label compute:ID`.
The worker needs both, because it checks a native provider against its own
`allow_providers` and resolves a `--env` task's preset again from its own
`execution_presets`; `--allow-provider` is left out for `local`, which is always
allowed. The server never holds project IAM. On
GKE, the worker that carries the label needs the kubeconfig the helper writes
in its state directory. On GCE, a worker shares the server's `--database` and
serves the same checkouts ([several servers on one database](server.md#several-servers-on-one-database)).
Without a shared database, use the VM as its own server through the `by --remote`
URL its `deployment.json` records ([compute on GCE](compute-gce.md)).

## What developers run

```sh
export BRANCHYARD_REMOTE=https://by.example:8421
export BRANCHYARD_TOKEN_FILE=~/.config/branchyard/token
by run "Make the flaky parser test deterministic" --check "cargo test" --yes
```

A developer lists the environments their tenant is offered and runs on one by
ID; the server supplies its provider, secrets, worker labels and model, and
checks them again when the turn runs:

```sh
by run "Make the flaky parser test deterministic" --env gke-kata --yes
by fan "Try three fixes" --harness claude-code,codex --env gce-gvisor --yes
```

`--env` needs `--remote`, starts a task or a fan only (a fork or reincarnation keeps its
branch's environment), and is refused with `--provider`, `--command` or provisioning
flags. See [direct tasks on a preset](server.md#direct-tasks-on-a-preset) for what the
server checks and what is not done yet. Workspace console sessions choose a preset by ID
as before.

**Status, 8 November 2026.** Hermetic only. `cargo test -p branchyard-server --lib admin_compute`
(11 tests, passed) covers preset-to-target reading through `Provider::compute`,
label matching against constructed live workers, the implicit local preset,
every plan shape and its refusals, admin scope on both routes, the public page
with `401` on its data route through the real router, the main and admin
routers with and without `admin_listen` (each `404` for the other's routes), and
the configuration checks on the admin listener. `cargo test -p branchyard --lib providers::`
checks every provider's `compute()`, and `cargo test -p branchyard-server --lib cli`
the `read,run,merge` token default. Direct tasks on a preset are covered by
`cargo test -p branchyard-server --lib presets` (resolution per tenant, labels,
secrets, model, every refusal, and a run on a labelled local preset with the fake
agent) and `cargo test -p branchyard-cli --test remote run_env` (the built `by`
against `by serve`: `--env` runs with the server's harness command, an unknown
preset and a client `--command` are refused, and `fork --env` and a local `--env`
exit 2). The page was driven in headless Chromium
against a mocked API. Not done: a running server with `--admin-listen` bound
and reached over TLS; a plan applied from the console's output to a real GCE
project or GKE cluster; applying plans or writing presets from the server;
a direct preset task on a real GKE or GCE provider; follow-up turns on such a branch; error projection for direct preset tasks.
