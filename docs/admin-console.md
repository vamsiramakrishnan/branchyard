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

Serve the server as usual, then open `/admin` on its address and paste a token
with the `admin` scope. The page has no data of its own. It keeps the token in
the browser tab's session storage and sends it only to this server's API. The
page and its script are public. Their data routes are not:

| Method and path | Scope | Does |
|---|---|---|
| `GET /admin`, `GET /admin/admin.js` | none | the console's page and script, with a `script-src 'self'` content security policy |
| `GET /v1/admin/compute` | `admin` | every execution preset read as a compute target, and every live worker with the targets it can claim |
| `POST /v1/admin/compute/plan` | `admin` | the commands, worker label and preset for new GCE or GKE compute |

## Compute targets

`GET /v1/admin/compute` answers `{"origin", "targets", "workers"}`. `origin` is
`configured`, or `default` when no `execution_presets` exist and the server offers
its implicit local preset. Each target is one preset:

| Field | Meaning |
|---|---|
| `preset`, `label` | the preset's ID and name |
| `provider` | the provider kind, as `allow_providers` names it |
| `platform` | `worker` (on the claiming worker's machine), `gke` (an Agent Sandbox the worker creates) or `substrate` |
| `isolation` | `process`, `gvisor`, `runc`, `microsandbox`, `kata`, `substrate` or `recipe` |
| `network`, `image`, `namespace` | the native provider's network choice, image and Kubernetes namespace, when it has them |
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
it with their own credentials, starts the dispatching worker with the label, and
adds the preset to the configuration. The server never holds project IAM. On
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

Workspace console sessions choose a preset by ID. Direct tasks from `by run`
cannot choose a `container` or `agent_sandbox` preset yet (`403 preset_required`),
so a developer's `by --remote` turn reaches native GKE and GCE isolation only
through a workspace session today.

**Status, 8 November 2026.** Hermetic only: `cargo test -p branchyard-server --lib admin_compute`
(9 tests, passed) covers preset-to-target reading, label matching against
constructed live workers, the implicit local preset, every plan shape and its
refusals, admin scope on both routes, and the public page with `401` on its
data route through the real router. The page was driven in headless Chromium
against a mocked API. Not done: a plan applied from the console's output to a
real GCE project or GKE cluster; a server opened in a browser with live
workers; applying the plan from the server; writing the preset into the
configuration from the console; choosing a preset from `by run --remote`.
