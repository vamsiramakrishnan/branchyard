# Environment recipes

A recipe is a repository's own scripts for making a machine to work on: a cloud VM, a container, a lab box. `[recipes.NAME]` in `branchyard.toml` names commands that create, suspend, resume and destroy it, each printing how to reach it; Branchyard runs harness commands there over `ssh` (or the recipe's own exec command) through the **recipe sandbox provider**. The contract is Orca's environment recipes (`environmentRecipes` in `orca.yaml`), ported from its source ([ports](vendoring.md#emdash-and-orca-ports-as-data-and-as-translations)).

```toml
[recipes.devbox]
description = "a disposable VM in our cloud project"
create  = "./scripts/vm/create.sh"
suspend = "./scripts/vm/suspend.sh"
resume  = "./scripts/vm/resume.sh"
destroy = "./scripts/vm/destroy.sh"     # or "none" when it is cleaned up elsewhere
doctor  = "./scripts/vm/doctor.sh"      # optional: is the CLI installed and logged in?
timeout_seconds = 900                   # per script (the default)
```

```sh
by recipe list                  # name, trust, what it can do
by recipe show devbox           # its commands, digest and trust
by recipe trust devbox          # after reading them
by recipe check devbox          # the doctor, then create, exec, suspend, resume, exec, destroy
by recipe check devbox --no-smoke --json
```

## The contract

Each command runs with `sh -c` in the repository root, in a process group of its own (killed whole after `timeout_seconds`), with these variables added to yours:

| Variable | Value |
|---|---|
| `BRANCHYARD_RECIPE` | The recipe's name |
| `BRANCHYARD_RECIPE_MODE` | `create`, `suspend`, `resume`, `destroy` or `doctor` |
| `BRANCHYARD_RECIPE_INSTANCE` | The machine's name (the sandbox name), unique per machine |
| `BRANCHYARD_ROOT` | The repository root |
| `BRANCHYARD_RECIPE_RESULT_SCHEMA_VERSION` | `1` |

`create` and `resume` print **one JSON object** on stdout (the last 1 MiB is kept; logs go to stderr), and a machine left running must not keep the script's stdout or stderr open:

```json
{
  "schemaVersion": 1,
  "connection": {
    "type": "ssh",
    "target": {"host": "10.0.0.7", "port": 22, "username": "dev",
               "identityFile": "~/.ssh/devbox", "identitiesOnly": true},
    "projectRoot": "/home/dev/app"
  },
  "env": {"CI": "1"},
  "userData": {"vmId": "i-0abc"}
}
```

- `connection.type` is `ssh` (Orca's ssh target: `host`, `port` (default 22), `username`, optional `label`, `configHost` (a `Host` of your ssh configuration, used instead of the rest), `identityFile` and `identitiesOnly`) or `exec`, with an `argv` prefix such as `["docker", "exec", "-i", "vm-1"]` that is followed by `sh -c SCRIPT`.
- `projectRoot` is an absolute path on the machine: where execs run unless they name another directory.
- `env` is given to every exec there. `userData` is kept and handed back.
- Unknown fields, another `schemaVersion`, a relative `projectRoot` or an option-shaped host are refused, as Orca refuses them. Orca's `orca-server` pairing connection and its `provisioned-root` checkout mode are not ported: Branchyard does not run an Orca server on the machine.

`suspend`, `resume` and `destroy` receive Orca's lifecycle payload on stdin, one line: `{"schemaVersion": 1, "mode": "suspend", "recipe": "devbox", "instance": "…", "recipeResult": {…}}`, the result `create` (or the last `resume`) printed. A non-zero exit is a failure, reported with the end of the script's stderr; a destroy that fails keeps the machine's record so it can be retried.

`doctor` exits 0 when this machine can run the recipe. `by recipe check` also runs Orca's static checks: each command should be a repository-relative script (`./…`) that exists and is executable; a missing `destroy` is a warning unless it is `"none"`; and `suspend` without `resume` (or the reverse) is a warning, since it strands a machine asleep.

## Trust

A repository's recipe is code you have not read, so none of its commands runs until you trust it, exactly as for [`[workspace]` scripts](workspace.md#trust): the decision is in the same per-user trust file (`BRANCHYARD_TRUST_FILE`, mode 0600), keyed by the repository's canonical root and the recipe's name, for the SHA-256 of its commands and timeout; any change asks again. On a terminal `by recipe check` shows the commands and asks once; without one it refuses and points to `by recipe trust NAME`. A harness on a branch (`BRANCHYARD_BRANCH` set) can never trust or run one. `[recipes.NAME]` in your own user file (`~/.config/branchyard/config.toml`) needs no trust and replaces the repository's recipe of the same name.

## The recipe sandbox provider

`branchyard_recipe::RecipeProvider` implements `SandboxProvider` ([providers](providers.md)) over one recipe:

| Operation | What it does |
|---|---|
| `ensure` | Runs `create` once per sandbox name and keeps the result. A spec with mounts, an image or resource limits is refused (`Invalid`): the machine is whatever the script makes, and cannot see this host's directories |
| `exec` | Runs the argument vector, unchanged, in `projectRoot` (or the spec's directory) with the result's `env` and the spec's, over the transport. A small `sh` wrapper on the machine makes the command a process group leader (with `setsid` when the transport did not), records the group under `~/.branchyard/recipe-exec/`, and reports a missing program before starting, so `exec` fails with an I/O error as the contract requires |
| `kill`, `teardown`, `stop` | Signal that recorded group on the machine (closing an ssh connection leaves remote processes running); `teardown` names what it found |
| `pause`, `resume` | `suspend` and `resume`; declared (`Capabilities::pause`) only when the recipe has both |
| `destroy` | Stops what runs, runs `destroy` with the payload, forgets the machine |

It declares `exec`, and `pause` when it can, and nothing else: no checkpoint, restore, branch, share, live branch or ingress, so [admission](sandbox-snapshots.md) refuses a task that requires any of them rather than weakening it. With a state directory (`with_state_dir`), each machine's result is kept in a 0600 file so another process can exec in, pause, resume or destroy it by name. ssh runs with `BatchMode=yes`: a recipe's machine must be reachable without a prompt.

**Not yet:** `by run --provider recipe:NAME`. The engine's providers ship a branch's worktree to the sandbox by mounting it (Microsandbox) or by transfer through the bridge (Substrate); a recipe's machine needs one of those, or a checkout made by the recipe, and neither is wired yet. Today the provider is a library with a CLI check, ready for the engine.

## Tested

- `crates/branchyard-recipe/tests/provider.rs`: a recipe whose scripts make a local "VM" (a directory) reached through `tests/fixtures/fake-ssh`, which runs each "remote" command here in a session of its own, as sshd would. The provider passes the whole sandbox conformance suite in its mount-less mode (lifecycle, exit status, a missing program, stdio, env and cwd, mounts refused, kill reaching descendants, teardown naming survivors, drop, stop); capabilities follow the recipe and admission holds to them; create, exec with the result's env over `ssh -p 2222 -l dev`, suspend (payload checked), exec refused while paused, resume's result replacing create's, destroy (payload checked); specs it cannot honor and failing or malformed scripts refused; a state directory shared by two provider values. Unit tests cover result parsing (each refusal), Orca's first-token rule, `destroy = "none"`, the doctor's checks, the script variables, stdin and timeout, and the transports' command lines; `tests/upstream.rs` checks the vendored Orca sources still have the shape the port follows.
- `crates/branchyard-cli/tests/recipes.rs`: the built `by`: an untrusted recipe refused without a terminal with nothing created, a harness unable to trust, `trust` (a 0600 trust file), `show --json`, a full `check` (doctor, create, exec, suspend, resume, exec, destroy), `--no-smoke --json`, a changed command refused again, `untrust`; a user-file recipe needing no trust (with its absolute-path and `destroy = "none"` warnings), a failing doctor failing the check and skipping the smoke, and an invalid recipe name refused by `by config validate`.

**Not tested:** a real cloud VM, a real `sshd`, or the `exec` transport against a real container runtime.
