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
by harnesses --profiles                        # the profiles Branchyard drives (no repository needed)
by harnesses --all                             # every CLI in the catalog
```

## What `by harnesses` shows

```text
$ by harnesses
HARNESS      VERSION        LOGIN                     LIVE RUN     QUOTA            PATH
claude-code  2.1.310        logged in                 live 9/9     5h 50% · wk -    /home/me/.local/bin/claude
codex        0.157.1        key set (CODEX_API_KEY)   live 9/9     5h 93% · wk 40%  /usr/local/bin/codex
gemini-cli   slow to start  login not verified       live 8/9     -                /usr/local/bin/gemini
goose        ?              unknown                   no live run  -                /usr/local/bin/goose
  claude-code: claude auth status: logged in (oauth_token)
  codex: codex login status: Logged in using ChatGPT; CODEX_API_KEY is set, and Codex uses it instead of the login ...
  gemini-cli: version: slow to start (took >10 s); `by harnesses --refresh` asks again, and BRANCHYARD_HARNESS_TIMEOUT (seconds) sets how long to wait
  gemini-cli: login not verified: no ~/.gemini/oauth_creds.json and no GEMINI_API_KEY or GOOGLE_API_KEY; run `by harnesses login gemini-cli`, or set GEMINI_API_KEY or GOOGLE_API_KEY
  gemini-cli: installed 0.65.0 · live-tested 0.63.0
  goose: no version: its version command printed no version
  goose: Branchyard cannot read goose's login: the catalog records no status command, credential file or key variable for it; sign in inside goose itself

3 of the 45 harnesses with a known executable are installed on this machine. ...
```

Only installed harnesses are listed. `--json` prints the `Inventory`: `host`, `os`, `detected_at_ms`, `checked` (the IDs looked for), `harnesses` (each `id`, `path`, `on_path`, `version` or `version_note`, `login` with `state`, `evidence`, `detail` and, when the login rests on a key, `key` (its variable's name) and `stored`, and `quota`), and `tools` (the install programs found: `npm`, `bun`, `pipx`, `uv`, `brew`, `curl`, `bash`, `sh`). The table's words for a login are in [login states](#login-states). Beside the table, `LIVE RUN` says what is known of the harness driven: `live 9/9` is a recorded session of the harness itself, `no live run` is none. The line `installed X · live-tested Y` appears when the version installed is not the version the live run was on: a run says nothing about another version (the scenarios and the version are in [qualification](qualification/README.md)).

A harness **can run** on a machine when it is installed, found on `PATH`, not logged out, and its login has not reached a usage limit. Otherwise the table says why, and what to do.

## How detection works

Detection is one POSIX `sh` script, generated from the [harness catalog](../catalog/harnesses.toml) and run on the machine: with `/bin/sh -s` here, over `ssh`, or through a recipe's transport. Nothing needs installing there. For each harness with a known executable it:

1. **Finds the executable**: the first of its names on `PATH`, else in the usual install directories (`~/.local/bin`, `~/.npm-global/bin`, `~/.bun/bin`, `~/.claude/local`, `~/.cargo/bin`, `/usr/local/bin`, `/opt/homebrew/bin`). One found only there is marked "not on PATH": a harness started by name would not find it. `BRANCHYARD_HARNESS_DIRS` (colon-separated, `$HOME/` expanded on the machine) replaces the list.
2. **Runs its version command** (`--version`; Jules's `version`) with no input, killed after 10 seconds (`BRANCHYARD_HARNESS_TIMEOUT` sets the seconds): a Node harness takes three to six seconds to start cold. Every installed harness is asked at once, each in a background subshell of the script, so a run takes as long as the slowest harness and not the sum (nine real CLIs took 42.7 seconds one after the other with a 5-second bound and 4.4 seconds together; see [what is tested](#what-is-tested)). The first version-shaped word of its output is the version (`codex-cli 0.157.1` is `0.157.1`). One that still has not answered is `slow to start (took >10 s)`, with the next step, not a bare `?`; one that answered the last time keeps that version, noted as such.
3. **Reads its login without reading a secret.** See below.
4. **Looks for install programs**, for [installing](#installing-and-updating).

Only names, paths, versions and the first lines of a status command's output come back; the login detail shows the last of them, since a CLI may print a warning before its answer (Codex names its home directory). A key variable is tested with `[ -n "${NAME:-}" ]` and a credential file with `[ -e ]`; neither is read. Three harnesses also have a configuration file searched for one setting (`grep -q`, below); nothing in the file is printed.

### Login states

| Evidence | When |
|---|---|
| `verified` | The harness's own status command said so. Upstream records one only for Codex: emdash runs `codex login status` and reads it with "not authenticated", "not logged in", "not signed in", "login required" (logged out) and "authenticated", "logged in", "signed in" (logged in). Branchyard also asks Claude Code `claude auth status` (JSON with `loggedIn`; run on 2.1.310), which sees a login the credential file does not, such as a token the host provides. |
| `likely` | A key variable is set (logged in), a credential file exists (logged in), or none of the harness's known credential files and variables exist (logged out). |
| `unknown` | Branchyard knows no credential file or status command for the harness, and no key variable is set. |

`by harnesses`, `by doctor` and `by up` use the same words, and each says only what is known:

| Shown | Means |
|---|---|
| `logged in` | The harness's own status command said so (`verified`). |
| `key set (GEMINI_API_KEY)` | A key variable is in the environment, named; the key is not checked. |
| `key stored (DEEPSEEK_API_KEY)` | `by harnesses login ID --api-key` stored the key in a file named in `[secrets]`, and the file exists. Branches with a private home get it; export it for your own sessions. |
| `credentials found` | A credential file exists. It is not read, so this is not a verified login. |
| `logged out` | The harness's own status command said so. |
| `login not verified` | None of the credential files and key variables Branchyard knows of is there. That is not the same as logged out: a login Branchyard does not look for may work. The detail says what was missing and the command. |
| `unknown` | Branchyard has nothing to go on. The detail says what would tell: a key variable it recognises, or that the catalog records no status command, credential file or key variable (sign in inside the harness itself, then `by harnesses --refresh`). |

A harness "cannot run" only for something that stops it: it is off `PATH`, it is at its usage limit, or its own command said it is logged out. A login that is only not verified is never called that.

The command to get a harness logged in is one wording for the listing, `by doctor`, `by up` and the router: `run \`by harnesses login ID\`` when the catalog records a login command, `run \`by harnesses login ID --api-key\`` when it records only a key variable, and, for a harness with neither, to sign in inside it and `by harnesses --refresh`; the key variables that would do instead follow.

A key variable wins over a status command that says logged out: Codex runs with `OPENAI_API_KEY` whatever its login says. It wins over one that says logged in, too: with `CODEX_API_KEY` set, Codex sends its requests with that key whatever `codex login status` says, so a `verified` Codex login's detail names the variable when it is set. A key without quota answers `Quota exceeded. Check your plan and billing details.` beside a valid ChatGPT login; unset the variable, or run the branch isolated (which drops `CODEX*` variables and takes the login as `CODEX_AUTH`), to use the login. Where the files are comes from Scion's harness configurations (`auth.types.*.required_files`, `autodetect.env`): `~/.claude/.credentials.json` (or `$CLAUDE_CONFIG_DIR`), `~/.codex/auth.json` (or `$CODEX_HOME`), `~/.gemini/oauth_creds.json`, `~/.local/share/opencode/auth.json`, `~/.gemini/antigravity-cli/antigravity-oauth-token`, `~/.copilot/config.json` and `~/.grok/auth.json`. Key variables are the catalog's `auth_env` and Scion's (`CLAUDE_CODE_OAUTH_TOKEN`, `CODEX_API_KEY`, `GEMINI_API_KEY`, ...). On macOS, Claude Code keeps its login in the keychain, so a missing file there is `unknown`, not logged out.

Four harnesses need more than a file's presence. The script searches their configuration for one setting and prints a fact, never a line of the file:

- **Antigravity** (`agy` 1.3.3) reads a key from `GEMINI_API_KEY` alone (`GOOGLE_API_KEY` has no effect), and only when `~/.gemini/antigravity-cli/settings.json` has `"modelProvider": "gemini"`. With both, it is `likely` logged in (`GEMINI_API_KEY is set and settings.json selects the gemini provider`). With the key and no such setting `agy` would ask for a Google sign-in, but Branchyard sets the provider in isolated runs ([provisioning](provisioning.md)), so it stays `likely` logged in and the detail says `agy will ask for a Google sign-in; Branchyard sets it in isolated runs`; `by harnesses login antigravity` writes the setting for your own sessions ([logging in](#logging-in)). A Google sign-in (`~/.gemini/antigravity-cli/antigravity-oauth-token`) or `AGY_TOKEN` counts whatever the key and the setting say, and with none of them it is `likely` logged out; `agy` itself is the sign-in command.
- **DeepSeek Harness** (`dsh` 0.2.0-rc.2) has no login command. It is `likely` logged in when `DEEPSEEK_API_KEY` is set (the key of its shipped provider, `deepseek-official`), when a variable named by an `apiKeyEnv:` line in `$DSH_HOME/cordis.patch.yml` or `$DSH_HOME/profiles/acp/cordis.patch.yml` is set (`DSH_HOME` defaults to `~/.dsh`; a commented line does not count), or when either file has a `providers:` key (its own key variable is then not checked). Otherwise it is `likely` logged out, and the detail says to set `DEEPSEEK_API_KEY` or configure a provider under the `llm-pi-ai` plugin; `dsh --profile acp --dump-config` prints the composed configuration. The version is `dsh --version`.
- **Oh My Pi** (`omp` 18.10.3) reads a provider key from its environment (`GEMINI_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY` or `ZAI_API_KEY`: `likely` logged in, `GEMINI_API_KEY is set (an API key; not checked)`), or from `~/.omp/agent/.env` or `~/.omp/.env`, which it loads (the script prints the variable's name, never its line). `omp login PROVIDER` stores an OAuth login as a row of `~/.omp/agent/agent.db`; where the machine has `sqlite3` or `python3`, the script reads the provider names of the stored logins and nothing else, and the detail says `omp login stored for anthropic in ~/.omp/agent/agent.db (not checked)`. With the database and neither reader the login is `unknown` and the detail says what is missing; with no key and no login it is `likely` logged out, and the detail says to set a key or run `omp login`. `omp` is also a Bun script that needs Bun 1.3.14 or newer: when its version command fails and the Bun on `PATH` is missing or older, the version note says so and names the installer that upgrades it (`curl -fsSL https://omp.sh/install | sh`), because `npm install -g` or `bun install -g` of `@oh-my-pi/pi-coding-agent` leaves Bun as it is.
- **Gemini CLI** (0.63.0) takes `GEMINI_API_KEY` (or `GOOGLE_API_KEY`) or `~/.gemini/oauth_creds.json`. Its headless `gemini -p` refuses an untrusted directory unless `GEMINI_CLI_TRUST_WORKSPACE=true` (or `--skip-trust`), so a logged-in Gemini CLI's detail says whether that variable is set and how to set it.

Claude Code has `claude auth status`, and emdash calls a status check for it, but that check's source (`claude/auth.ts`) is not in the vendored files. Branchyard asks the command itself, and reads only the `loggedIn` flag and `authMethod` from its JSON: `claude auth status: logged in (oauth_token)`. It was run on 2.1.310 and printed `{"loggedIn": true, ...}`; a Claude Code too old to have it says nothing usable, and the credential file and key variables decide, as for the harnesses below. Run on 25 October 2026 against 2.1.310 with a private `HOME` and a clean environment, it answered `loggedIn: false, authMethod: none` (shown as `logged out`, with `by run --harness claude-code` refused before a branch, exit 1), and `loggedIn: true, authMethod: oauth_token` once `ANTHROPIC_AUTH_TOKEN` and `ANTHROPIC_BASE_URL` were set: Claude Code's name for a gateway token such as Z.ai's, though no OAuth login is involved. Two things in what it reads are not in the home or the environment you gave it. In a hosted Claude Code container it also reads that container's own token from `/home/claude/.claude/remote/.oauth_token` whatever `HOME` is (seen by `strace`), so a clean environment there still answers `logged in`; mask that directory (a private mount namespace with a `tmpfs` over it) before testing a logged-out state or pointing Claude Code at another provider, or the container's token may be what is sent. And the command is Claude Code starting up, in your `HOME`: it ran Claude Code's own start-up migrations, which rewrote `"model": "opus"` to `"opus[1m]"` (and reordered the keys) in an existing `~/.claude/settings.json` and created `~/.claude.json` and `~/.claude/backups/`, while `claude --version`, which is also run, changed nothing. Every command that reads this login (`by harnesses`, `by doctor`, `by setup`, `by up`, the preflight of `by run`) can do that once. Claude Code documents no switch to turn the migrations off.

### Quota

On this machine, Claude Code's and Codex's default logins carry the usage `by usage` reads ([usage](usage.md)): the percent of the 5-hour and weekly windows, and whether a limit was reached. A login at its limit cannot run. Other machines show no quota unless they advertise it: `by serve` and `by worker` do; a bare `branchyard-server` does not.

### Caching

This machine's inventory is kept for a minute in `harness-inventory.json` beside your user configuration, keyed by `PATH`, `HOME`, the install directories and the time limit, so a second `by harnesses` (or `by doctor`) within the minute reads the file and runs nothing. `--refresh` detects again; an install, update or login here clears it. Other machines are detected each time.

Versions outlive the minute: when a fresh detection finds a version command too slow to answer, a harness at the same path that answered last time keeps that version, and the note under the table says `slow to start (took >10 s); the version is from the last time it answered`. A harness that has never answered says only that it is slow, with `--refresh` and `BRANCHYARD_HARNESS_TIMEOUT` as the next step.

A key `by harnesses login ID --api-key` stored is added after the cache is read, from `[secrets]` and the files it names (by name, never read): the harness then shows `key stored (VARIABLE)`.

## Other machines

- **`--on ssh://[user@]host[:port]`** runs the script with `ssh -T -o BatchMode=yes host sh -s` (`BRANCHYARD_SSH` names the program). A path after the host is allowed and ignored.
- **`--on recipe:NAME`** makes a fresh machine from the repository's [recipe](recipes.md), runs the script through its transport, and destroys it. A repository's recipe runs only once you trust it, as `by recipe` requires.
- **`--remote URL`** shows the server's live workers that serve repositories you can see, the answering server first, each with the inventory it advertised (`GET /v1/inventory`). `--on` and `--remote` together are refused.

## Installing and updating

```sh
by harnesses install codex                # asks on a terminal
by harnesses install codex --yes
by harnesses update codex --version 0.200.0 --yes --on ssh://me@build.example
by up                                     # installs every harness the configuration names, under the same policy (docs/up.md)
```

`by catalog refresh` reads the npm registry's latest release of every catalog harness installed with `npm install -g`, pinned with its integrity hash and cached with a checksum ([live catalogs](registry.md#live-catalogs)). `by harnesses` then notes a newer release beside an installed harness, and `by harnesses update ID` without `--version` names it. Nothing is fetched unless asked, and the cached release is only a suggestion: `--version` takes it.

`install` and `update` run the catalog's install command for the harness: the first one whose programs the machine has (`npm install -g @openai/codex`, else `curl ... | sh`, else `brew install --cask codex`). `install` of a harness already there does nothing; `update` runs it again.

**Pins.** A package manager's command (`npm`, `bun`, `pnpm`) is pinned to the version the harness's default profile was checked against (`codex-cli 0.157.1` gives `@openai/codex@0.157.1`), or to `--version`. A piped installer takes no version, so it runs unpinned and `by` says so; `--version` with one is refused.

**Verification.** After the command, detection runs again for that harness. The install is verified when the command exited 0, the harness is found, and its version is the pinned one when pinned. Otherwise `by` fails and shows the end of the output.

**Policy.** `[harnesses]` in your user configuration (`~/.config/branchyard/config.toml`, or `BRANCHYARD_USER_CONFIG`):

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

A refused install names the file and the key: without a terminal and with nothing set, `not running ...: [harnesses] install is "never" here`, then `put this in <your user file>: [harnesses] install = "ask"`, and `by harnesses install ID --yes` to run it. An `allow` list that leaves the harness out says to add it there.

**Trust.** Installing runs software on your machine, so only your user file can allow it. A repository's `branchyard.toml` may set `install = "never"`; `install = "ask"`, `"auto"` or `allow` there is a configuration error, as `by config validate` reports. Install commands come only from the catalog built into `by`, never from a repository. A harness running on a branch (`BRANCHYARD_BRANCH` set) can never install or log in.

**Where.** Here, or `--on ssh://...` (over `ssh`, as above). A recipe's machines are made fresh each time, so `--on recipe:NAME` is refused with the command to put in its create script. A server's workers never install; `--remote` is refused with how to install on a worker's machine.

## Logging in

```sh
by harnesses login codex                        # runs `codex login --device-auth`
by harnesses login codex --on ssh://me@build    # the same over ssh -t
printf %s "$KEY" | by harnesses login claude-code --api-key
```

`login` runs the harness's own login command from the catalog (`claude auth login`, `codex login --device-auth`, `opencode auth login`, `gemini` and `agy`, which sign in on their first interactive run) with your terminal, so a URL or device code it prints reaches you directly. Over ssh it uses `ssh -t`. A harness found off `PATH` is run by its path. Afterwards detection runs again and `by` prints the login state.

Without a terminal (a script, or `--remote`, since a worker has none) nothing runs: `by` says what to run and where (`run \`codex login --device-auth\` there, in a terminal, as the worker's user`). A recipe's machine gets its login from the recipe's create script or its environment. A harness whose catalog entry records no login command says what to do instead: give it a key with `--api-key` when it has a key variable, or start it and sign in inside it, then `by harnesses --refresh`.

**Antigravity with a key.** With `GEMINI_API_KEY` set, `by harnesses login antigravity` has nothing to sign in to; `agy` needs `"modelProvider": "gemini"` in `~/.gemini/antigravity-cli/settings.json` to read the key. `by` merges that one key into the file (the rest of it kept, with its key order and indentation; the file is rewritten as plain JSON, so comments in it would be refused), shows the change as a diff, and writes it after you answer yes on a terminal or with `--yes`. Without a terminal and without `--yes` it shows the diff and writes nothing (exit 2, `status` `manual`, like any login that only says what to run). A different provider already chosen (`"modelProvider": "vertex"`) is never replaced: `by` says so and leaves the file. The write is recorded in the harness log as `wrote_settings`. With the setting already there it says so and does nothing.

**Exit codes.** `login` says whether it logged in.

| Exit | `--json` `status` | When |
|---|---|---|
| 0 | `done` | The harness's own flow ran and exited 0; the state afterwards is `login` in the JSON |
| 0 | `stored` | `--api-key` stored a key |
| 2 | `manual` | Nothing ran: there is no terminal, the machine is a recipe's, a worker needs a login, or the catalog records no login command (it names `--api-key` or where to look). The message (stderr; with `--json`, `run` or `workers` in the JSON) says what to run and where |
| 1 | `failed` | The flow ran and exited non-zero. `by` also exits 1, with a message and no `status`, when it cannot start one: the harness is not installed here |

So `by harnesses login codex && by run ...` goes on only after a login that happened. The log records a `manual` outcome the same way.

**An API key.** `--api-key` reads a key from standard input (hidden on a terminal) for the harness's first key variable, writes it to `secrets/<VARIABLE>` beside your user configuration (directory 0700, file 0600), and names it in your user file's `[secrets]` (`ANTHROPIC_API_KEY = "@…/secrets/ANTHROPIC_API_KEY"`). That is the existing secrets path: branches with a private home (`--isolated`, a sandbox) are given it ([setup](setup.md)). The key is never printed, never on a command line, never in the log, and refused when the file would land inside the repository. A harness you run directly reads its own environment, so export the variable from that file yourself.

## The log

Every install, update and login attempt is a line in `harness-events.jsonl` beside your user configuration (0600): when, the machine (`local`, `ssh://host`), the harness, the action, who asked (`by harnesses` or `router`), the command, the outcome (`verified`, `failed`, `refused`, `ran`, `manual`, `stored_key`; older lines say `reported` for what is now `manual`), the versions before and after, and why. `by harnesses log [--json]` prints it.

## Workers

`by serve`, `by worker` and `branchyard-server` detect the harnesses on their machine when they start and every five minutes, on a thread of their own, and send the inventory with each beat to the `workers` table (`by_workers` on PostgreSQL, column `inventory`, added by the usual migrations). From it each worker derives a label `harness:<id>` for every harness that can run there, so work can require one (`--require-label harness:codex`) with nothing configured ([worker labels](server.md#worker-labels)). Labels take `:` for this.

**Steering.** A task that runs harnesses by name (no `command` of its own or from `harness_commands`, and no sandbox provider) gets `harness:<id>` added to its required labels when some live worker serving the repository advertises an inventory in which all of them can run. When none does, nothing is added: work is steered toward a worker that has its harnesses, never held back because no worker advertises them. `by --remote ... run --harness codex` therefore lands on a worker that has Codex logged in.

`"inventory": false` in the server configuration, or `--no-inventory`, turns detection and advertising off. A server that triggers routed tasks ([triggers](triggers.md)) consults the live workers' inventories the same way.

## Before a run

`by run`, `by fan` and `by send` look at the harness before they create a branch, so a harness that cannot start fails with the command to run, not after a branch exists and a 401 has been retried for half a minute. They ask the inventory `by harnesses` shows: this machine's cached one when it is a minute old or less, else a detection of just the harnesses named.

| The inventory says | `by run --harness ID` |
|---|---|
| not installed | Refuses with `Next: by harnesses install ID` (`--yes` too, where `install = "ask"` and there is no terminal), or the catalog's own command when the install policy would refuse it, with the file and key that would allow it |
| found, not on `PATH` | Refuses, with the directory to add |
| logged out, verified (the harness's own status command said so) | Refuses, with the login hint; `--no-preflight` runs anyway |
| logged out, but `--isolated` and the run's secrets (`--secret`, `[secrets]`, a key `by harnesses login ID --api-key` stored) name the harness's key variable | One note on stderr, and goes on: the private home gets a login the host's status command cannot see |
| logged out, likely (credential files and key variables, a guess) | One note on stderr, and goes on |
| logged in, or unknown | Goes on, silently |

A harness the catalog knows but nothing drives (`droid`, `kiro`) is not "no harness named": `by` says it is known but not drivable, and lists what is. `by send` checks the harness the branch already has. Nothing is checked, and `--no-preflight` is not needed, with `--command` (the executable is yours; a branch started with one keeps it for its later sends), a routed run (below: the router asks the same inventory), `--provider` of a sandbox or a recipe (the image provides the harness), or inside a harness's own branch. The check is for the default harness too, and then says that `by run` uses `claude-code` unless `--harness` or `[defaults] harness` says otherwise. `--no-preflight` is for a login Branchyard cannot see, such as Codex with a model provider of its own; `--yes` is not used for this, since it answers tool permission requests.

For Claude Code the key variables an isolated run provisions are `ANTHROPIC_API_KEY` (written to a 0600 file in the private home that `apiKeyHelper` prints) and `CLAUDE_CODE_OAUTH_TOKEN`. Run on 25 October 2026 against Claude Code 2.1.310 with no login and GLM-4.6 behind Z.ai's Anthropic-compatible endpoint: `by run --harness claude-code --isolated --secret ANTHROPIC_API_KEY=VAR` printed `by: note: claude-code is logged out here; the isolated run gets ANTHROPIC_API_KEY from its secrets. Going on.` and the turn completed ($0.10, one `Write`, card `ready`). Isolation drops the `ANTHROPIC*` variables from the environment, so the endpoint went into the private home's `.claude/settings.json` (`env.ANTHROPIC_BASE_URL`) before the run; provisioning merged `apiKeyHelper` beside it. The same secret without `--isolated` is still refused, and the refusal reads `set ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN`, which is what `--secret` did; add `--isolated`. `--secret ANTHROPIC_AUTH_TOKEN=VAR` (a gateway token) is refused as logged out, and rightly: it is not a key variable here, and with `--no-preflight` provisioning reports `unused secrets ANTHROPIC_AUTH_TOKEN` and the turn fails with `Not logged in`. To reach a gateway from an isolated Claude Code, give the key as `ANTHROPIC_API_KEY` and write the base URL into the home's settings, or use the [model gateway](model-gateway.md). A key stored with `printf %s "$KEY" | by harnesses login claude-code --api-key` showed as `key stored (ANTHROPIC_API_KEY)` in `by harnesses`, and an `--isolated` run with no `--secret` then went on without a note, provisioned it the same way and completed ($0.19, one `Write`). `[secrets]` written by hand in `branchyard.toml` takes the same path as `--secret` and was not run live.

## The router

A routed `by run` or `by fan` ([fleet](fleet.md)) asks this machine's inventory about each candidate that runs a harness by name (no `command`, no sandbox), after its profile checks and before the `PATH` check. A candidate that is not installed, is logged out, is off `PATH` or is at its usage limit is excluded with that reason, like an unavailable one:

```text
by:   not codex: codex is verified logged out (codex login status: Not logged in); run `by harnesses login codex`, or set OPENAI_API_KEY or CODEX_API_KEY
by:   not qwen-code: qwen-code is not installed on this machine; install it with `by harnesses install qwen-code` (the router installs on demand only with [harnesses] install = "auto")
```

With `install = "auto"` (and `allow` naming it, when set), a missing candidate is installed first, verified, and recorded in the log by `router`; then the route goes on. `by fleet route` never installs: it shows such a candidate excluded with "a routed run would install it first". A routed fan picks each attempt from the same eligible candidates, so split work lands where its harnesses run.

With no `[fleet]` configured, `--auto` routes among the harnesses this inventory reads as logged in, likely or verified: the [inventory default](fleet.md#the-inventory-default). An installed harness that is logged out, or whose login cannot be told, is listed as left out with its login detail. `by init project` reads the same inventory to suggest a default harness that is logged in and to offer that default as an explicit `[fleet.default]` ([setup](setup.md#routing-and-the-gateway-from-what-is-present)).

## Not done yet

- The check before a run covers `by run`, `by fan` and `by send` here; `by fork`, `by map` and `by reincarnate` do not make it, and `--remote` is the server's.
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

- `crates/branchyard/tests/inventory.rs`: detection on a temporary `PATH` of fake harnesses: versions read, Antigravity's key counting only with `modelProvider` gemini in a fake home's settings, DeepSeek Harness's provider read from `cordis.patch.yml` (either file, `DSH_HOME`, an `apiKeyEnv` variable, a comment ignored), Gemini CLI's trusted-workspace note, a status command saying logged out (verified), a key variable seen by name (its value in no output), unknown logins, a missing harness, a version command that hangs killed by the timeout, each command run once; `only`; the cache's key and TTL; an install through a fake `npm`, pinned, verified by detecting again, and a pin not delivered or a failing command not verified; the log; the router excluding a logged-out and a missing candidate with reasons, a candidate with a command left to the `PATH` check, the allowlist; a gate installing on demand under `auto` and recording it, a preview that runs nothing, `ask` never installing.
- Probing, this round: unit tests for the timeout note and its bound, the script's concurrency and read-back order, the versions kept from the last answer, the login words, `claude auth status`, the one login hint and the live-run levels (`crates/branchyard/src/inventory.rs`); `crates/branchyard-cli/tests/harnesses.rs` runs fake harnesses that each wait for a peer to start (printing `-alone` when asked one after the other), one that takes four seconds to print a version under the default bound, one that answered and then hangs, and Antigravity's settings merge (shown without `--yes` and not written, written with it with the rest of the file kept, already set, a different provider refused, no file). Measured here with the real CLIs, no model calls, page cache dropped before each run: `by harnesses` took 42.7 seconds and showed `?` for six of nine versions with the 5-second bound and the harnesses asked in turn, and 4.4 seconds with every version with the 10-second bound and all asked at once; a second call took 0.08 seconds.
- `crates/branchyard-server`: the store conformance (`check_labels`, on SQLite and, in `tests/postgres.rs`, PostgreSQL) for a beat carrying an inventory, derived labels, steering and `WorkersGate`; an older SQLite `workers` table migrated; `tests/inventory.rs` over HTTP: work requiring `harness:codex` waiting and saying why, then claimed by a worker whose inventory has Codex with no label configured, `GET /v1/inventory`, a task naming Codex steered and one naming a harness no worker has not held back; on PostgreSQL, two servers on one database, the second a worker with Codex.
- `crates/branchyard-cli/tests/harnesses.rs`, with the built `by`: the table and JSON (verified, likely, unknown, a timeout, off `PATH`), the cache and `--refresh`, `--on ssh://` and `--on recipe:` through `fake-ssh` (the recipe's machine destroyed after, an untrusted recipe refused), installs under `never`, `ask`, `--yes`, `auto` and an allowlist, a repository trying to allow installs refused, a pin and a failed verification, the log; install and login over ssh (`ssh -t`); a login through a pty (the device code passed through, verified after; `--json` `done`), without a terminal (exit 2, `manual`), an API key stored 0600 and named in `[secrets]` (`--json` `stored`); `by run`, `by fan` and `by send` refused before a branch for a verified logged-out and a missing harness, noting a guess, skipping the check for `--command` and `--no-preflight`; `by harnesses --profiles` outside a repository; a refused install naming the file and key; the router excluding a logged-out and a missing candidate, previewing, and installing one on demand before running it on the fake ACP agent; `by serve` advertising its inventory to `by harnesses --remote`, login reported for a worker, install refused.

**Not tested:** a real harness installed, updated or logged in to; a real `npm`, `curl` installer or Homebrew; a real `sshd`; macOS's keychain case; quota on a real login; a trigger's routed task consulting the workers' inventories (`WorkersGate` is tested in the store conformance, not through a firing trigger). The status phrases are emdash's; only `codex login status` printing `Logged in using ChatGPT` was observed, on Codex 0.157.1. The Antigravity, DeepSeek Harness and Gemini CLI paths above are tested against fake binaries and configuration files, not by signing in to the real CLIs.

## Code

| Where | What |
|---|---|
| [`inventory.rs`](../crates/branchyard/src/inventory.rs) | Types, the detection script and its parser, login checks and their sources, the cache, policy, plans, pins, installs, the log, `LocalGate` |
| [`fleet.rs`](../crates/branchyard/src/fleet.rs) | `RouteOptions::harnesses`, consulted in `availability` |
| [`store.rs`](../crates/branchyard-server/src/store.rs), [`ops.rs`](../crates/branchyard-server/src/ops.rs), [`api.rs`](../crates/branchyard-server/src/api.rs) | The workers' `inventory` column, advertising and derived labels, steering at admission, `WorkersGate`, `GET /v1/inventory` |
| [`harness_cmd.rs`](../crates/branchyard-cli/src/harness_cmd.rs) | `by harnesses` and its actions, machines (`--on`), quota, the API-key store, the router's gate |
| [`config.rs`](../crates/branchyard-setup/src/config.rs) | `[harnesses]` and the rule that a repository may only forbid installs |
