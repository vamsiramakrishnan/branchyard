# Setup

## One command

```sh
cd my-repo
by setup
```

`by setup` takes a repository from nothing to a working `by run`. It runs the pieces below in order, each skipped when there is nothing to do, and ends with a short card: what it found, what it wrote, what it installed, what only you can do, and the next command. It composes the interview engine and the harness installer that `by init` drives; it adds no logic of its own to them, and it calls no model.

1. **Detect.** The repository and its default branch, the harnesses on `PATH` with their versions and login, and the check command the files suggest (`Cargo.toml` → `cargo test`, `package.json` → `npm test`, …).
2. **Config.** Without a `branchyard.toml`, the `project` topic with its defaults ([what it writes](#topics)). On a terminal it asks only what cannot be detected: which harness to use by default when several are logged in, a check command when the files suggest none, and isolation. An existing `branchyard.toml` is left as it is. The model gateway is not part of the defaults: the file ends with a two-line comment that points at [model-gateway.md](model-gateway.md), and `by init project` offers it as a question.
3. **Scripts.** When the defaults include `[workspace]` scripts (an install, say), `by setup` shows them. On a terminal it asks whether to trust them; with `--yes` it trusts the ones it just wrote and says so in the card (`by workspace untrust` takes it back). Without this the first `by run` is refused until `by workspace trust`.
4. **Hooks.** For every harness on `PATH` that has an install (`by init plugin` lists them), the same install `by init plugin ID` makes: hooks, and where the harness has them, commands and skills, written or merged into its own files. Each harness is planned alone, so a file of yours that differs from the packaged copy holds back that harness and nothing else (it is named, with the command that shows the diff). The files are listed before they are written, and a merge only adds.
5. **Hook check, and whether the harness was heard.** After installing it runs `by hook --harness ID …` once per harness, with a canned input and no model, the way the harness would. `by hook answers` means `by` is on `PATH` (every hook starts with `command -v by`), and it answered in that harness's dialect without complaint. `by hook is silent` names why: `by` is not on `PATH`, or it printed a complaint. The check forgets its session, so it leaves nothing in `.branchyard/hooks/`, and it sets `BRANCHYARD_HOOK_CHECK=1`, so it is not noted as the harness's call. Answering is not proof that the harness runs the hook (a trust prompt, a restart): `by hook` notes every call a harness makes ([heard](harness-hooks.md#heard-did-the-harness-run-it)), and the line under each harness says what was heard: `heard 3m ago (session-start)`, `never heard: start a new Codex session`, or `last heard 2d ago (session-start), before by setup rewrote the hooks` when setup has just written that harness's files again (it notes when it wrote them, beside the user configuration).
6. **Logins.** For each harness whose login is not known to be good, the command to run (`by harnesses login ID`, which runs the harness's own flow). `by setup` never starts a login. A login Branchyard could not verify is called *not verified*, never *logged out*: it looks for login files and key variables only, and an environment can authenticate a harness without them. The card names, for the default harness, the auth source it found, or `unknown`, and says that `by run` may still use (and spend on) a login it could not see.

The card ends with **Only you can do:** the logins above, the steps each harness's install leaves to you (Codex trusting its hooks, Gemini CLI's trusted folder, a restart, …), the hook problems above, and `by workspace trust` if you declined it; then `Next: by run "fix the flaky test" --check "cargo test"` with the check it detected.

```sh
by setup                    # on a terminal: shows the files, asks once, then writes
by setup --yes              # take every default, ask nothing
by setup --dry-run          # show everything, write nothing
by setup --json             # one object: detected, written, installed, todo, next
by setup --yes --harness claude-code,codex   # hooks for these only (named even if not on PATH)
by setup --yes --no-hooks   # branchyard.toml only
by setup --yes --no-config  # hooks only
by setup --guided          # choose a project and compute backend, then check readiness
by setup --directory /path/to/project
by setup --yes --up --json  # apply setup and return one readiness result
```

The single-command installer hands its terminal to `by setup --guided`. This
flow can select an existing project or initialize a selected directory with
consent. It offers local execution, a local Microsandbox runtime, an existing
Substrate connection, or a dedicated GKE/GCE deployment. Antigravity is selected
when `agy` is detected; remote setup defaults to its packaged image. Cloud setup
reviews the named resources and asks before creating them. The cloud helpers are
packaged with the CLI, so a Branchyard source checkout is unnecessary.

Guided setup reuses the project interview and hook installers above. An existing
`branchyard.toml` is preserved and its compute resources are checked again. Use
`by init project` to change an existing connection. A local microVM requires a
runtime-enabled Linux build and KVM; the portable package reports the missing
build support before attempting setup.

For GKE connections created by the guided workflow, rerunning `by setup --guided`
refreshes the Control token and its owned local forwarding processes. It checks
the private deployment records before doing so and changes no cloud resources
or actor template. If a forwarding port changed, only the saved endpoint/router
fields are updated, retaining the rest of the configuration and its comments.
Manually configured connections are left to their operator. Cloud helpers require
Python 3.10 or newer; setup checks that version before invoking them.

`--guided` finishes by applying the shared `up` checks. `--up` adds that step to
ordinary setup too. For a saved HTTP or SSH server, setup authenticates to that
server and checks its serving workers' advertised harness and credential
evidence. These checks make no model call. With either option, a readiness step
that needs attention exits 1; `--json` includes `readiness` and `ready` in the
same setup object. A dry run skips application and readiness. Without a terminal,
guided setup requires explicit `--yes`; a missing terminal reports `manual`
with exit 3. Initializing a repository also requires
an explicit `--directory`. Headless defaults select local execution, while cloud
creation uses the interactive resource review.

The suggested first command includes `--allow-unapproved-tools` for a harness
such as agy that cannot route individual tool approvals through Branchyard.
Installing a cloud worker does not approve repository workspace scripts or run
project dependency installation; review and trust those explicitly on the worker
and configure its server workspace policy before the first task that needs them.

Without a terminal, and without `--yes`, `by setup` behaves as `--dry-run` and says to rerun with `--yes`; `--json` never prompts either. Detecting refreshes the harness inventory cache that `by harnesses` keeps beside the user configuration (a minute old at most); a dry run touches nothing else. A second run writes nothing and says so (`Nothing to write: already set up.`): the config exists, each harness's files are already as packaged, and the hook check still runs. `by setup` exits 0 when it is done, even with things left for you, and non-zero only on a real error, with the status of its kind ([exit status](cli.md#exit-status)): 3 outside a git repository (`not_a_repository`; with `--no-config` it installs hooks anywhere), 2 for a harness named with `--harness` that it has no install for (`unknown_harness`), 1 for a harness named with `--harness` whose files cannot be installed (`would_overwrite` or `invalid_plan`, as `by init plugin` says it), project defaults that would not load, or a write that failed. Each says what failed and the command to run next; with `--json` it is the error envelope with its `exit`. It does not run inside a harness's branch (`denied`, exit 4).

`--json` prints one object (not the `branchyard.setup/v1` protocol; its shape is tested in `crates/branchyard-cli/tests/setup_cmd.rs`):

| Key | Holds |
|---|---|
| `dry_run` | nothing was written (asked for, or no terminal and no `--yes`) |
| `detected` | `repository` (`root`, `git`, `default_branch`), `harnesses` (`harness`, `version`, `login`, `evidence`, `auth_source`), `default_harness` (the same, plus `verified`), `check` |
| `config` | `path`, `state` (`written`, `would_write`, `exists`, `skipped`), `summary`, `workspace_trusted` |
| `written`, `unchanged` | the files written, and those already as planned, as the plans name them |
| `would_write` | in a dry run, the files it would write |
| `installed` | per harness: `state` (`installed`, `would_install`, `current`, `refused`), `reason`, `files`, `provides`, `hooks` (`answers`, `silent` or `null`: the canned check), `hooks_detail`, and `heard` (`null` for a refused install): `state` (`heard`, `never`, `stale`), `dialect`, `kind`, `harness_event`, `at_ms`, `age_seconds`, `session` (a digest), `repository`, `installed_at_ms` |
| `todo` | what only you can do: `kind` (`login`, `note`, `hooks`, `workspace`, `install`), `harness`, `text`, `command` |
| `next` | the next command: `by setup --yes` after a dry run, else `by run …` |

Bare `by` (no command) is the same idea for a repository that already has Branchyard in it: a card of under twelve lines with whether the repository is set up (`branchyard.toml`, `.branchyard/`), a `hooks` row when a harness on `PATH` has hooks installed (`heard: Codex 3m ago · never heard: Gemini CLI (start a new session)`), its branches by state (running, ready, asking, failed, …) and one `Next:` line: `by setup` when it is not set up, `by run …` when there are no branches, `by show BRANCH` for the first one that needs you, else `by ls`. Outside a git repository it says so and points at `cd <repo> && by setup`. `by --help` lists every command.

`by init plugin --status [ID…] [--json]` is the check on its own, at any time: per harness, whether its install is `current`, `stale` (some files missing or different) or `missing`, when a session of it was last heard running the hooks (`HEARD`: `3m ago`, `never`, `before install`), whether `by` is on `PATH`, and the one-time steps still on your list. It writes nothing.

Not done: `by setup` is tested with fake harness executables and a private HOME, on a terminal through util-linux `script`; it has not been run against every real harness. `by hook answers` stops at `by hook`; `heard` is the proof that a harness's own hook runner called it, and it comes only once a session of that harness has started after the install.

## The interview

`by init` sets Branchyard up by interview. One declarative interview per topic decides the questions, detects defaults from the machine, and produces a plan of files, each diffed against what exists and checked by the loader that will read it. Two front-ends run it:

- **A person** runs `by init` in a terminal: a wizard shows what it detected, asks, shows every diff, and writes only after a final confirmation.
- **A coding harness** (Claude Code, Codex, …) runs the [setup skill](../plugins/branchyard/skills/setup/SKILL.md): it drives `by init TOPIC --json --next`, asks the person each batch with its own question tool (Claude Code's `AskUserQuestion`), shows the dry-run diff, and applies after the person agrees.

Both front-ends call the same engine, [`branchyard-setup`](../crates/branchyard-setup/src/lib.rs), so they ask the same questions and write the same bytes. Nothing in setup calls a model, and no answer, plan or output ever holds a secret's value.

## Topics

| Topic | Writes | Checked by |
|---|---|---|
| `project` | `branchyard.toml` at the repository root, or the user's `~/.config/branchyard/config.toml`: default harness, model, effort, budget, turns, duration, permissions, isolation (a private HOME, not a sandbox: [modes](modes.md#isolated)), check, provider, secrets by name, harness logins by where their files are, the [inventory default](fleet.md#the-inventory-default) as an explicit `[fleet.default]`, [model gateway](model-gateway.md) backends and routes for the provider keys set ([below](#routing-and-the-gateway-from-what-is-present)), a server to use, and (project only) a [`[workspace]`](workspace.md) suggested from the repository's lockfiles, `Cargo.toml`, `pyproject.toml`, `go.mod`, Compose file and `.env` files (a run script only when `package.json` has a `dev` or `start` script, and only then is it asked about), or imported from another tool's committed configuration ([below](#importing-another-tools-workspace-configuration)) | the configuration parser `by` uses ([below](#configuration)) |
| `server` | A server configuration (default `.branchyard/server.json`): listen address and TLS, SQLite or PostgreSQL, one credential per tenant (hash only) with its token in a 0600 file under `tokens/`, tenant quotas, allowed providers, delegation, secrets by reference, a signed webhook | `branchyard-server`'s own loader and `Config::validate`, as `by serve --check` runs them |
| `rig` | A [rig](rigs.md) spec: a lead seat and, by shape, an implementer (two may run), a reviewer denied every editing tool, or two implementers on different harnesses; budgets split so the children fit | `by rig check`'s parser and planner |
| `deploy` | `compose.yaml` (PostgreSQL 16 and the server), `server.json`, and `secrets/` (client token, database password, both 0600, and a `.gitignore`) | the server's loader with `--insecure-bind`; `docker compose config` when docker is installed |
| `plugin` | For each harness you choose (the ones found on `PATH` are chosen for you), what its install manifest lists: hooks merged into its settings, and for Claude Code the `setup` and `delegate` skills, the commands and a status line ([distribution](distribution.md#install-per-harness)). One installer: `by init plugin ID...` and the interview write the same files | each file's own format (JSON, TOML, the skill frontmatter) |

`project` and `server` are the complete ones; `rig` covers the common team shapes (see [rigs](rigs.md) for fields it does not ask about); `deploy` and `plugin` are thin.

## For a person: the wizard

```sh
by init                      # choose a topic
by init project
by init server --defaults    # every question at its default, still reviewed before writing
```

The topic completes in every shell `by completions` supports, and an unknown one is a usage error listing the five (exit 2).

It detects the repository and its default branch, the installed harnesses and their versions (`--version`, ten seconds at most each, all asked at once), each installed harness's login as `by harnesses` reads it ([harness lifecycle](harness-lifecycle.md): its own status command, or credential files and key variables by name), the logins harnesses keep in files under the home directory ([below](#persisting-a-harness-login)), which credential variables are set (by name only), docker and the PostgreSQL client tools, the platform and KVM, a check command from the files at the root (`Cargo.toml` → `cargo test`, `package.json` → `npm test`, …), and existing configuration files, whose values become the defaults. Then it asks, reviews every file with its diff and its validator's verdict, asks before replacing a file that differs, and asks once more before writing. Ctrl-C at any point writes nothing.

Once a provider's key is set (`GEMINI_API_KEY`, `OPENAI_API_KEY` or `CODEX_API_KEY`, `ANTHROPIC_API_KEY`, or the key a `[models.backends]` entry names), `by models list` asks that provider which models the key can call, caches the answer, and marks the models no price covers, so a model name need not come from the static `models` lists of the harness catalog; `--harness ID` sets a harness's catalog list beside the live one ([model gateway](model-gateway.md#listing-models)). It prints the variable's name, never the key.

Without a terminal, `by init` refuses (exit 2) and points to the protocol. `--defaults` answers every question with its default. The wizard is `cliclack`; see [what was tested](#what-is-tested).

`by init project` needs a git repository, since the file it writes belongs to one: outside any, it refuses (``not a git repository: /path. Run `git init` here, or cd into one``), and so does `by up`. From a subdirectory it finds the repository's root. For defaults that apply everywhere, run it inside any repository and choose "Just me".

A `branchyard.toml` that `by` itself refuses (an unknown key, a value of the wrong kind) is not silently used as a starting point, nor silently dropped. The detected facts say so first (``branchyard.toml: does not load: line 2: unknown field `agent` (did you mean `harness`, under [defaults]?)``), none of its values become defaults, the plan's first note says the plan replaces the file, and `--apply` refuses with `branchyard.toml does not load (line 2); --force replaces it`. The same goes for the user file when you choose "Just me". `by config validate` shows what the loader says of it.

When the file written carries `[workspace]` scripts that are new or changed, the closing list leads with `by workspace trust` ("your first `by run` needs this"): until then `by run` refuses with "has scripts you have not trusted". When the repository's `[workspace]` is as it was, that decision stands and the command is listed last. A plan whose default harness is not on `PATH` says so in a note, with `by harnesses install ID`, unless branches run on a server or in a sandbox.

## For a harness: the protocol

```sh
by init --json                                          # the topics, and each one's first command
by init TOPIC --json --next [--answers FILE|-]          # the next batch, or done and the plan
by init TOPIC --answers FILE|- --dry-run [--json]       # the plan: files, diffs, validation
by init TOPIC --answers FILE|- --apply [--force] [--json]
```

`by init --help` shows the flags under *Protocol steps*. The command line is `by`'s clap parser, so the rules are enforced before anything runs, each as a usage error (exit 2) with clap's usage line: `--next`, `--dry-run` and `--apply` conflict with each other (with a `tip:` that they are separate steps), a step needs a topic, `--answers` needs a step, `--force` needs `--apply` (and conflicts with the other two steps), and `--json` with a topic needs a step. `--answers=FILE` works as well as `--answers FILE`, and the global options (`--remote` and the rest) may appear anywhere, though setup ignores them.

Answers are one flat JSON object keyed by question id. A step returns the normalized answers so far; send them back with the new ones. The loop:

1. `--next` returns at most four questions, none depending on another in the same batch, so one `AskUserQuestion` call takes the whole batch.
2. Each question has an `id`, a `kind`, a `header` of at most 12 characters, a `prompt`, `why` it is asked, at most four `choices` (label, description, the recommended one first), `more_choices` reachable as "Other", whether other text is allowed, a `default`, whether it is `optional`, the `when` condition that made it appear, and its `rules`.
3. An answer may be a choice's value or its label (what `AskUserQuestion` returns), text a question's rules accept, or `"skip"` or `null` to skip an optional question (a required question's `null` takes its default; `--defaults` fills every unanswered one). A refused answer comes back in `errors` and its question is asked again; the message never repeats a `secret_ref` answer.
4. When no question remains, `"done": true` and `plan` holds the files, the commands to run next, notes, and `valid`.

A step, from `crates/branchyard-setup/tests/golden/server-second.json` (facts and questions shortened):

```json
{
  "protocol": "branchyard.setup/v1",
  "topic": "server",
  "done": false,
  "facts": [
    { "id": "repository", "label": "Repository", "value": "/src/app (default branch main)" },
    { "id": "harnesses", "label": "Harnesses", "value": "claude-code 2.1.283 (Claude Code), codex codex-cli 0.157.1" }
  ],
  "answers": { "database": "postgres", "listen": "0.0.0.0:8421", "path": ".branchyard/server.json", "tenancy": "multi" },
  "errors": [],
  "questions": [
    {
      "id": "tls", "kind": "confirm", "header": "TLS",
      "prompt": "Should the server serve HTTPS with its own certificate?",
      "why": "Required off loopback, unless a proxy terminates TLS and you pass --insecure-bind.",
      "choices": [
        { "value": true, "label": "Yes", "description": "give a PEM certificate chain and key", "recommended": true },
        { "value": false, "label": "No", "description": "plain HTTP (loopback only)" }
      ],
      "allow_other": false, "default": true, "optional": false,
      "when": { "truthy": { "id": "listen" } }
    },
    {
      "id": "database.url", "kind": "text", "header": "Postgres URL",
      "prompt": "Which PostgreSQL database?",
      "why": "Stored in the configuration, so it must not hold a password: use a .pgpass file.",
      "choices": [
        { "value": "postgres://branchyard@localhost/branchyard", "label": "Local database", "description": "user branchyard on localhost", "recommended": true }
      ],
      "allow_other": true, "default": "postgres://branchyard@localhost/branchyard", "optional": false,
      "when": { "equals": { "id": "database", "value": "postgres" } },
      "rules": [ { "rule": "postgres_url" } ]
    }
  ],
  "remaining": 11
}
```

A plan's file:

```json
{
  "path": ".branchyard/server.json", "kind": "server_config", "mode": "0644",
  "action": "create", "overwrites": false, "sensitive": false,
  "content": "{\n  \"listen\": \"127.0.0.1:8421\", …", "diff": "--- /dev/null\n+++ b/.branchyard/server.json\n…",
  "validation": { "validator": "branchyard-server config (by serve --check)", "ok": true }
}
```

A generated token is a file with `"sensitive": true`, `"mode": "0600"`, and `null` content and diff. Kinds of question: `select`, `multiselect`, `text`, `confirm`, `number`, `path`, and `secret_ref`, which asks where a secret is (a variable name, or `@path`) and refuses anything that looks like a credential itself. Refusals print `{"error": {"kind", "message", "paths", "exit"}}` on stdout and exit 1: `incomplete`, `invalid_plan`, `would_overwrite`, `invalid_answers`, `io`; usage errors exit 2, and `by init project` outside a repository (`not_a_repository`) exits 3 ([exit status](cli.md#exit-status)).

The JSON Schema of every document is [`schema/setup.protocol.json`](../schema/setup.protocol.json). Output is deterministic: maps are ordered, questions keep their topic's order, and the same answers on the same machine print the same bytes (except a freshly generated token's hash).

### A Claude Code session

With the plugin loaded (`claude --plugin-dir plugins/branchyard`, or `by init plugin`), `/branchyard:setup server` or "set up a Branchyard server for this repo" starts the skill. An illustration of the exchange (not a recorded transcript; no model was run for this document):

```text
› /branchyard:setup server
● Bash(by init server --json --next)
  Detected: repository /src/app (main); Claude Code 2.1.283 and Codex installed; docker, psql.
● AskUserQuestion
  Config file: Beside by's state (recommended) · In the repository
  Listen:      Loopback (recommended) · All interfaces · IPv6 loopback
  Database:    SQLite (recommended) · PostgreSQL
  Tenants:     Just me (recommended) · Several teams
  › Beside by's state, Loopback, PostgreSQL, Several teams
● Write(.branchyard/setup-answers.json)
● Bash(by init server --json --next --answers .branchyard/setup-answers.json)
● AskUserQuestion   (TLS, Postgres URL, Tenant names, Scopes) …
● Bash(by init server --answers .branchyard/setup-answers.json --dry-run --json)
  create .branchyard/tokens/team-a-admin.token (0600, never shown)
  create .branchyard/server.json   ✓ branchyard-server config (by serve --check)
  Apply these 5 files?  › Yes
● Bash(by init server --answers .branchyard/setup-answers.json --apply --json)
● Bash(by serve --config .branchyard/server.json --check)   configuration ok
```

## Importing another tool's workspace configuration

A repository already set up for emdash, Orca, Superset, Conductor or a Dev Container says what a new worktree needs. `by init project` reads that file and suggests the same `[workspace]`, which you review like every other answer; nothing in it runs until you trust it (`by workspace trust`).

| File | Tool | Imported | Not imported |
|---|---|---|---|
| `.emdash.json` | emdash | `preservePatterns` as `copy`; `scripts.prepare` then `scripts.setup` as `setup`; `scripts.run`; `scripts.teardown`; `shellSetup` as `shell_setup` | nothing it defines |
| `orca.yaml` | Orca | `scripts.setup`; `scripts.archive` as `teardown`; the first `defaultTabs` command as the run script | further tabs; `worktree.sharedDirectories` (links, not copies); environment recipes, `issueCommand` |
| `.superset/config.json` | Superset | `setup`, `run` and `teardown` command lists, in `cwd` when set; `.superset/<key>.sh` when a key is absent | the per-user and `config.local.json` overrides, which are not committed |
| `.conductor/settings.toml` | Conductor | `[scripts]` `setup`; `archive` as `teardown`; `run`, or the default (else first) `[scripts.run.NAME]` with its `args` and `options.cwd` | the other named run scripts; `run_mode`, `available_in` |
| `.devcontainer/devcontainer.json`, `.devcontainer.json` | Dev Containers ([containers.dev](https://containers.dev)) | `initializeCommand`, `onCreateCommand`, `updateContentCommand` and `postCreateCommand`, in that order, as `setup` (a string in a shell, an array as one quoted command, an object's commands in order rather than in parallel); `postStartCommand` as the run script; `containerEnv` then `remoteEnv` as `[workspace.env]`; `prepare = true` with the file among the `inputs`, since a container is an environment built once per change to its definition | `image`, `build`, `dockerFile` and `features` (a container runtime, which Branchyard does not run: setup runs on this host, in the worktree, so what the image provided must be installed there), users, mounts, ports, `runArgs`, `customizations`, `hostRequirements`, `postAttachCommand`; a `.devcontainer/NAME/devcontainer.json` in a subfolder |

- A multi-line script becomes one setup command per line when each line stands alone, and stays one command when it has shell structure (`if`, loops, continuations, here-documents).
- Each tool's variables are renamed to Branchyard's: `$EMDASH_PORT`, `$CONDUCTOR_PORT` → `$BRANCHYARD_PORT`; `$EMDASH_ROOT_PATH`, `$ORCA_ROOT_PATH`, `$SUPERSET_ROOT_PATH`, `$CONDUCTOR_ROOT_PATH` → `$BRANCHYARD_ROOT`; `$EMDASH_TASK_PATH`, `$ORCA_WORKTREE_PATH`, `$SUPERSET_WORKSPACE_PATH`, `$CONDUCTOR_WORKSPACE_PATH` → `$BRANCHYARD_WORKTREE`; `$SUPERSET_WORKSPACE_NAME`, `$CONDUCTOR_WORKSPACE_NAME` → `$BRANCHYARD_BRANCH`. Others are left as written.
- A devcontainer's `${...}` expressions become the shell's: `${localWorkspaceFolder}` and `${containerWorkspaceFolder}` → `${BRANCHYARD_WORKTREE}`; their `Basename` forms and `${devcontainerId}` → `${BRANCHYARD_BRANCH}` (the worktree is named for the branch); `${localEnv:VAR}`, `${containerEnv:VAR}`, `${env:VAR}` → `${VAR}`, with a default as `${VAR:-default}`. Comments and trailing commas in the file are allowed, as the specification allows them.
- Precedence: a committed file's setup, run script and teardown replace what the lockfiles suggest; with several files, the first in the table's order that gives each part wins, and every file's copy globs are added (a glob `[workspace] copy` would refuse is left out and said).
- What was left out, and a file that could not be read, appear in the facts as *Not imported*.

The emdash and Orca readers are ports of `emdash-config.ts` (Apache-2.0) and `orca-yaml.ts` (MIT) at pinned revisions (`crates/branchyard-setup/src/import/`, recorded in `patches/ports.json`); Orca's file is read by a small reader for the block subset of YAML, which refuses flow collections, anchors and tags rather than guessing. Superset's and Conductor's are written from their public documentation only: Superset is under the Elastic License 2.0, and nothing of its code, schema or documentation text is copied. The devcontainer reader is written from the specification, with Coder's envbuilder (Apache-2.0, vendored under `vendor/envbuilder`) as the reference for the fields and the variable grammar; nothing of it is copied. Tested on fixtures written for the tests, not on those tools' own repositories.

## Routing and the gateway from what is present

`by init project` turns what it detected into configuration, one question per step, each with a default you can take or change:

| Step | Asked when | Writes | Default |
|---|---|---|---|
| Logins | a harness keeps a login in a file Branchyard can provision ([below](#persisting-a-harness-login)) | `[secrets] NAME = "@~/path"` for each login chosen | the installed harnesses' logins when new branches get a private home (isolated, or Microsandbox), else none |
| Fleet | no `[fleet]` in either file, and an installed harness is logged in | `[fleet.default]`: the [inventory default](fleet.md#the-inventory-default) as it is now (candidates, effort where it applies, judge) | no: without it `--auto` routes among what is logged in at that moment; with it `by run` without `--harness` routes too |
| Gateway | `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `CODEX_API_KEY` or `GEMINI_API_KEY` is set, and the file has no `[models.backends]` of its own | backends and routes for the keys set, below | no: the gateway is opt-in. `--defaults` and `by setup` leave it out and the file ends with a two-line comment pointing at [model-gateway.md](model-gateway.md); say yes to write the backends |
| Claude on Gemini | the gateway step is yes and `GEMINI_API_KEY` is set | a `claude-*` route to the gemini backend with a `backend_model` (default `gemini-3.5-flash`), or, with an Anthropic key, the gemini backend as that route's fallback | no; the question says the trade-off: the answers come from Gemini, not Claude, thinking comes back unsigned, and `count_tokens` is not served |

The default harness it suggests is Claude Code or Codex when installed and not known to be logged out, else the first installed harness that is logged in.

| Variable set | Backend | Routes |
|---|---|---|
| `ANTHROPIC_API_KEY` | `anthropic`: `api = "anthropic"` | `claude-*` |
| `OPENAI_API_KEY`, else `CODEX_API_KEY` | `openai`: `api = "openai"`, `key` the variable that is set | `gpt-*`, `codex-*` |
| `GEMINI_API_KEY` | `gemini`: `api = "openai"`, `url = "https://generativelanguage.googleapis.com/v1beta/openai"`, `compat = "gemini"` ([Gemini](model-gateway.md#gemini-through-the-openai-compatible-endpoint)) | `gemini-*` |

Keys are named, never written. The backends do not put a branch on the gateway: a branch uses them when run with `--model-gateway`, or once `[models] allow` is set. On a machine with `GEMINI_API_KEY` and `CODEX_API_KEY` set, Codex logged in by device code and Gemini CLI by its key, choosing the login, the fleet and Claude on Gemini gives, in short:

```toml
[secrets]
CODEX_AUTH = "@~/.codex/auth.json"

[models.backends.gemini]
api = "openai"
url = "https://generativelanguage.googleapis.com/v1beta/openai"
key = "GEMINI_API_KEY"
compat = "gemini"

[models.backends.openai]
api = "openai"
key = "CODEX_API_KEY"

[[models.routes]]
model = "claude-*"
backends = ["gemini"]
backend_model = "gemini-3.5-flash"

[[models.routes]]
model = "gpt-*"
backends = ["openai"]

[[models.routes]]
model = "codex-*"
backends = ["openai"]

[[models.routes]]
model = "gemini-*"
backends = ["gemini"]

[fleet.default]
candidates = [{ harness = "codex" }, { harness = "gemini-cli" }]
judge = { harness = "codex" }
```

## Persisting a harness login

A device-code login lives in a file. `codex login --device-auth` writes `~/.codex/auth.json`: its `auth_mode` (`chatgpt` for a ChatGPT login, `apikey` for a key) and the tokens, including the refresh token Codex renews its access with as it expires. A branch that runs in your own home uses that file as it is. A branch with a private home (`--isolated`, a sandbox) is not logged in, so Branchyard provisions the login into it as the `CODEX_AUTH` secret: the file's content, written 0600 to the branch's own `~/.codex/auth.json` before the harness starts ([provisioning](provisioning.md)).

```sh
by run "Fix the flaky test" --harness codex --isolated --secret CODEX_AUTH=@$HOME/.codex/auth.json
```

`by init project` finds the file, shows it with its `auth_mode` (the only thing it reads from it, and only when it looks like a word), and offers to record it in `[secrets]` by where it is, never by its content: `CODEX_AUTH = "@~/.codex/auth.json"`. Claude Code's login is provisioned the same way as `CLAUDE_AUTH` from `~/.claude/.credentials.json` (on macOS it is in the keychain, not a file), and Gemini CLI's as `GEMINI_OAUTH_CREDS` from `~/.gemini/oauth_creds.json`. Antigravity's token file and settings are found and shown (its `modelProvider`) but not provisioned: Antigravity takes `GEMINI_API_KEY`.

On an ephemeral machine, a CI runner or a cloud session whose home does not survive, keep the file's content as a secret variable named `CODEX_AUTH` in the environment the machine starts with (a repository or environment secret), and restore it to `~/.codex/auth.json`, mode 0600, before `by` runs:

```sh
mkdir -p ~/.codex
( umask 077 && printf '%s' "$CODEX_AUTH" > ~/.codex/auth.json )
chmod 600 ~/.codex/auth.json
codex login status        # Logged in using ChatGPT
```

The refresh token in it keeps the login valid from one run to the next without logging in again. Restoring the file serves every branch: those in your own home read it, `by harnesses` and the [inventory default](fleet.md#the-inventory-default) see Codex logged in, and `[secrets] CODEX_AUTH = "@~/.codex/auth.json"` carries it into private homes. If a copy is refused because its refresh token was used up elsewhere (by another copy of the same login), log in once more and store the new file's content.

## Applying

`--apply` recomputes the plan from the answers, then:

- refuses an invalid plan (`invalid_plan`) and writes nothing;
- refuses to replace any file whose content differs (`would_overwrite`, listing them) unless `--force`; a `branchyard.toml` that does not load is named as that, with its line, rather than as different content; a file already exactly as planned is left alone, so applying twice writes nothing the second time;
- writes each file through a temporary file and a rename, with its mode;
- creates a generated secret only when it does not exist, and never reads, replaces or prints one that does: re-running `by init server` keeps each existing token (its hash is taken from the file) so clients keep working.

## Configuration

`branchyard.toml` at the repository root, and the user file `~/.config/branchyard/config.toml` (`$XDG_CONFIG_HOME` if set; `BRANCHYARD_USER_CONFIG` names another path), share one format. The project file overrides the user file key by key; `BRANCHYARD_REMOTE`, `BRANCHYARD_TOKEN_FILE`, `BRANCHYARD_REPO` and `BRANCHYARD_CA_FILE` override both; flags override everything. Relative paths resolve against the file's directory, and `~/` against the home directory.

The format, its strict parser, the layering and the loader are one crate, [`branchyard-config`](../crates/branchyard-config/src/lib.rs), which the setup engine re-exports as `branchyard_setup::config` and the engine as `branchyard::config`. What the merged file gives a yard is the engine's too ([`branchyard::configure`](../crates/branchyard/src/configure.rs)): `Yard::open_configured(root)` opens a repository and gives it the connector gateway (`[connectors]`), the person's approvals (`[approvals]`), the model gateway (`[models]` with `[secrets]`), the knowledge settings (`[knowledge]`) and the `[defaults]` every task then starts from, with `[secrets]`, `[mcp]`, `[network]`, the default grants and the model access filled in when a task runs and left nothing by its caller. `by` opens every repository that way outside a harness, a server builds its model gateway with the same code, and a Rust program gets the same yard from the same files. A harness on a branch (`BRANCHYARD_BRANCH` set) reads no configuration, and `Yard::open` gives a yard with no defaults.

```toml
#:schema https://raw.githubusercontent.com/vamsiramakrishnan/branchyard/main/schema/branchyard.config.json
version = 1

[defaults]                  # new branches: by run, by fan
harness = "claude-code"     # or a profile ID
model = "large"
effort = "high"             # low, medium, high, xhigh, or 0-100
auth = "api-key"
budget_usd = 5
max_turns = 25
max_minutes = 30
permissions = "ask"         # or "yes"; unset: ask on a terminal, else deny
isolated = true            # a private HOME, not a sandbox (docs/modes.md#isolated); --isolated=false for one run
check = "cargo test"
provider = "local"          # or "microsandbox" / "substrate", with the matching table
instructions = "docs/agents.md"

[secrets]                   # applied only with a private home (isolated or sandboxed)
ANTHROPIC_API_KEY = "ANTHROPIC_API_KEY"   # the variable of that name
CODEX_AUTH = "@~/.codex/auth.json"         # a file; never the value itself
ZAI_API_KEY = "@~/.config/branchyard/secrets/ZAI_API_KEY"

[logins]                    # provider logins, with the provider's key from [secrets]; private home only
claude-code = "zai"         # by harnesses login claude-code --provider zai writes it

[mcp]
docs = "/usr/local/bin/docs-mcp --stdio"

[remote]
url = "https://branchyard.example.com:8421"
token_file = ".branchyard/tokens/admin.token"
ca_file = "ca.pem"
repo = "app"

[serve]
config = ".branchyard/server.json"          # by serve --config, when none is given

[microsandbox]
image = "ghcr.io/you/claude-code:2.1"
cpus = 2
memory_mib = 4096
pass_env = ["ANTHROPIC_API_KEY"]

[notify]                    # when a branch needs you or ends; see the README
enabled = true              # unset: on; --no-notify turns it off for one command
desktop = false             # also run notify-send (osascript on macOS)
terminal = "auto"           # osc9, osc777, bell or none

[connectors]                # the connector gateway; see docs/connectors.md
gateway = "http://127.0.0.1:8931/mcp"    # its /mcp URL; without it, connectors are off
bundles = "../connectors"                # the bundle root it serves
anvil = "anvil"                          # Anvil's command line
grants = ["github:read"]                 # for new branches with a private home that name none
sandbox_gateway = "http://192.168.127.1:8931/mcp"   # the gateway as a sandbox reaches it

[workspace]                 # project file only; scripts run once you trust them
copy = [".env", ".env.*"]
setup = "pnpm install --frozen-lockfile"
teardown = 'docker compose -p "by-$BRANCHYARD_BRANCH" down'

[workspace.run.dev]
command = "PORT=$BRANCHYARD_PORT pnpm dev"
default = true
```

`[workspace]` prepares each new branch's worktree and cleans up after it; it is not merged key by key. The user file may replace one repository's section with `[projects."/path/to/repo".workspace]`, which needs no trust; `[workspace]` in the user file and `[projects]` in a project file are refused. See [workspace](workspace.md).

The file is read strictly: an unknown key, a harness that is not in the registry, an effort, secret source, MCP command or URL the flags would refuse, is an error naming the key and its line, and every `by` command stops on it. A secret source that looks like a credential (a known key prefix, or a long mixed-case alphanumeric string) is refused without being quoted. The `#:schema` line lets taplo and editors with TOML schema support complete and check the file against [`schema/branchyard.config.json`](../schema/branchyard.config.json).

What each command takes, in `crates/branchyard-cli/src/defaults.rs` (`defaults::apply` after clap parses the command line and before dispatch; `defaults::apply_serve` for `serve` and `worker`, whose arguments go to the server's own parser):

| Command | Takes |
|---|---|
| `run`, `fan` | every `[defaults]` key the flags left unset, `[mcp]`, and `[secrets]` and `[connectors] grants` (unless `--connector` names any) when the branch has a private home; `isolated = true` is turned off for one run by `--isolated=false`. With `fork`, `reincarnate` and `rig run`, the effective `[workspace]`, once trusted ([workspace](workspace.md#trust)) |
| `send`, `fork`, `reincarnate`, `spawn` | `permissions` only: the rest would override what the branch, its fork parent or its seat already has |
| `serve`, `worker` | `[serve] config` as `--config`, unless the arguments give one (`--config FILE`, `--config=FILE`, `-c FILE`), ask for help or the version, or are `token new` |
| `watch`, and `run`, `fan`, `send`, `fork` while they wait | `[notify]`, unless `--no-notify` is given |
| every local command that opens the repository | `[connectors]`: the gateway its branches' turns are given, and what `by gateway` and `by connect` run ([connectors](connectors.md#configuration)) |
| every command but `serve`, `worker`, `init`, `config`, `mcp`, `completions`, `man` | `[remote]` for what `--remote`, `--token-file`, `--ca-file`, `--repo` and their variables left unset; `token_file`, `ca_file` and `repo` only when `url` is the server in use |

How "left unset" is decided: clap reads the four global variables itself (`env = "BRANCHYARD_REMOTE"` and so on), so by the time the files are consulted a variable is already in the parsed options and counts as given, which is what puts variables above both files. None of the options the files fill has a clap default value or a variable of its own, so an option that is `None` (or `--yes`/`--ask` both absent, or `--isolated` absent) was not on the command line. `by serve`'s `--config` is asked of the server's parser: `branchyard_server::cli::names_config` parses the arguments and checks `ArgMatches::value_source("config")` is the command line, so every spelling clap accepts counts and nothing else does.

Neither file is read by a `by` running inside a harness on a branch (`BRANCHYARD_BRANCH` set): the engine already gave that branch its options, and a project file must not turn a local branch's delegation into remote calls.

```sh
by config show [--json]              # every effective value and where it came from
by config path [--json]              # the user and project files, and whether they exist
by config validate [FILE] [--json]   # load FILE, or every file by reads, strictly; exit 1 if invalid
by config schema                     # the JSON Schema
```

`--json` may come before or after the action (`by config --json show`). `by config` alone prints its help and exits 2, as any command missing its action does.

`by doctor` runs these loaders with the rest of what makes a machine and repository ready (the store, the trust decision, the harnesses, a configured server) and prints one report of `ok`, `warn` and `fail` rows with what to do about each: see [doctor](doctor.md).

## `by serve --check`

`by serve --config FILE --check` (also `branchyard-server --check`) builds and validates the configuration as serving would and prints its warnings, then exits without binding, serving or writing a file: where serving would create a default token it only says so. It refuses what the build cannot serve (a `database` on a build without the `postgres` feature fails with the command that adds it), but it opens no store and connects to no database, so a wrong host or password shows only when `by serve` starts. `by init server` and `by init deploy` check every server configuration they propose with the same function, on a private staging copy of the plan's files.

## What is tested

- **Imports**: each importer on its own fixtures (mapping, variable renaming, script splitting, refusals of wrong types), the YAML subset reader, the vendored emdash and Orca sources still having the fields the ports read, precedence among several files in the engine, and `by init project --defaults --apply` writing a loadable `branchyard.toml` from each of the four files (`tests/setup.rs`).
- **Workspace**: the `[workspace]` section's parsing, refusals, digest and precedence, the project topic's detection and plan, and a golden batch (`tests/golden/project-workspace.json`); see [workspace](workspace.md#what-is-tested).
- **Engine** (`crates/branchyard-setup`): conditions and batching, answer normalization for every kind, detection with a fake probe (installed harnesses, KVM, existing files as defaults), every topic walked batch by batch answering by label and finishing with a valid plan, determinism of the JSON, no generated secret in any response, a pasted secret refused without being repeated, golden first batches, both schemas' freshness, and the embedded skills byte for byte against `plugins/branchyard/skills`. The project topic on a fake machine like the one above: logins found by path with their label and never a token (a value that could be a credential is not read as a label), recorded by where they are, suggested when isolated, removed when deselected; the inventory default written as `[fleet.default]` (logged-in harnesses only, the default harness first, effort `medium` for an isolated Codex, a judge that can be denied every tool), not offered with a `[fleet]` or nothing logged in; gateway backends for the keys present, Gemini's with its URL and `compat`, Claude on Gemini as a route or as Anthropic's fallback, and nothing without a key or over backends the file already has.
- **Command line** (`crates/branchyard-cli/src/args/`): `init` parsed into its topic and step for every topic and flag spelling; each conflict and missing requirement a usage error with exit 2 and its message; the help's topics and flags; `config`'s four actions with `--json` before or after them, and its usage errors; completions for five shells and the man page listing `init`, its flags and `config`. `defaults.rs` and the server's `names_config` test that `-c FILE`, `-cFILE`, `--config=FILE`, `--help`, `--version` and `token new` keep their own configuration.
- **CLI** (`crates/branchyard-cli/tests/setup.rs`): the built `by` in temporary repositories: a scripted harness completing every topic through `--next`, `--dry-run` and `--apply`; applying twice writes nothing; every generated file passes the tool that reads it (`by serve --check`, `by rig check`, `by config validate`); a multi-tenant server with one 0600 token per tenant and only hashes in the configuration; an off-loopback server without TLS refused by the server's loader; refusal to replace a file without `--force`; a pasted secret never echoed; no generated secret in any output; the non-terminal refusal; configuration defaults under flags and variables, and not inside a harness's branch; a Codex auth file holding a refresh token, a fake Gemini CLI and `GEMINI_API_KEY`: the login shown with its `auth_mode`, recorded as `CODEX_AUTH` by path, `[fleet.default]` and the Gemini backend with a Claude route written, the file passing `by config validate`, and the token in no output and no file. The machine's own credential variables are removed from every setup command. The wizard runs on a pseudo-terminal (util-linux `script`) accepting every default and writing after review; its question-to-prompt mapping and review rendering are unit-tested.
- **Unloadable file, scripts, plugin** (`crates/branchyard-setup/src/hint.rs`, the project topic's tests, `tests/install.rs`, `crates/branchyard-cli/tests/setup.rs`): an unknown key in the project or the user file named with its line and its replacement (`agent`, `budget`, a `[defaults]` key at the top, a typo), the plan's first note, the `--apply` refusal; a run script proposed only for a `package.json` script that exists (`dev`, then `start`, in the lockfile's package manager) and the question absent otherwise, a run script the file already has kept; `by workspace trust` first only for scripts that changed; the default harness missing from `PATH`; `by init project` and `by up` outside a repository, and from a subdirectory; `by init plugin` with nothing named refused with the way out (exit 2), one installer for the interview and for named harnesses, a line per harness of what it gets, diffs headed relative to the home directory, Claude Code's status line merged when absent and kept, with the line to choose, when present.
- **`by setup`** (`crates/branchyard-cli/tests/setup_cmd.rs`): the built `by` with four fake harness executables first on `PATH` (and `by` itself, to check its hooks), a private HOME and a throwaway repository: `--yes` writes `branchyard.toml` and merges the hooks into all four harnesses' files and prints the card; a second run changes no byte; `--dry-run` and a missing terminal write nothing; the `--json` object; `--harness`, `--no-hooks`, `--no-config`; a guessed login called *not verified* with its auth source, and a credentials file naming its source; the `[workspace]` scripts shown and trusted with `--yes`; hooks `silent` (once, for all four) when `by` is not on `PATH`; each harness `never heard` after the install, `heard` once a Codex session's hook call is made, `stale` after setup writes Codex's file again and `heard` after the next call, in the card, the JSON, bare `by` and `by init plugin --status`; a differing file holding back one harness; bare `by` in a set-up, not-set-up and no-git directory and with a finished branch; `by init plugin --status` current, stale, missing; `by init` without a terminal exiting 2; the gateway left out of the generated file; and, on a pseudo-terminal, the questions, the review, the single write confirmation and the declined trust.
- Not tested: a real harness running the skill (no model call was made), the wizard on macOS, and `docker compose up` of a generated deployment.
