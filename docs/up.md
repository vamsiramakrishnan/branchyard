# Up

Written 18 October 2026.

`by up` brings a repository's configured backends up in one command. It reads `branchyard.toml` and the user file ([configuration](setup.md#configuration)), works out what the repository needs and the state of each piece, does what it can (install a harness the policy allows, start the connector gateway), and lists what only a person can do: a login, a key, a trust decision. `by doctor` ([doctor](doctor.md)) reads and repairs nothing; `by up` is the command that acts.

```sh
by up            # install what is allowed, start the gateway, list the rest
by up --plan     # what it would do and what is missing; changes nothing
by up --yes      # confirm the installs the [harnesses] policy would ask about
by up --json     # the steps as JSON, for a script or the Python SDK
```

Outside a git repository it stops before any step, with the fix: ``by: not a git repository: /path. Run `git init` here, or cd into one`` (exit 1).

The same operation is the engine's ([`branchyard::up`](../crates/branchyard/src/up.rs)): `Yard::up_plan`, `Yard::up_apply` and `Yard::up` over a `Machine`, and the Python SDK's `Local.up()` ([SDKs](sdk.md#local-mode)). `by up` is one caller of it.

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
| `provider` | `[defaults] provider` when it is not `local`: Microsandbox, as the engine checks it; a recipe, as your trust decision says | | a recipe you have not trusted as it is now (`by recipe trust NAME`) | no `[microsandbox]` or a build without the feature; no `[recipes.NAME]` |
| `workspace` | the `[workspace]` scripts, when there are any | | not trusted as they are now (`by workspace trust`) | |

| State | Means |
|---|---|
| `ready` | Nothing to do. |
| `planned` | `up` does this itself: install, start the gateway. `--plan` shows it and does nothing. |
| `needs_person` | A decision or a secret only you supply. A row with an `action` is an install the policy makes you confirm: on a terminal `by up` asks, `--yes` answers. |
| `blocked` | This configuration or machine cannot do it; the row says what to change. |

| Exit | When |
|---|---|
| 0 | Every step is ready (after applying, or in the plan). |
| 1 | At least one step is not. |
| 2 | A usage error, as for every `by` command. |

Nothing prints a secret's value: a secret is reported by its variable's name or its file's path. Installing follows the `[harnesses]` policy exactly as `by harnesses install` does ([harness lifecycle](harness-lifecycle.md#installing-and-updating)): only your user file can allow it (a repository's `install = "auto"` is a configuration error), an unset policy asks on a terminal and refuses elsewhere, and each install is recorded in the harness log as `by up`. Harnesses are detected afresh, not from the minute's cache, so a login made since the last `by harnesses` counts. A harness running on a branch cannot run `by up`, and `--remote` is refused: a server brings up its own machine ([server](server.md)).

`--json` prints `{"steps": [{"name", "state", "detail", "todo"?, "action"?}], "state", "ready"}`, where `action` is `{"kind": "install", "plan": {...}}` (the `InstallPlan` `by harnesses install` shows) or `{"kind": "start_gateway"}`.

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

## From the SDKs

**Rust.** `Yard::open_configured(root)` opens the repository and gives the yard what the configuration files describe ([`branchyard::configure`](../crates/branchyard/src/configure.rs)): the connector gateway, the approvals, the model gateway, the knowledge settings, and the `[defaults]` every `yard.task(...)` then starts from, with the provisioning (`[secrets]`, `[mcp]`, `[network]`, grants, model access) filled in when the task runs. `yard.up_plan(&config, &options, &mut machine)` computes the plan, `yard.up_apply` does the planned steps, `yard.up` both; `branchyard::up::LocalMachine` is this machine, and a test passes a fake. The local machine cannot read the trust decisions `by recipe trust` and `by workspace trust` keep, so from the Rust SDK those steps say to check with `by`; `by up` reads them.

**Python.** `branchyard_client.Local(root).up(plan=False, yes=False)` runs `by up --json` and returns a `Plan` (`steps`, `todo`, `ready`, `step(name)`); `Local.doctor()` and `Local.harnesses()` likewise. `Client.local(root)` starts a `by serve` of the process's own for everything else ([SDKs](sdk.md#local-mode)).

## What is tested

- The engine (`crates/branchyard/src/up.rs`), over a fake machine: the needed harnesses from every table that names one; a missing harness planned, confirmed or left to the person by each policy; an install applied, verified at its pinned version and followed by a login step, or blocked with the command's output when nothing was installed, and never run when no tool can; secrets by variable and file, a backend's key included, nothing printing a value; the gateway started when Anvil is there, blocked when not, left alone when it listens, and blocked with the reason when the start fails; recipe and workspace trust in each state; a sandbox provider this build cannot run; the JSON shape.
- `by up` (`crates/branchyard-cli/tests/up.rs`, the built `by`, hermetic with a fake harness and a fake `npm`): a configured harness installed and logged in is ready and exits 0; `--plan` runs nothing, then the install runs once under `install = "auto"`, the login is yours until the fake logs in, and the install is in the harness log; without a terminal an unset policy leaves the install to the person and `--yes` cannot widen it, a repository file that allows installs is refused by the loader, and the text report lists what to do; secrets by name, never shown; a pinned gateway with no Anvil is blocked; inside a harness and with `--remote` it refuses.
- The Python `Local` and `Client.local` over a fake `by` (`tests/test_sdk_client.py`), and `Client.local` against the real `by serve` (`crates/branchyard-cli/tests/sdk_client.rs`).

## Not done

`by up` acts on this machine only; `by serve` has no route that brings a worker's machine up. It writes no secret: a key is yours to export or put in the file `[secrets]` names. A recipe's or the workspace's trust is still your decision made with `by recipe trust` and `by workspace trust`; `up` only says when it is needed. The Rust SDK's `LocalMachine` starts the gateway by running `by gateway start`, so a program without a `by` beside it gets that step blocked. There is no TypeScript client yet.
