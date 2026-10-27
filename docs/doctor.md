# Doctor

`by doctor` answers one question: is this machine, and this repository, ready to use Branchyard? It runs a fixed list of checks and prints one row for each: a status (`ok`, `todo`, `warn` or `fail`), what it found in one line, and, for every row that is not `ok`, what to do in one line. A line above the table, `Mode:`, says how this repository runs (below). It reads; it repairs nothing, and it prints no secret.

```sh
by doctor                    # the mode, a table, then what to do about each row that is not ok
by doctor --json             # the same as JSON, for a script or a CI step
by doctor --path ~/src/app   # check that repository instead of the one around here
by doctor --refresh          # ask the harnesses again instead of reading the cached inventory
```

The checks reuse what `by` already knows rather than looking again: the loaders `by config validate` runs, the trust decision `by workspace trust` records, the inventory `by harnesses` shows (kept a minute; `--refresh` detects again), the engine's own provider check, the service registry `by services` lists, and the client `--remote` uses. When a row says something is wrong, a command that depends on it would say the same.

`by up` ([up](up.md)) is the command that acts: it installs the harnesses the configuration names where your user file allows, starts the connector gateway, and lists what only you can do. Run `by doctor` to see where you stand and `by up` to get there.

## Statuses and exit codes

| Status | Means |
|---|---|
| `ok` | Nothing to do. Also what a check says when the thing it checks is not configured (`server.reachable: not configured`). |
| `todo` | A step you have not taken yet, with nothing wrong: Branchyard has not been set up in this repository (`by setup`), or a harness is installed and not logged in yet. Where everyone starts, so it is not a warning. |
| `warn` | Worth looking at, or a check that could not run. Nothing that stops a command working. |
| `fail` | A command that needs this would fail. |

`todo` is a status of its own and not a `warn` with a gentler word: a script that treats `warn` as something to read can leave `todo` alone, and `status` in the JSON is `todo` for a report whose worst row is one. The order is `ok`, `todo`, `warn`, `fail`; the report's status is the worst row.

A check that cannot run because another failed is a `warn` whose detail begins `not checked:` and whose fix names the check to fix first (`fix repository.git first`). It is never an `ok`: the report does not claim what it did not look at. The text output leaves these rows out of its *To do* list, since the failed row above them says what to do.

| Exit | When |
|---|---|
| 0 | No row failed: the report is `ok`, `todo` or `warn`. A fresh repository exits 0. |
| 1 | At least one row failed. |
| 2 | A usage error, as for every `by` command (`by doctor --nonsense`). |

There is no separate status for "doctor itself could not run". A problem with finding something out is a row (`repository.git` fails when `--path` is not a directory), and the rest of the report still prints. The report is on standard output.

## The mode

The line above the table says how this repository runs, from the same effective configuration the checks read (the user file, `branchyard.toml`, the `BRANCHYARD_*` variables and `--remote`):

```
Mode: local, no isolation · SQLite (.branchyard/state.db) · no server
```

| Mode line | When |
|---|---|
| `local, no isolation · SQLite (.branchyard/state.db) · no server` | The default: harnesses run as your user in a worktree. |
| `local, isolated (private HOME, no sandbox) · SQLite … · no server` | `[defaults] isolated = true`: a scrubbed environment and a private HOME, which is not a sandbox. |
| `sandbox: microsandbox · SQLite … · no server` | `[defaults] provider = "microsandbox"`. |
| `sandbox: recipe NAME · SQLite … · no server` | `[defaults] provider = "recipe"` with `recipe = "NAME"`. |
| `server: URL · state is kept by the server` | `--remote`, `BRANCHYARD_REMOTE` or `[remote] url`. Any `user:password@` in the URL is left out. |
| `… · by serve here uses PostgreSQL` | `[serve] config` names a server file whose `database` is a PostgreSQL URL. The URL is never shown. |

When the configuration does not load there is no mode to read: the line says `not known`, and `config.merged` says why. In `--json` it is `mode`.

## The checks

Rows come in this order, and their names are stable: a script may match on them. `harnesses.ID` has one row for each installed harness, and `hooks.ID` one for each harness with hooks installed, so their names depend on the machine.

| Check | What it looks at | `warn` | `fail` |
|---|---|---|---|
| `repository.git` | The repository around the current directory (`git rev-parse --show-toplevel`), or `--path`, or `BRANCHYARD_ROOT` inside a harness; its branch | no commit yet: branches start from one | not in a repository, `--path` is not a directory, or git cannot run |
| `repository.state` | `.branchyard/` exists and its store opens, by `Yard::open`, the open every `by` command makes (it also recovers a branch whose engine stopped) | not opened on purpose (see `repository.schema`) | the store does not open (no `.branchyard/` is a `todo`, below the table) |
| `repository.schema` | The schema version recorded in `.branchyard/state.db` against the one this binary reads, read through a read-only connection | older: the next `by` command that opens the store upgrades it in place, and an older `by` cannot read it after | newer than this binary, a version it does not read, or a file that is not a readable database |
| `repository.services` | `.branchyard/registry.db`, the registry `by services` shows | a record is expired or reclaiming, or live with a lease that ran out: `by services gc` reclaims it | the registry cannot be read |
| `config.user` | The user file (`BRANCHYARD_USER_CONFIG`, else `~/.config/branchyard/config.toml`) loads and validates, strictly, as `by config validate` loads it | | the loader refused it; the row shows its message, which names the file and the line |
| `config.project` | `branchyard.toml` at or above the repository, the same way | | the same |
| `config.merged` | The two files merged with the `BRANCHYARD_*` variables that override `[remote]`, as `by config show` merges them | a file above did not load | the merge was refused |
| `config.workspace_trust` | `[workspace]` scripts: whether they are trusted, as they are now (the digest of the section against the one `by workspace trust` recorded) | untrusted, or changed since trusted: new branches are refused until `by workspace trust` | |
| `harnesses.summary` | How many harnesses `by harnesses` finds and how many can be used: `9 installed: 1 logged in, 2 login unknown, 5 login not verified, 1 logged out` | none installed, or a harness off `PATH` or at its usage limit | |
| `harnesses.ID` | One row for each installed harness, from the same cache: `VERSION · LOGIN · LIVE RUN` in at most 120 characters (below) | off `PATH`, or at its usage limit | |
| `harnesses.default` | `[defaults] harness`, or `claude-code` (what `by run` starts) when unset, is installed, and its login is not known to be missing | unset and `claude-code` is not installed; or it is installed and its login is not verified, or it is logged out | set to a harness that is not installed |
| `hooks.ID` | One row for each harness on `PATH` whose Branchyard hooks are installed (`by init plugin --status` is not `missing`) or were heard: whether a session of it called `by hook` or `by statusline`, from the notes those keep beside the user configuration ([heard](harness-hooks.md#heard-did-the-harness-run-it)). `ok`: `install current · heard 3m ago (session-start) in /home/me/app, as SessionStart`. `todo`: `install current · never heard`, or heard only before `by setup` last wrote the hooks; the fix is to start a new session of the harness (or to put `by` on `PATH`, when it is not) | | |
| `harnesses.credentials` | Which of the usual credential variables are set (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `DEEPSEEK_API_KEY`, `GH_TOKEN` and the rest of the list `by init` detects), by name only; and each `[secrets]` entry's source (a variable, or an `@file`) | a `[secrets]` entry whose variable is unset or file is missing | |
| `provider.default` | `[defaults] provider`. Local always passes. Microsandbox goes through the engine's own check (a build with the `microsandbox` feature, an image). A recipe must be defined and trusted | a recipe that is not trusted as it is now; or the check could not run without the store | Microsandbox the engine refuses; a recipe that is not defined |
| `provider.sandboxes` | With Microsandbox as the default: the kept sandboxes and snapshots the store holds, the oldest kept one's age, and any older than `[microsandbox] max_kept_minutes` | a kept sandbox past `max_kept_minutes`: nothing destroys it until a turn ends on the provider or its branch takes its next turn; `by rm BRANCH` destroys it now | the rows cannot be read |
| `host.namespaces` | Whether this host can confine a local harness to the egress proxy in an unprivileged user and network namespace ([egress](egress.md)): the engine's own probe | it cannot (a kernel or security-module setting, `BRANCHYARD_EGRESS_NETNS=off`, or not Linux): a network policy would be advisory | `[network] enforce = "required"` with an allowlist, on a host that cannot: every branch with that policy is refused |
| `host.landlock` | Whether this host can hold a local harness to its permission preset's write scope with Landlock ([file guard](modes.md#the-file-guard)), and the ABI: the engine's own probe, a restricted `/bin/sh` refused a write | it cannot (a kernel before 5.13 or without Landlock enabled, `BRANCHYARD_GUARD=off`, or not Linux): `read-only` and `edit-worktree` then decide only the tool calls a harness asks about | |
| `host.loopback` | A listener can be opened on `127.0.0.1`, which the egress proxy and the gateways use | it cannot | |
| `host.kvm` | Read-write access to `/dev/kvm`, which `--provider microsandbox` needs | Linux without it | |
| `server.reachable` | A server named by `--remote`, `BRANCHYARD_REMOTE` or `[remote] url` answers `GET /v1/repos` with the token in the token file, and serves the `repo` named, if one is | | no token file, an unreadable token or CA file, no answer within 5 seconds, a refusal, or the repository named is not served |
| `serve.config` | With `[serve] config`: the server file it names (relative to the repository) validates, as `by serve --check` reads it. No row without `[serve]` | it validates with warnings (a TLS file that does not exist yet, plain HTTP beyond loopback, a provider this build cannot run) | the file does not load or validate, or names a `database` on a build without the `postgres` feature |
| `serve.database` | With `[serve] config`: which store the file names, SQLite in its data directory or PostgreSQL (host and database, never the user or password), and whether this build can open it. A name, not a connection | the file does not load, so the store cannot be named | PostgreSQL on a build without the `postgres` feature: `by serve` would refuse to start |
| `models.keys` | With `[models]`: each backend's key, as the gateway reads it (an entry of `[secrets]`, else a variable of that name), is set, by name | a backend that needs a key names none, or `[models]` names no backend | a key variable that is unset, or a key file that is missing |
| `models.prices` | With `[[models.routes]]`: each route that names one model has a price on every backend the gateway may send it to (its weighted backends and fallbacks), as the model that backend is asked for: the route's, or its `backend_model` where the call is translated; from `[models.prices]` or the built-in catalog | a routed model nothing prices on a backend it may reach: those calls are recorded unpriced, and a dollar budget cannot count them | |

A repository with no `.branchyard/` is a `todo` (`not set up yet`, with `by setup` as the fix), not a `fail`: nothing is broken, it has not been set up. Any other `by` command in the repository, such as `by ls`, also creates `.branchyard/`.

**The harness rows.** A harness row reads like `2.1.310 · login not verified · live 9/9`, three parts:

- the version, or `slow to start` when its version command did not answer in time ([harness lifecycle](harness-lifecycle.md#how-detection-works)); it reads `installed X` when the version a live run was on is a different one, since a run says nothing about another version;
- the login, in the words `by harnesses` uses (`logged in`, `key set (VARIABLE)`, `key stored (VARIABLE)`, `credentials found`, `logged out`, `login not verified`, `unknown`; [login states](harness-lifecycle.md#login-states));
- the live run: `live 9/9` for a recorded session of the harness itself, `live-tested Y (9/9)` beside `installed X` when it was on another version, `no live run` for none.

The row is `ok` for a harness that can be used, including one whose login cannot be read. `todo` is a login still to make (`login not verified`, or `logged out` by the harness's own command), with `by harnesses login ID` as the fix, the same command `by up` gives. `warn` is a harness found off `PATH` or at its usage limit. A login that is only not verified is never called "cannot run". `harnesses.default` says the same about the default harness: `warn` with that command when its login is not verified.

A route whose model is a pattern (`claude-*`) is priced per model when a call is made; the row names such patterns as not checked. The Substrate provider is chosen by flags, never by a file, so there is nothing configured for doctor to check; no cheap reachability check for it exists.

## An example

A repository `by` has been used in, on a machine with two harnesses, one of them not logged in to the default:

```
Mode: local, no isolation · SQLite (.branchyard/state.db) · no server

STATUS  CHECK                   DETAIL
ok      repository.git          /home/me/app (on branch main)
ok      repository.state        /home/me/app/.branchyard opens; 3 branches
ok      repository.schema       schema 2, the version this binary reads
ok      repository.services     no registry yet: nothing has registered a service
ok      config.user             no user file at /home/me/.config/branchyard/config.toml (optional)
ok      config.project          /home/me/app/branchyard.toml loads and validates
ok      config.merged           the files merge with the BRANCHYARD_* variables
warn    config.workspace_trust  [workspace] in /home/me/app/branchyard.toml has scripts you have not trusted (digest 3f9a1c07b2e4); new branches are refused until you do
ok      harnesses.summary       2 installed: 1 logged in, 1 login not verified
todo    harnesses.claude-code   2.1.310 · login not verified · live 9/9
ok      harnesses.codex         installed 0.164.0 · logged in · live-tested 0.157.1 (9/9)
warn    harnesses.default       not set: `by run` without --harness uses claude-code, which is installed, but its login is not verified
ok      harnesses.credentials   set: GH_TOKEN (names only)
ok      provider.default        local (the default): harnesses run as your user in a worktree, with no isolation
ok      provider.sandboxes      no sandbox provider is configured, so no sandbox is kept
ok      host.namespaces         unprivileged user and network namespaces work: a local harness can be confined to the egress proxy
ok      host.landlock           Landlock ABI 7: under --permissions read-only or edit-worktree a local harness can write only in its preset's scope
ok      host.loopback           a listener on 127.0.0.1 can be opened
warn    host.kvm                no read-write access to /dev/kvm: `--provider microsandbox` cannot start a microVM here
ok      server.reachable        not configured: no --remote, BRANCHYARD_REMOTE or [remote] url; commands run here
ok      models.keys             not configured: no [models]
ok      models.prices           not configured: no [models]

To do:
  config.workspace_trust: review it with `by workspace show`, then run `by workspace trust`
  harnesses.claude-code: run `by harnesses login claude-code`, or set ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN
  harnesses.default: run `by harnesses login claude-code`, or set ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN
  host.kvm: load the kvm module and add your user to the kvm group; local branches need none

warn: 3 warnings, 1 to do, 18 ok
```

The exit status is 0. Where a row would fail, the summary reads `fail: 1 failed, 2 warnings, 1 to do, 17 ok` and the status is 1. The table never cuts a detail, so a configuration refusal is shown whole.

In a repository `by` has not been used in, the same report says:

```
Mode: local, no isolation · SQLite (.branchyard/state.db) · no server

STATUS  CHECK             DETAIL
ok      repository.git    /home/me/app (on branch main)
todo    repository.state  not set up yet (no .branchyard/ in /home/me/app)
...

To do:
  repository.state: run `by setup`

todo: 1 to do, 19 ok
```

and exits 0.

`--json` prints:

```json
{
  "format": 3,
  "mode": "local, no isolation · SQLite (.branchyard/state.db) · no server",
  "checks": [
    {
      "name": "repository.git",
      "status": "ok",
      "detail": "/home/me/app (on branch main)",
      "fix": null
    },
    {
      "name": "repository.state",
      "status": "todo",
      "detail": "not set up yet (no .branchyard/ in /home/me/app)",
      "fix": "run `by setup`"
    }
  ],
  "hooks": [],
  "status": "todo"
}
```

`status` is the worst of the rows. `fix` is a string for every row that is not `ok` and `null` for every `ok`; `detail` and `fix` are one line each. (The example shows two rows; the real list has every check above, in the order above.) `hooks` has one object for each `hooks.ID` row: `harness`, `label`, `install` and `heard` (`state`: `heard`, `never` or `stale`; `kind`, `harness_event`, `at_ms`, `age_seconds`, `session`, a digest of the session id, `repository` and `installed_at_ms`).

**`format`.** The report is versioned. Format 3, this one, adds `hooks` and the `hooks.ID` rows to format 2. Format 2 differs from the unversioned report before it in four ways: the `todo` status (a repository with no `.branchyard/` was a `fail`, exit 1); `mode`; `harnesses.summary` and one `harnesses.ID` row for each installed harness in place of the single `harnesses.installed` row; and `format` itself. Rows that were there keep their names. A reader that treats any status it does not know as "not ok, not fail" reads both formats; the Python SDK's `Report` ([sdk](sdk.md)) passes the strings through.

## What it reads, writes and costs

- **Offline.** The only network use is one `GET /v1/repos` to a server you configured, with a five-second deadline however the network behaves. An `ssh://` remote is not contacted (it starts its own server; `by remote ssh status` shows it).
- **Quick.** Harness detection comes from the cache `by harnesses` keeps beside the user configuration, so a run takes well under a second once it is warm; the first run, or one after the cache expires, asks every installed harness for its version at once (ten seconds at most each, as `by harnesses` does: nine real CLIs took 4.4 seconds cold). `by harnesses --refresh` refreshes the cache.
- **Changes nothing you keep.** It creates no `.branchyard/` and writes no exclude entry in a repository that has none, opens a `.branchyard/` that exists as every command does, and does not open one whose schema is older (opening would upgrade it) or newer. It runs no `[workspace]` script and no recipe. It opens a listener on loopback and starts one confined `/bin/sh -c 'exit 0'` for the namespace probe, both gone before it prints.
- **No secret.** Credentials and keys are reported by variable or file name. The server's token is sent to the server and printed nowhere.

## Options

| Option | Does |
|---|---|
| `--json` | Print the JSON above instead of the table. |
| `--path PATH` | Check the repository at or around `PATH` instead of the one around the current directory. The configuration files are looked for from `PATH` too. It is `--path` and not `--repo` because `--repo NAME` is the global option that names a repository on a server (`BRANCHYARD_REPO`). |
| `--remote`, `--token-file`, `--ca-file`, `--repo` | The global options, as for any command; here they say which server `server.reachable` asks. `by doctor` always looks at this machine and repository, with or without `--remote`; the server is one row. `[remote]` in the configuration fills what they leave unset. |

## What it does not do

- It does not repair. Each fix is a command for you to run: `by setup`, `by workspace trust`, `by services gc`, `by harnesses login ID`.
- It does not run a recipe's own doctor (`by recipe check NAME` does, and a smoke test), a harness, a model call or a sandbox. It does not reach a Microsandbox or Substrate provider.
- It does not look at `[connectors]`, `[sync]`, `[usage]`, trackers, the `gh` CLI, or the recipes other than the default provider's. Of `[serve]` it reads only whether the server file names a PostgreSQL database, for the `Mode:` line.
- The `Mode:` line is read from configuration. It connects to no database and no server, so `PostgreSQL` there means a server file names one, not that it answers.
- Not done: the harness rows were checked against fake harnesses and the nine real CLIs on this machine (no model call); no Claude Code that is really logged out was available, so the `loggedIn: false` case in the unit tests is written from the shape of the logged-in output, not recorded.
- `serve.config` and `serve.database` read the server file `[serve] config` names. They never start the server, open its stores or connect to its database: a wrong host, user or password shows when `by serve` starts.
- Nothing is checked on a server beyond `GET /v1/repos`; `by --remote URL harnesses` and `by --remote URL services` show more.

## Adding a check

A check is a function from the shared context to rows, in `crates/branchyard-cli/src/doctor_cmd/checks.rs`, listed once in `CHECKS` in `crates/branchyard-cli/src/doctor_cmd.rs`. Give each row a stable `area.thing` name, an `ok` row no fix and every other row one line saying what to do, and use the engine's or the loader's own answer rather than a second implementation of it. A check that needs the store asks the context for it (`Context::store`), which opens it at most once and not at all when opening would change it.

## What is tested

`crates/branchyard-cli/tests/doctor.rs` runs the built `by` against temporary repositories:

- a repository without `.branchyard/` is a `todo` with `by setup` as its fix and the report exits 0, and the report creates neither `.branchyard/` nor an exclude entry; the table starts with the `Mode:` line and ends with `1 to do`;
- the `Mode:` line for local, isolated, microsandbox and recipe providers, a `--remote` URL with its credentials left out, and a `[serve]` file naming PostgreSQL (the URL never shown); and `not known` when the configuration does not load;
- the harness rows with fake harnesses on a temporary `PATH` and a home of its own: a summary and one row of at most 120 characters for each, the version beside the live-tested one, a login not verified as a `todo` that never says "cannot run", a login the harness's own command says is missing as a `todo`, and the default harness agreeing with its row;
- a harness with hooks installed: no `hooks.ID` row before `by init plugin codex --apply`, then a `todo` `never heard`, then `ok` `heard … (session-start) in REPO, as SessionStart` after a hook call with a Codex payload, with `hooks` in the JSON and no session id in the output;
- an initialized repository has no failure;
- a broken `branchyard.toml` and a broken user file each fail naming the file and line, in JSON and in the table, and what depends on them says `not checked`;
- untrusted `[workspace]` scripts warn, are `ok` after `by workspace trust`, warn again naming both digests after a change, and copy-only sections need no trust;
- a store at an older schema is reported and left at it, a newer one fails, and a file that is not a database fails naming it;
- a missing git repository (and a `--path` that is not a directory) fails, and `--path` checks another repository from outside it;
- the JSON has exactly the documented fields, unique dotted names, a fix on every row that is not `ok`, and the worst row as its status; the exit status is 0 with warnings only, 1 with a failure, and 2 for a usage error;
- credentials appear by name and their values are in no output, stdout or stderr;
- a provider that cannot run, an undefined or untrusted recipe, a default harness that is not installed, and `enforce = "required"` on a host that cannot confine (made deterministic with `BRANCHYARD_EGRESS_NETNS=off`);
- `host.landlock` naming the Landlock ABI where the host has it, and warning with `BRANCHYARD_GUARD=off` (`crates/branchyard-cli/tests/guard.rs`);
- a configured server asked once with its token against a mock, a refused token, a missing token file, a repository it does not serve, a server that stopped, and an `ssh://` remote left alone;
- the model gateway's key variables named and never shown, and an unpriced routed model.

Unit tests cover the rendering (the table never cuts a detail, the summary with to-dos, the mode line, the JSON shape, colour on the status only), each check's pure part (stale services, sandbox age against `max_kept_minutes`, the harness summary and rows and the default-harness row, a server URL without its credentials, `[secrets]` sources, key rows, trust rows), and a request to a server that hangs ending at its deadline. The engine's read-only schema reader, its sandbox listing and its price lookup are tested in `crates/branchyard/src/doctor.rs`.
