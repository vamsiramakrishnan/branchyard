# Harness lifecycle

Branchyard manages harnesses like dependencies. Every machine that runs work can say which harness CLIs it has, at which version, and whether each is logged in. `by harnesses` installs or updates one when your policy allows, walks you through logging in once, and the router sends work only to where a harness can run, here or on a worker. This is the *harness lifecycle* track of [Wave 5](roadmap.md#wave-5).

> **Status.** Implemented and tested hermetically with fake harness binaries, a fake `npm`, a fake `ssh` and a recipe that makes a local "VM". No real harness was installed, updated or logged in to by these tests. See [what is tested](#what-is-tested).

```sh
by harnesses                                   # this machine: version, login, quota
by harnesses --json
by harnesses --on ssh://me@build.example       # another machine, over ssh
by harnesses --on recipe:devbox                # a fresh machine from a recipe
by harnesses --remote https://by.internal      # a server's live workers
by harnesses install codex                     # the catalog's command, under [harnesses]
by harnesses update codex --version 0.200.0 --yes
by harnesses login codex                       # its own login flow, once
printf %s "$KEY" | by harnesses login claude-code --api-key
by harnesses log                               # every install, update and login here
by harnesses --profiles                        # the profiles Branchyard drives
by harnesses --all                             # every CLI in the catalog
```

## What `by harnesses` shows

```text
$ by harnesses
HARNESS      VERSION  LOGIN                 QUOTA            PATH
claude-code  2.1.283  logged in (likely)    5h 50% · wk -    /home/me/.local/bin/claude
codex        0.157.1  logged in (verified)  5h 93% · wk 40%  /usr/local/bin/codex
goose        ?        unknown               -                /usr/local/bin/goose
  claude-code: ~/.claude/.credentials.json exists
  codex: codex login status: Logged in using ChatGPT
  goose: no version: its version command printed no version
  goose: Branchyard knows no way to tell for this harness

3 of the 45 harnesses with a known executable are installed on box. ...
```

Only installed harnesses are listed. `--json` prints the `Inventory`: `host`, `os`, `detected_at_ms`, `checked` (the IDs looked for), `harnesses` (each `id`, `path`, `on_path`, `version` or `version_note`, `login` with `state`, `evidence` and `detail`, and `quota`), and `tools` (the install programs found: `npm`, `bun`, `pipx`, `uv`, `brew`, `curl`, `bash`, `sh`).

A harness **can run** on a machine when it is installed, found on `PATH`, not logged out, and its login has not reached a usage limit. Otherwise the table says why, and what to do.

## How detection works

Detection is one POSIX `sh` script, generated from the [harness catalog](../catalog/harnesses.toml) and run on the machine: with `/bin/sh -s` here, over `ssh`, or through a recipe's transport. Nothing needs installing there. For each harness with a known executable it:

1. **Finds the executable**: the first of its names on `PATH`, else in the usual install directories (`~/.local/bin`, `~/.npm-global/bin`, `~/.bun/bin`, `~/.claude/local`, `~/.cargo/bin`, `/usr/local/bin`, `/opt/homebrew/bin`). One found only there is marked "not on PATH": a harness started by name would not find it. `BRANCHYARD_HARNESS_DIRS` (colon-separated, `$HOME/` expanded on the machine) replaces the list.
2. **Runs its version command** (`--version`; Jules's `version`) with no input, killed after 5 seconds (`BRANCHYARD_HARNESS_TIMEOUT` sets the seconds). The first version-shaped word of its output is the version (`codex-cli 0.157.1` is `0.157.1`).
3. **Reads its login without reading a secret.** See below.
4. **Looks for install programs**, for [installing](#installing-and-updating).

Only names, paths, versions and the first lines of a status command's output come back. A key variable is tested with `[ -n "${NAME:-}" ]` and a credential file with `[ -e ]`; neither is read.

### Login states

| Evidence | When |
|---|---|
| `verified` | The harness's own status command said so. Upstream records one only for Codex: emdash runs `codex login status` and reads it with "not authenticated", "not logged in", "not signed in", "login required" (logged out) and "authenticated", "logged in", "signed in" (logged in). |
| `likely` | A key variable is set (logged in), a credential file exists (logged in), or none of the harness's known credential files and variables exist (logged out). |
| `unknown` | Branchyard knows no credential file or status command for the harness, and no key variable is set. |

A key variable wins over a status command that says logged out: Codex runs with `OPENAI_API_KEY` whatever its login says. Where the files are comes from Scion's harness configurations (`auth.types.*.required_files`, `autodetect.env`): `~/.claude/.credentials.json` (or `$CLAUDE_CONFIG_DIR`), `~/.codex/auth.json` (or `$CODEX_HOME`), `~/.gemini/oauth_creds.json`, `~/.local/share/opencode/auth.json`, `~/.gemini/antigravity-cli/antigravity-oauth-token`, `~/.copilot/config.json` and `~/.grok/auth.json`. Key variables are the catalog's `auth_env` and Scion's (`CLAUDE_CODE_OAUTH_TOKEN`, `CODEX_API_KEY`, `GEMINI_API_KEY`, ...). On macOS, Claude Code keeps its login in the keychain, so a missing file there is `unknown`, not logged out.

Claude Code has `claude auth status`, and emdash calls a status check for it, but that check's source (`claude/auth.ts`) is not in the vendored files, so Branchyard does not run it: a Claude Code login is `likely` at best.

### Quota

On this machine, Claude Code's and Codex's default logins carry the usage `by usage` reads ([usage](usage.md)): the percent of the 5-hour and weekly windows, and whether a limit was reached. A login at its limit cannot run. Other machines show no quota unless they advertise it: `by serve` and `by worker` do; a bare `branchyard-server` does not.

### Caching

This machine's inventory is kept for a minute in `harness-inventory.json` beside your user configuration, keyed by `PATH`, `HOME` and the install directories. `--refresh` detects again; an install, update or login here clears it. Other machines are detected each time.

## Other machines

- **`--on ssh://[user@]host[:port]`** runs the script with `ssh -T -o BatchMode=yes host sh -s` (`BRANCHYARD_SSH` names the program). A path after the host is allowed and ignored.
- **`--on recipe:NAME`** makes a fresh machine from the repository's [recipe](recipes.md), runs the script through its transport, and destroys it. A repository's recipe runs only once you trust it, as `by recipe` requires.
- **`--remote URL`** shows the server's live workers that serve repositories you can see, the answering server first, each with the inventory it advertised (`GET /v1/inventory`). `--on` and `--remote` together are refused.

## Installing and updating

```sh
by harnesses install codex                # asks on a terminal
by harnesses install codex --yes
by harnesses update codex --version 0.200.0 --yes --on ssh://me@build.example
```

`install` and `update` run the catalog's install command for the harness: the first one whose programs the machine has (`npm install -g @openai/codex`, else `curl ... | sh`, else `brew install --cask codex`). `install` of a harness already there does nothing; `update` runs it again.

**Pins.** A package manager's command (`npm`, `bun`, `pnpm`) is pinned to the version the harness's default profile was checked against (`codex-cli 0.157.1` gives `@openai/codex@0.157.1`), or to `--version`. A piped installer takes no version, so it runs unpinned and `by` says so; `--version` with one is refused.

**Verification.** After the command, detection runs again for that harness. The install is verified when the command exited 0, the harness is found, and its version is the pinned one when pinned. Otherwise `by` fails and shows the end of the output.

**Policy.** `[harnesses]` in your user configuration:

```toml
[harnesses]
install = "ask"      # "never", "ask" or "auto"
allow = ["codex", "claude-code"]   # when set, only these
```

| `install` | `by harnesses install` and `update` | The router |
|---|---|---|
| `never` | Refused | Never installs |
| `ask` | Asks on a terminal; `--yes` answers in advance | Never installs |
| `auto` | Runs | Installs a missing candidate on demand |
| unset | `ask` on a terminal, `never` without one (a script, a server, a worker) | Never installs |

**Trust.** Installing runs software on your machine, so only your user file can allow it. A repository's `branchyard.toml` may set `install = "never"`; `install = "ask"`, `"auto"` or `allow` there is a configuration error, as `by config validate` reports. Install commands come only from the catalog built into `by`, never from a repository. A harness running on a branch (`BRANCHYARD_BRANCH` set) can never install or log in.

**Where.** Here, or `--on ssh://...` (over `ssh`, as above). A recipe's machines are made fresh each time, so `--on recipe:NAME` is refused with the command to put in its create script. A server's workers never install; `--remote` is refused with how to install on a worker's machine.

## Logging in

```sh
by harnesses login codex                        # runs `codex login --device-auth`
by harnesses login codex --on ssh://me@build    # the same over ssh -t
printf %s "$KEY" | by harnesses login claude-code --api-key
```

`login` runs the harness's own login command from the catalog (`claude auth login`, `codex login --device-auth`, `opencode auth login`) with your terminal, so a URL or device code it prints reaches you directly. Over ssh it uses `ssh -t`. A harness found off `PATH` is run by its path. Afterwards detection runs again and `by` prints the login state.

Without a terminal (a script, or `--remote`, since a worker has none) nothing runs: `by` says what to run and where (`run \`codex login --device-auth\` there, in a terminal, as the worker's user`). A recipe's machine gets its login from the recipe's create script or its environment.

**An API key.** `--api-key` reads a key from standard input (hidden on a terminal) for the harness's first key variable, writes it to `secrets/<VARIABLE>` beside your user configuration (directory 0700, file 0600), and names it in your user file's `[secrets]` (`ANTHROPIC_API_KEY = "@…/secrets/ANTHROPIC_API_KEY"`). That is the existing secrets path: branches with a private home (`--isolated`, a sandbox) are given it ([setup](setup.md)). The key is never printed, never on a command line, never in the log, and refused when the file would land inside the repository. A harness you run directly reads its own environment, so export the variable from that file yourself.

## The log

Every install, update and login attempt is a line in `harness-events.jsonl` beside your user configuration (0600): when, the machine (`local`, `ssh://host`), the harness, the action, who asked (`by harnesses` or `router`), the command, the outcome (`verified`, `failed`, `refused`, `ran`, `reported`, `stored_key`), the versions before and after, and why. `by harnesses log [--json]` prints it.

## Workers

`by serve`, `by worker` and `branchyard-server` detect the harnesses on their machine when they start and every five minutes, on a thread of their own, and send the inventory with each beat to the `workers` table (`by_workers` on PostgreSQL, column `inventory`, added by the usual migrations). From it each worker derives a label `harness:<id>` for every harness that can run there, so work can require one (`--require-label harness:codex`) with nothing configured ([worker labels](server.md#worker-labels)). Labels take `:` for this.

**Steering.** A task that runs harnesses by name (no `command` of its own or from `harness_commands`, and no sandbox provider) gets `harness:<id>` added to its required labels when some live worker serving the repository advertises an inventory in which all of them can run. When none does, nothing is added: work is steered toward a worker that has its harnesses, never held back because no worker advertises them. `by --remote ... run --harness codex` therefore lands on a worker that has Codex logged in.

`"inventory": false` in the server configuration, or `--no-inventory`, turns detection and advertising off. A server that triggers routed tasks ([triggers](triggers.md)) consults the live workers' inventories the same way.

## The router

A routed `by run` or `by fan` ([fleet](fleet.md)) asks this machine's inventory about each candidate that runs a harness by name (no `command`, no sandbox), after its profile checks and before the `PATH` check. A candidate that is not installed, is logged out, is off `PATH` or is at its usage limit is excluded with that reason, like an unavailable one:

```text
by:   not codex: codex is verified logged out (codex login status: Not logged in); run by harnesses login codex
by:   not qwen-code: qwen-code is not installed on this machine; install it with `by harnesses install qwen-code` (the router installs on demand only with [harnesses] install = "auto")
```

With `install = "auto"` (and `allow` naming it, when set), a missing candidate is installed first, verified, and recorded in the log by `router`; then the route goes on. `by fleet route` never installs: it shows such a candidate excluded with "a routed run would install it first". A routed fan picks each attempt from the same eligible candidates, so split work lands where its harnesses run.

## Not done yet

- A delegated child, graph node or rig seat that names its harness is not checked against the inventory before it starts here; on a server, only tasks (one branch or a fan) are steered, not sends, forks, spawns or graph nodes.
- Workers never install on demand, whatever their configuration; install on a worker's machine with `--on ssh://` or in its image.
- Detection inside a sandbox image (Microsandbox, Substrate) is not wired: `--on` takes ssh and recipes only.
- Harness catalogs refreshed from upstream releases belong to the ambient registry track; pins come from the profiles' checked versions.

## SDK

| What | Where |
|---|---|
| Types | `branchyard::inventory::{Inventory, HarnessState, Login, LoginState, Evidence, Quota}` |
| Detection | `detect_local(&DetectOptions)`, `detect_with(options, stdin, make_command)`, `script`, `parse` |
| Cache | `InventoryCache::{new, get, put, local, clear}`, `DEFAULT_TTL` |
| Installing | `InstallPolicy`, `InstallMode`, `Permission`, `plan`, `install`, `run_local`, `checked_version` |
| Log | `HarnessLog`, `HarnessEvent` |
| Routing | `HarnessGate`, `LocalGate` (with `preview`, `with_runner`, `with_log`), `RouteOptions::harnesses` |
| Remote | `Client::inventory()` (`GET /v1/inventory`, `InventoryReport`, `WorkerInventory`) |

## What is tested

- `crates/branchyard/tests/inventory.rs`: detection on a temporary `PATH` of fake harnesses: versions read, a status command saying logged out (verified), a key variable seen by name (its value in no output), unknown logins, a missing harness, a version command that hangs killed by the timeout, each command run once; `only`; the cache's key and TTL; an install through a fake `npm`, pinned, verified by detecting again, and a pin not delivered or a failing command not verified; the log; the router excluding a logged-out and a missing candidate with reasons, a candidate with a command left to the `PATH` check, the allowlist; a gate installing on demand under `auto` and recording it, a preview that runs nothing, `ask` never installing.
- `crates/branchyard-server`: the store conformance (`check_labels`, on SQLite and, in `tests/postgres.rs`, PostgreSQL) for a beat carrying an inventory, derived labels, steering and `WorkersGate`; an older SQLite `workers` table migrated; `tests/inventory.rs` over HTTP: work requiring `harness:codex` waiting and saying why, then claimed by a worker whose inventory has Codex with no label configured, `GET /v1/inventory`, a task naming Codex steered and one naming a harness no worker has not held back; on PostgreSQL, two servers on one database, the second a worker with Codex.
- `crates/branchyard-cli/tests/harnesses.rs`, with the built `by`: the table and JSON (verified, likely, unknown, a timeout, off `PATH`), the cache and `--refresh`, `--on ssh://` and `--on recipe:` through `fake-ssh` (the recipe's machine destroyed after, an untrusted recipe refused), installs under `never`, `ask`, `--yes`, `auto` and an allowlist, a repository trying to allow installs refused, a pin and a failed verification, the log; install and login over ssh (`ssh -t`); a login through a pty (the device code passed through, verified after), without a terminal (reported), an API key stored 0600 and named in `[secrets]`; the router excluding a logged-out and a missing candidate, previewing, and installing one on demand before running it on the fake ACP agent; `by serve` advertising its inventory to `by harnesses --remote`, login reported for a worker, install refused.

**Not tested:** a real harness installed, updated or logged in to; a real `npm`, `curl` installer or Homebrew; a real `sshd`; macOS's keychain case; quota on a real login; a trigger's routed task consulting the workers' inventories (`WorkersGate` is tested in the store conformance, not through a firing trigger). The status phrases are emdash's, not observed from a real `codex login status`.

## Code

| Where | What |
|---|---|
| [`inventory.rs`](../crates/branchyard/src/inventory.rs) | Types, the detection script and its parser, login checks and their sources, the cache, policy, plans, pins, installs, the log, `LocalGate` |
| [`fleet.rs`](../crates/branchyard/src/fleet.rs) | `RouteOptions::harnesses`, consulted in `availability` |
| [`store.rs`](../crates/branchyard-server/src/store.rs), [`ops.rs`](../crates/branchyard-server/src/ops.rs), [`api.rs`](../crates/branchyard-server/src/api.rs) | The workers' `inventory` column, advertising and derived labels, steering at admission, `WorkersGate`, `GET /v1/inventory` |
| [`harness_cmd.rs`](../crates/branchyard-cli/src/harness_cmd.rs) | `by harnesses` and its actions, machines (`--on`), quota, the API-key store, the router's gate |
| [`config.rs`](../crates/branchyard-setup/src/config.rs) | `[harnesses]` and the rule that a repository may only forbid installs |
