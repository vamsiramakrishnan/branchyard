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
by run 'Fix the flaky test' --provider recipe:devbox   # the harness runs on the machine
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

## Running a branch on a recipe's machine

`--provider recipe:NAME` on `by run`, `fan`, `spawn`, `send`, `fork` and `reincarnate` runs each turn's harness on a machine the recipe makes:

```sh
by recipe trust devbox
by run 'Fix the flaky test' --provider recipe:devbox --harness codex
by run 'Port the parser' --provider recipe:devbox --keep-sandbox pause   # suspend between turns
by rm port-the-parser                                                     # destroys a kept machine
```

| Flag | Meaning |
|---|---|
| `--provider recipe:NAME` | The recipe, as `by recipe list` shows it |
| `--recipe-workdir PATH` | Where the worktree goes on the machine. Default `/tmp/branchyard/<branch>-<8 hex>/workspace`, unique to this repository's branch |
| `--recipe-home PATH` | The harness's `HOME` there. Default the same directory's `home`. It is replaced on each resumed turn: give a directory Branchyard owns |
| `--pass-env NAME,...` | Variables copied by name from your environment, such as an API key |
| `--keep-sandbox pause`, `--max-paused N` | Suspend the machine when a turn ends and resume it for the next, through the recipe's `suspend` and `resume`; refused when the recipe lacks either |

`[defaults] provider = "recipe"` with `recipe = "NAME"` in `branchyard.toml` chooses one for every new branch, as `provider = "microsandbox"` does.

What happens in a turn:

1. **Trust.** The recipe must be trusted as it is now, or be your own (user file). Otherwise the command stops before anything is made, and says to run `by recipe trust NAME`; a changed recipe asks again. A harness on a branch cannot use one. The branch keeps the recipe's commands as they were trusted: later turns, recovery and `by rm` run those, not what the configuration says by then.
2. **The machine.** `create` runs (or `resume`, for a kept machine), and its record is kept in `.branchyard/recipes/`, beside the turn's journaled `sandbox` step, so another process can reach it.
3. **The worktree and home go in.** The transfer the Substrate provider uses ([substrate](substrate.md)): the worktree's commit and its files as they are, tracked and untracked but not ignored, recreated in a new repository at the workdir; the branch's private home copied to the home directory. Over ssh they cross as `cat` and `tar` streams through execs. A workdir that holds a repository Branchyard did not put there is refused, never cleared.
4. **The harness runs** in the workdir, over ssh (or the exec command), with `HOME`, the result's `env` and the passed variables added to the machine's own login environment. Provisioning works as in any sandbox: secrets, model, effort and MCP servers are written into the private home, which travels; connectors need `[connectors] sandbox_gateway`, an address the machine can reach.
5. **The work comes back.** When the turn ends, the machine's files are applied to the worktree (its commits come back as commits), the home comes back, and the machine is destroyed through `destroy`, or suspended when kept. The candidate is snapshotted as for a local harness, so it shows in `by diff` and merges the same way.

**Recovery.** If `by` dies mid-turn, the machine outlives it, and so does the harness: ssh does not stop remote processes. When the branch is next opened, recovery stops what still runs there, brings the work back (only if the worktree still holds exactly what was sent), and destroys the machine; a kept machine stays for the next turn. `by rm` destroys a kept machine.

**Where it runs.** On the machine that has the repository, as the person who trusted the recipe. `by --remote … --provider recipe:NAME` is refused before anything is sent, and a server refuses a recipe in a request (`403 provider_not_allowed`), whatever `allow_providers` says: a request would carry commands the server would run as itself.

**What the machine needs.** `sh`, `git`, `tar`, `cat`, `mkdir` and `ps`, the harness on its `PATH` (or named by `--command`), and an ssh login that needs no prompt. A recipe that hands out a machine shared by several branches (a lab box) works, since each branch has its own workdir; a sandbox there is not isolated from the others.

**Not supported:** checkpoints with a machine snapshot (`--sandbox-snapshots`), forking a branch from another's machine, and delegation from a harness on the machine. Admission refuses a task that requires them, as for any provider that does not declare them.

## Tested

- `crates/branchyard-recipe/tests/provider.rs`: a recipe whose scripts make a local "VM" (a directory) reached through `tests/fixtures/fake-ssh`, which runs each "remote" command here in a session of its own, as sshd would. The provider passes the whole sandbox conformance suite in its mount-less mode (lifecycle, exit status, a missing program, stdio, env and cwd, mounts refused, kill reaching descendants, teardown naming survivors, drop, stop); capabilities follow the recipe and admission holds to them; create, exec with the result's env over `ssh -p 2222 -l dev`, suspend (payload checked), exec refused while paused, resume's result replacing create's, destroy (payload checked); specs it cannot honor and failing or malformed scripts refused; a state directory shared by two provider values. Unit tests cover result parsing (each refusal), Orca's first-token rule, `destroy = "none"`, the doctor's checks, the script variables, stdin and timeout, and the transports' command lines; `tests/upstream.rs` checks the vendored Orca sources still have the shape the port follows.
- `crates/branchyard/tests/recipe.rs`: turns on a recipe's machine through the engine, with the fake ACP agent behind `fake-ssh`: a turn's file in the candidate and merged, the machine created and destroyed through the recipe; `HOME`, the result's `env` and a passed variable reaching the harness; a provisioned home (a secret, 0600) going in and coming back; a kept machine suspended, resumed by the next turn (the worktree sent again) and destroyed by removal; a recipe without `suspend` refused for `keep`; and recovery after the engine is killed mid-turn: the orphaned harness stopped, its file brought back into the worktree and the candidate, the machine destroyed, the staging directory gone.
- `crates/branchyard-cli/tests/recipes.rs`: the built `by`: `by run --provider recipe:devbox` refused while untrusted (nothing created), then run with the fake ACP agent, the file it wrote on the machine in `by diff`, the machine destroyed; a kept machine left suspended and destroyed by `by rm`; `--remote` refused before connecting; `[defaults] provider = "recipe"` choosing it (with the default workdir); a changed recipe refused again; `[defaults] recipe` alone refused by `by config validate`. Also an untrusted recipe refused without a terminal with nothing created, a harness unable to trust, `trust` (a 0600 trust file), `show --json`, a full `check` (doctor, create, exec, suspend, resume, exec, destroy), `--no-smoke --json`, a changed command refused again, `untrust`; a user-file recipe needing no trust (with its absolute-path and `destroy = "none"` warnings), a failing doctor failing the check and skipping the smoke, and an invalid recipe name refused by `by config validate`.

**Not tested:** a real cloud VM, a real `sshd`, the `exec` transport against a real container runtime, or a real harness on a recipe's machine. The fake ssh runs "remote" commands on this host, so the transfer's `tar` and `git` are this host's on both sides.
