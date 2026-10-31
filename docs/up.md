# Up

Written 30 October 2026.

`by up` brings a repository's configured backends up in one command. It appears in the Start group of `by --help`, beside setup and doctor. It reads `branchyard.toml` and the user file ([configuration](setup.md#configuration)), works out what the repository needs and the state of each piece, does what it can (install a harness the policy allows, start the connector gateway), and lists what only a person can do: a login, a key, a trust decision. `by doctor` ([doctor](doctor.md)) reads and repairs nothing; `by up` is the command that acts.

```sh
by up            # install what is allowed, start the gateway, list the rest
by up --plan     # inspect readiness; apply no install/start actions
by up --plan --smoke   # also list Substrate workers: one read-only call, no actor, no model
by up --yes      # confirm the installs the [harnesses] policy would ask about
by up --json     # the steps as JSON, for a script or the Python SDK
```

Opening the repository can initialize `.branchyard/` and its store, including with
`--plan`. The plan applies no install or gateway-start action.

Outside a git repository it stops before any step, with the fix: ``by: not a git repository: /path. Run `git init` here, or cd into one`` (exit 3, `not_a_repository`).

The same operation is the engine's ([`branchyard::up`](../crates/branchyard/src/up.rs)): `Yard::up_plan`, `Yard::up_apply` and `Yard::up` over a `Machine`, and the Python SDK's `Local.up()` ([SDKs](sdk.md#local-mode)). `by up` is one caller of it.

With `[defaults] provider = "substrate"`, `up` and `doctor` authenticate to the
Control API and inspect the configured bridge template. They create no actor and
change no cluster resources. They check in stages and stop at the first that
fails: the bridge key and API token file on this machine, then whether Control
answers, then whether it accepts the token, then the template. Each failure lands
on its own [readiness check](#readiness-checks) with a remedy; after fixing it,
rerun `by up --plan`.

Remote harnesses come from the actor image, so `up` does not require their binaries
on the workstation. It checks whether the selected harness's provisioner can use
the available `[secrets]` sources in a private home. A host OAuth login does not
transfer to an actor. Antigravity currently needs a provisionable `GEMINI_API_KEY`
or `GOOGLE_API_KEY`; `AGY_TOKEN` and a host login do not satisfy this check.
Credential presence means a source is available, not that a model accepted it.
Template inspection proves neither bridge execution nor model connectivity.

For a new GCE worker, [`by setup --guided`](setup.md) reviews the VM and SSH
transport before provisioning. `by compute gce --tunnel-through-iap` offers an
offline JSON plan and explicit `--apply` for operators who supply the cloud
settings themselves. IAP uses a dedicated TCP 22 firewall rule, shown in the
plan. `by up` checks a saved connection; it does not create cloud VMs or change
their firewall rules. See [GCE worker setup](compute-gce.md) for access and cleanup.

For Microsandbox, `up` and `doctor` distinguish build support from host readiness.
On Linux they check read/write access to `/dev/kvm`, with separate remedies for a
missing device and a login missing the `kvm` group. This does not start or qualify
a VM. If the runtime reports an overlong Unix socket path, it recommends a short,
user-owned `MSB_HOME` and initializing the pinned runtime there.

On a terminal, progress reports the readiness and apply phases. Piped output and
`--json` remain deterministic; JSON never prompts. Before installing a harness or starting the connector
gateway, apply checks again whether another invocation already completed it. Failed
steps retain their remedies so rerunning `by up` can finish the remaining work.

## The steps

Each step has a stable dotted name, a state, what was found in one line and, for a step that is not ready, what to do in one line. Rows come in this order.

| Step | What it looks at | Planned | Needs you | Blocked |
|---|---|---|---|---|
| `store` | `.branchyard/`, made by opening the repository | | | |
| `harness.<id>` | each harness the configuration names: `[defaults] harness`, the `[fleet]` candidates, judges and goal judges, the `[knowledge]` distiller; `claude-code` when none is named. A profile ID counts as its harness | not installed, and `[harnesses] install` in your user file is `auto`, or `ask` with `--yes` (or your yes on the terminal) | not installed, and the policy is `ask` without a terminal or `--yes`, `never`, or the harness is not in `allow`: the install command is shown to run by hand | the catalog has no install command this machine's tools can run |
| `login.<id>` | the harness's login as `by harnesses` detects it, in its words (`logged in`, `key set (VARIABLE)`, `key stored (VARIABLE)`, `credentials found`) | | `logged out` or `login not verified`: the same command `by doctor` gives, `by harnesses login ID` (with `--api-key` for a harness that has only a key variable), or the key variable to set | |
| `secret.<NAME>` | each `[secrets]` entry and each `[models]` backend's key: its variable set, or its file present, by name | | the variable is unset or the file is missing | |
| `models` | the model gateway `[models]` describes, which starts with each turn | | | `[models]` does not convert for the gateway |
| `connectors` | the connector gateway, when `[connectors]` is set or the repository has bundles: the URL `gateway` pins, else the one registered for this repository ([registry](registry.md)) | nothing listens and Anvil is on `PATH` (or at `[connectors] anvil`): started in the background, as `by gateway start` does, and ready only once something answers at its URL | | Anvil is not installed, or the started gateway does not answer (its log says why) |
| `provider` | `[defaults] provider` when it is not `local`: Microsandbox build and KVM access; Substrate authentication and template inspection; recipe configuration and trust | | a recipe you have not trusted as it is now (`by recipe trust NAME`) | missing provider configuration or build support; unavailable KVM; failed Substrate connection or template check |
| `workspace` | the `[workspace]` scripts, when there are any | | not trusted as they are now (`by workspace trust`) | |

| State | Means |
|---|---|
| `ready` | Nothing to do. |
| `planned` | `up` does this itself: install, start the gateway. `--plan` shows it and applies no actions. |
| `needs_person` | A decision or a secret only you supply. A row with an `action` is an install the policy makes you confirm: on a terminal `by up` asks, `--yes` answers. |
| `blocked` | This configuration or machine cannot do it; the row says what to change. |

| Exit | When |
|---|---|
| 0 | Every step is ready (after applying, or in the plan). |
| 1 | At least one step is not. |
| 2 | A usage error, as for every `by` command. |

Configuration, repository and denied-command failures use the shared [CLI exit
status contract](cli.md#exit-status); a missing repository exits 3 and an
invocation from a harness branch exits 4.

Nothing prints a secret's value: a secret is reported by its variable's name or its file's path. Installing follows the `[harnesses]` policy exactly as `by harnesses install` does ([harness lifecycle](harness-lifecycle.md#installing-and-updating)): only your user file can allow it (a repository's `install = "auto"` is a configuration error), an unset policy asks on a terminal and refuses elsewhere, and each install is recorded in the harness log as `by up`. Harnesses are detected afresh, not from the minute's cache, so a login made since the last `by harnesses` counts. A harness running on a branch cannot run `by up`, and `--remote` is refused: a server brings up its own machine ([server](server.md)).

`--json` prints `{"steps": [{"name", "state", "detail", "todo"?, "action"?}], "checks": [{"id", "label", "status", "detail", "next"?}], "state", "ready"}`, where `action` is `{"kind": "install", "plan": {...}}` (the `InstallPlan` `by harnesses install` shows) or `{"kind": "start_gateway"}`, and `checks` are the seven [readiness checks](#readiness-checks).

```
STATE         STEP                 DETAIL
ready         store                /home/me/src/app/.branchyard
ready         harness.codex        /home/me/.local/bin/codex (version 0.157.1)
needs_person  login.codex          not logged in (codex login status: Not logged in)
ready         secret.GITHUB_TOKEN  $GH_TOKEN is set
planned       connectors           no gateway runs for this repository

To do:
  login.codex: log in: `codex login --device-auth`, or set OPENAI_API_KEY

1 step(s) need you
```

## Readiness checks

Below the steps, `by up` reports seven checks, always in this order. Each says
what this run established. A harness found on `PATH` is never reported as able to
run a task: being installed, being authenticated and having somewhere to run are
separate checks. `by doctor` shows the same seven when Substrate or a server is
configured, and `by setup --up` and `by setup --guided` include them in their
readiness result.

| Status | Means |
|---|---|
| `verified` | This run established it: a version command answered, Control accepted the token, a worker listing showed a free slot. |
| `unverified` | A source is present and nothing confirmed it: a key variable, a credential file, an image that should hold the harness. |
| `unknown` | Nothing was asked, or the answer said nothing. |
| `failed` | Checked, and missing, refused, unreachable or unsupported. |
| `not_applicable` | The configured target has no such thing: no control plane for local execution. |

| Check (`id`) | On this machine (`local`) | With Substrate |
|---|---|---|
| CLI and native harness installed (`cli_and_harness_installed`) | `verified` when each needed harness is on `PATH` and its version command answered; `unverified` when it gave no version; `failed` when it is missing, off `PATH` or older than the driver needs | `unverified`: the harness is in the template's image and has not run. `by substrate check --smoke --harness agy` runs it |
| Configuration valid (`configuration_valid`) | the files load and the provider's section is present | the same; no `[substrate]` section fails |
| Operator credentials available (`operator_credentials_available`) | `not_applicable` | `verified` once Control accepts the token and the template's key matches the bridge key; `failed` for a missing or unreadable key or token file, a refused (for example expired) token, or a key mismatch; `unverified` when the files are readable and Control did not answer |
| Remote control plane reachable (`remote_control_plane_reachable`) | `not_applicable` | `verified` when Control answered, even to refuse the token; `failed` with no answer |
| Worker capacity available (`worker_capacity_available`) | `not_applicable` | `unknown` without `--smoke`. With it: `verified` for a free actor slot, `failed` with no schedulable worker or none with room, `unverified` when workers report no actor capacity, `unknown` when Control will not list them |
| Requested execution capabilities supported (`execution_capabilities_supported`) | `failed` when the configured connector gateway cannot run | `verified` when the template declares the bridge execution contract; `failed` for a template Control has but this client cannot use |
| Native harness authentication verified (`harness_auth_verified`) | `verified` only when the harness's own status command says logged in; `unverified` for a key variable or credential file alone, since only a model call would confirm it; `failed` when logged out, at its usage limit, or with an Antigravity key its settings ignore; `unknown` when not checked | `unverified` when the actor's private home has a credential source; `failed` when it has none |

Microsandbox reports this host's KVM access as `unverified` capacity, since no
microVM is started. A recipe's own transport decides where its tasks run, so its
capacity is `unknown`. Every check that is not `verified` or `not_applicable`
carries a `next`. The text output lists them under *To verify* and ends with a
count, such as `checks: 4 verified, 3 not applicable`. The exit status still
follows the steps.

`--smoke` adds one read-only `ListWorkers` call to the configured Control
endpoint, bounded at 30 seconds like the template inspection. It contacts nothing
else, creates no actor, changes no resource and calls no model. On this machine it
adds nothing: detection already ran each harness's version and login status
commands, and the only further check would be a model call. To run the harness
itself, `by substrate check --smoke --harness agy` creates one disposable actor,
runs `agy --version` in it, and deletes it.

**Status, 30 October 2026.** Hermetic only. The seven checks were run against fake
harnesses on `PATH`, the fake Substrate cluster (a refused port, missing, wrong
and expired tokens, a missing template, a key mismatch, and supplied worker lists)
and a mock server (a refused token, no answer, a worker inventory). Not done: no
run against a live GKE Substrate deployment, so whether a deployment's operator
token may call `ListWorkers` is unknown (a refusal is reported as `unknown`); no
real harness login was checked.

## From the SDKs

**Rust.** `Yard::open_configured(root)` opens the repository and gives the yard what the configuration files describe ([`branchyard::configure`](../crates/branchyard/src/configure.rs)): the connector gateway, the approvals, the model gateway, the knowledge settings, and the `[defaults]` every `yard.task(...)` then starts from, with the provisioning (`[secrets]`, `[mcp]`, `[network]`, grants, model access) filled in when the task runs. `yard.up_plan(&config, &options, &mut machine)` computes the plan, `yard.up_apply` does the planned steps, `yard.up` both; `branchyard::up::LocalMachine` is this machine, and a test passes a fake. The local machine cannot read the trust decisions `by recipe trust` and `by workspace trust` keep, so from the Rust SDK those steps say to check with `by`; `by up` reads them.

**Python.** `branchyard_client.Local(root).up(plan=False, yes=False)` runs `by up --json` and returns a `Plan` (`steps`, `todo`, `ready`, `step(name)`); `Local.doctor()` and `Local.harnesses()` likewise. `Client.local(root)` starts a `by serve` of the process's own for everything else ([SDKs](sdk.md#local-mode)).

## What is tested

- The engine (`crates/branchyard/src/up.rs`), over a fake machine: the needed harnesses from every table that names one; a missing harness planned, confirmed or left to the person by each policy; an install applied, verified at its pinned version and followed by a login step, or blocked with the command's output when nothing was installed, and never run when no tool can; secrets by variable and file, a backend's key included, nothing printing a value; the gateway started when Anvil is there, blocked when not, left alone when it listens, and blocked with the reason when the start fails; recipe and workspace trust in each state; a sandbox provider this build cannot run; the JSON shape.
- `by up` (`crates/branchyard-cli/tests/up.rs`, the built `by`, hermetic with a fake harness and a fake `npm`): a configured harness installed and logged in is ready and exits 0; `--plan` runs nothing, then the install runs once under `install = "auto"`, the login is yours until the fake logs in, and the install is in the harness log; without a terminal an unset policy leaves the install to the person and `--yes` cannot widen it, a repository file that allows installs is refused by the loader, and the text report lists what to do; secrets by name, never shown; a pinned gateway with no Anvil is blocked; inside a harness and with `--remote` it refuses.
- The readiness checks (`crates/branchyard/src/up/readiness.rs`, `crates/branchyard/src/up.rs`): the installed check verified only when the version command answered; each login state on its own status; each Substrate stage (unusable credentials, no answer, a refused token, a key mismatch, an unusable template, an inspected one with and without worker listings, full and empty pools) on its own checks, with a `next` on every failure; a bridge key or token file that is missing or empty failing before any call; a plan applying an install recomputing the checks; a blocked connector gateway failing capabilities; Substrate on a refused port with `--smoke` running nothing on the machine.
- `by up --plan` and `--smoke` with the built `by`: a fake harness found and logged out is installed but not authenticated, and `--smoke` runs only its version and login status commands (`crates/branchyard-cli/tests/up.rs`); against the fake Substrate cluster, an authenticated plan, a worker listing with `--smoke` (a free slot, then a full pool), an expired token, `by doctor --json` and `by substrate check --json`, no harness looked for on the workstation, and no actor created (`crates/branchyard-cli/tests/substrate.rs`).
- The Python `Local` and `Client.local` over a fake `by` (`tests/test_sdk_client.py`), and `Client.local` against the real `by serve` (`crates/branchyard-cli/tests/sdk_client.rs`).

## Not done

`by up` acts on this machine only; `by serve` has no route that brings a worker's machine up. The Python `Plan` does not expose `checks` yet; read them from `by up --json`. Placement against an actor's CPU and memory requests is not checked by `--smoke`, only free actor slots. It writes no secret: a key is yours to export or put in the file `[secrets]` names. A recipe's or the workspace's trust is still your decision made with `by recipe trust` and `by workspace trust`; `up` only says when it is needed. The Rust SDK's `LocalMachine` starts the gateway by running `by gateway start`, so a program without a `by` beside it gets that step blocked. There is no TypeScript client yet.
