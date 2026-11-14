# Doctor

`by doctor` checks whether your machine and repository are ready to run Branchyard. It runs a standard suite of checks and prints a table with one row per check: a status (`ok`, `todo`, `warn`, or `fail`), a one-line summary of what it found, and a one-line remediation step for any row that is not `ok`. Above the table, a `Mode:` line shows how the repository is configured to run. `by doctor` is strictly read-only: it modifies nothing and never prints secrets.

```sh
by doctor                    # show the mode, check table, and next steps
by doctor --json             # output JSON for scripts or CI
by doctor --path ~/src/app   # check a specific repository path
by doctor --refresh          # re-probe harnesses instead of using the 1-minute cache
```

`by doctor` uses the same underlying checks as the rest of the CLI: the configuration loaders from `by config validate`, the trust state from `by workspace trust`, the harness inventory from `by harnesses` (cached for a minute unless you pass `--refresh`), the engine's sandbox provider check, the service registry from `by services`, and the remote client used by `--remote`. If a row reports a problem, any command that depends on that component will fail for the same reason.

While `by doctor` only inspects your setup, `by up` ([up](up.md)) takes action: it installs configured harnesses (when allowed by your user configuration), starts the connector gateway, and lists any remaining manual steps. Run `by doctor` to inspect your environment, and run `by up` to bring it ready.

When Substrate is configured as the provider or a remote server is set (`--remote`, `BRANCHYARD_REMOTE`, or `[remote] url`), `by doctor` appends the seven [readiness checks](up.md#readiness-checks) below the main table (and under `readiness` in `--json` output). Those readiness rows reuse the same network responses as `provider.default` and `server.reachable`. Against a remote server, `by doctor` queries the repository list and problem list, so worker-specific checks (installed harnesses, capacity, and logins) are reported as `unknown` with `by setup --up` recommended as the next step. `by doctor` never queries worker inventories or starts test actors; use `by up --plan --smoke` or `by substrate check --smoke` for an active smoke test.

## Statuses and exit codes

| Status | Means |
|---|---|
| `ok` | Ready to use, or the feature being checked is not configured (for example, `server.reachable: not configured`). |
| `todo` | An expected setup step has not been run yet (for example, `by setup` has not been run in this repository, or an installed harness is not logged in). Nothing is broken. |
| `warn` | Worth reviewing, or a check could not run because an earlier check failed. Does not block basic commands. |
| `fail` | A required component is misconfigured or unreachable; commands that depend on it will fail. |

`todo` is distinct from `warn` so that scripts and CI jobs watching for warnings do not flag a brand-new checkout. Severity increases in the order `ok`, `todo`, `warn`, `fail`, and the overall report `status` in `--json` matches the most severe row.

If a check cannot run because a prerequisite check failed, it is marked `warn` with a detail line starting with `not checked:` and a fix pointing to the prerequisite (such as `fix repository.git first`). It is never marked `ok`. In text output, those skipped checks are omitted from the *To do* summary at the bottom so you only see the root cause to fix first.

| Exit | When |
|---|---|
| 0 | No row failed (the overall status is `ok`, `todo`, or `warn`). A fresh repository exits 0. |
| 1 | At least one row failed. |
| 2 | Command-line usage error (for example, `by doctor --nonsense`). |

Even if the target path is not a Git repository (`repository.git` fails), `by doctor` still prints the full report to standard output and exits with code `1`.

## The mode

The header line above the table summarizes how the repository is configured to run based on your merged configuration (user config, `branchyard.toml`, `BRANCHYARD_*` environment variables, and `--remote`):

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
| `harnesses.credentials` | Which of the usual credential variables are set (`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `DEEPSEEK_API_KEY`, `ZAI_API_KEY`, `GH_TOKEN` and the rest of the list `by init` detects), by name only; each `[secrets]` entry's source (a variable, or an `@file`); and the [provider logins](harness-lifecycle.md#through-a-gateway-provider) of `[logins]` (`claude-code via zai (ZAI_API_KEY)`) | a `[secrets]` entry whose variable is unset or file is missing; a `[logins]` entry whose provider key `[secrets]` does not name (the fix is `by harnesses login ID --provider NAME`) | |
| `harnesses.google_cloud` | Only when a Vertex AI variable (`CLAUDE_CODE_USE_VERTEX`, `ANTHROPIC_VERTEX_PROJECT_ID`, `CLOUD_ML_REGION`, `GOOGLE_GENAI_USE_VERTEXAI`, `GOOGLE_CLOUD_PROJECT`, `GOOGLE_CLOUD_LOCATION`, `GCP_PROJECT`, `GCLOUD_PROJECT`, `AGY_ADC_AUTH`, `VERTEX_PROJECT_ID`, `VERTEX_REGION`), `GOOGLE_APPLICATION_CREDENTIALS` or gcloud's `application_default_credentials.json` is there: where [Application Default Credentials](setup.md#google-cloud) come from, in the order Google's libraries look (the file `GOOGLE_APPLICATION_CREDENTIALS` names, gcloud's file under `CLOUDSDK_CONFIG` or `~/.config/gcloud` (with the `[secrets] GCLOUD_ADC` line that carries it into isolated and sandboxed branches), then a Google Compute Engine VM's metadata server, told by its DMI product name, its scopes not checked), and which of those variables are set, by name only. A `todo` when a harness is switched to Vertex AI or ADC (`CLAUDE_CODE_USE_VERTEX` true, `GOOGLE_GENAI_USE_VERTEXAI=true`, `AGY_ADC_AUTH=true`, or a project OpenCode or Hermes reads) and none is found: the fix is `gcloud auth application-default login` | `GOOGLE_APPLICATION_CREDENTIALS` names a file that does not exist | |
| `provider.default` | `[defaults] provider`. Local always passes. Microsandbox goes through the engine's own check (a build with the `microsandbox` feature, an image). Substrate: the bridge key and API token file are readable, then one authenticated `GetActorTemplate` to Control, then the template's bridge contract and verifying key (no actor). A recipe must be defined and trusted | a recipe that is not trusted as it is now; or the check could not run without the store | Microsandbox the engine refuses; a Substrate stage that did not pass (an unreadable key or token file, no answer, a refused token, a key mismatch, a template it cannot use); a recipe that is not defined |
| `provider.sandboxes` | With Microsandbox as the default: the kept sandboxes and snapshots the store holds, the oldest kept one's age, and any older than `[microsandbox] max_kept_minutes` | a kept sandbox past `max_kept_minutes`: nothing destroys it until a turn ends on the provider or its branch takes its next turn; `by rm BRANCH` destroys it now | the rows cannot be read |
| `host.namespaces` | Whether this host can confine a local harness to the egress proxy in an unprivileged user and network namespace ([egress](egress.md)): the engine's own probe | it cannot (a kernel or security-module setting, `BRANCHYARD_EGRESS_NETNS=off`, or not Linux): a network policy would be advisory | `[network] enforce = "required"` with an allowlist, on a host that cannot: every branch with that policy is refused |
| `host.landlock` | Whether this host can hold a local harness to its permission preset's write scope with Landlock ([file guard](modes.md#the-file-guard)), and the ABI: the engine's own probe, a restricted `/bin/sh` refused a write | it cannot (a kernel before 5.13 or without Landlock enabled, `BRANCHYARD_GUARD=off`, or not Linux): `read-only` and `edit-worktree` then decide only the tool calls a harness asks about | |
| `host.loopback` | A listener can be opened on `127.0.0.1`, which the egress proxy and the gateways use | it cannot | |
| `host.kvm` | Read-write access to `/dev/kvm`, which `--provider microsandbox` needs | Linux without it | |
| `server.reachable` | A server named by `--remote`, `BRANCHYARD_REMOTE` or `[remote] url` answers `GET /v1/repos` with the token in the token file, and serves the `repo` named, if one is | | no token file, an unreadable token or CA file, no answer within 5 seconds, a refusal, or the repository named is not served |
| `server.problems` | With a server `server.reachable` asks: what it lists at `GET /v1/problems` ([problems](server.md#problems)), the things its background work could not do that a person can act on, asked once with a five-second deadline after `GET /v1/repos` succeeded. `ok`: `no problems`. No row without a server or with an `ssh://` remote | it lists any: the count and the oldest entry's `what` and `error` (`1 problem: trigger nightly-triage: paused after 3 failed runs in a row`), with that entry's `next` as the fix; a server older than the route answers `404`, and the fix is to upgrade it; no answer or an error from the route; `not checked` when `server.reachable` got no answer it accepted, so one outage is one failure | |
| `serve.config` | With `[serve] config`: the server file it names (relative to the repository) validates, as `by serve --check` reads it. No row without `[serve]` | it validates with warnings (a TLS file that does not exist yet, plain HTTP beyond loopback, a provider this build cannot run) | the file does not load or validate, or names a `database` on a build without the `postgres` feature |
| `serve.database` | With `[serve] config`: which store the file names, SQLite in its data directory or PostgreSQL (host and database, never the user or password), and whether this build can open it. A name, not a connection | the file does not load, so the store cannot be named | PostgreSQL on a build without the `postgres` feature: `by serve` would refuse to start |
| `models.keys` | With `[models]`: each backend's key, as the gateway reads it (an entry of `[secrets]`, else a variable of that name), is set, by name. An AIPlex backend whose key is read from a variable with a provider's name (`$ANTHROPIC_API_KEY`) is `ok` and says so: its value is sent to that backend and to nothing else, and it is hidden from every harness | a backend that needs a key names none, or `[models]` names no backend | a key variable that is unset, or a key file that is missing; or a backend with `auth = "turn-token"`, which a repository's own (local) yard cannot use: every turn on the gateway there is refused, and the fix is a `key` |
| `models.url` | Only when a `[models]` backend is an AIPlex access point (`control_plane = "aiplex"`, [model gateway](model-gateway.md#configuring-it)): each such backend's `url`, where its credential is sent. One row for all of them. `ok`: `every control-plane backend's url is https, or http on this machine (aiplex-anthropic, aiplex-openai)` | a `url` that is plain `http://` on a host that is not loopback, each backend named: the credential travels unencrypted unless the network between Branchyard and AIPlex encrypts it. The fix is an `https://` url, or making sure the network does | |
| `models.audience` | Only when a `[models]` backend is called with a token Branchyard signs (`auth = "turn-token"`, [model gateway](model-gateway.md#a-token-branchyard-signs)): each such backend's `audience` against its `url`, where the token is sent. One row for all of them. `ok`: `no signed token's audience names another host than its backend's url (aiplex-anthropic)`; an audience that is not an `http(s)` URL names no host and is not compared, and ports are not compared | an `audience` that is an `http(s)` URL on another host than the backend's `url`, each backend named: whoever is at the `url` is sent a token that is good at the audience for up to an hour. The fix is to make the `url` the audience's AIPlex access point, or the audience the AIPlex the `url` names | |
| `models.prices` | With `[[models.routes]]`: each route that names one model has a price on every backend the gateway may send it to (its weighted backends and fallbacks), as the model that backend is asked for: the route's, or its `backend_model` where the call is translated; from `[models.prices]` or the built-in catalog | a routed model nothing prices on a backend it may reach: those calls are recorded unpriced, and a dollar budget cannot count them | |

A repository with no `.branchyard/` is a `todo` (`not set up yet`, with `by setup` as the fix), not a `fail`: nothing is broken, it has not been set up. Any other `by` command in the repository, such as `by ls`, also creates `.branchyard/`.

**The harness rows.** A harness row reads like `2.1.310 · login not verified · live 9/9`, three parts:

- the version, or `slow to start` when its version command did not answer in time ([harness lifecycle](harness-lifecycle.md#how-detection-works)); it reads `installed X` when the version a live run was on is a different one, since a run says nothing about another version;
- the login, in the words `by harnesses` uses (`logged in`, `key set (VARIABLE)`, `key stored (VARIABLE)`, `credentials found`, `logged out`, `login not verified`, `unknown`; [login states](harness-lifecycle.md#login-states));
- the live run: `live 9/9` for a recorded session of the harness itself, `live-tested Y (9/9)` beside `installed X` when it was on another version, `no live run` for none.

The row is `ok` for a harness that can be used, including one whose login cannot be read. `todo` is a login still to make (`login not verified`, or `logged out` by the harness's own command), with `by harnesses login ID` as the fix, the same command `by up` gives. `warn` is a harness found off `PATH` or at its usage limit. A login that is only not verified is never called "cannot run". `harnesses.default` says the same about the default harness: `warn` with that command when its login is not verified.

A route whose model is a pattern (`claude-*`) is priced per model when a call is made; the row names such patterns as not checked. A saved Substrate connection is checked through Control as `provider.default` says; `by substrate check --smoke` creates and deletes one disposable actor to prove bridge execution, without a model. See [Substrate setup](substrate-setup.md).

## An example

A repository `by` has been used in, on a machine with two harnesses, one of them not logged in to the default:

```
Mode: local, no isolation · SQLite (.branchyard/state.db) · no server

STATUS  CHECK                   DETAIL
ok      repository.git          /home/me/app (on branch main)
ok      repository.state        /home/me/app/.branchyard opens; 3 branches
ok      repository.schema       schema 3, the version this binary reads
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
  harnesses.claude-code: run `by harnesses login claude-code`, or set ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN or CLAUDE_CODE_OAUTH_TOKEN
  harnesses.default: run `by harnesses login claude-code`, or set ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN or CLAUDE_CODE_OAUTH_TOKEN
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
  "format": 4,
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

**`format`.** The report is versioned. Format 4, this one, adds `readiness` to format 3: the seven [readiness checks](up.md#readiness-checks) (`id`, `label`, `status`, `detail`, `next`), present only when Substrate or a server is configured. Format 3 adds `hooks` and the `hooks.ID` rows to format 2. Format 2 differs from the unversioned report before it in four ways: the `todo` status (a repository with no `.branchyard/` was a `fail`, exit 1); `mode`; `harnesses.summary` and one `harnesses.ID` row for each installed harness in place of the single `harnesses.installed` row; and `format` itself. Rows that were there keep their names. A reader that treats any status it does not know as "not ok, not fail" reads both formats; the Python SDK's `Report` ([sdk](sdk.md)) passes the strings through.

## What it reads, writes and costs

- **Offline, except what you configured.** The network use is one `GET /v1/repos` to a server you configured and, when that succeeds, one `GET /v1/problems`, each with a five-second deadline however the network behaves, and, with Substrate as the provider, one authenticated `GetActorTemplate` to its Control endpoint, bounded at 30 seconds. An `ssh://` remote is not contacted (it starts its own server; `by remote ssh status` shows it).
- **Quick.** Harness detection comes from the cache `by harnesses` keeps beside the user configuration, so a run takes well under a second once it is warm; the first run, or one after the cache expires, asks every installed harness for its version at once (ten seconds at most each, as `by harnesses` does: nine real CLIs took 4.4 seconds cold). `by harnesses --refresh` refreshes the cache.
- **Changes nothing you keep.** It creates no `.branchyard/` and writes no exclude entry in a repository that has none, opens a `.branchyard/` that exists as every command does, and does not open one whose schema is older (opening would upgrade it) or newer. It runs no `[workspace]` script and no recipe. It opens a listener on loopback and starts one confined `/bin/sh -c 'exit 0'` for the namespace probe, both gone before it prints.
- **No secret.** Credentials and keys are reported by variable or file name. The server's token is sent to the server and printed nowhere.

## Options

| Option | Does |
|---|---|
| `--json` | Print the JSON above instead of the table. |
| `--path PATH` | Check the repository at or around `PATH` instead of the one around the current directory. The configuration files are looked for from `PATH` too. It is `--path` and not `--repo` because `--repo NAME` is the global option that names a repository on a server (`BRANCHYARD_REPO`). |
| `--remote`, `--token-file`, `--ca-file`, `--repo` | The global options, as for any command; here they say which server `server.reachable` and `server.problems` ask. `by doctor` always looks at this machine and repository, with or without `--remote`; the server is one row. `[remote]` in the configuration fills what they leave unset. |

## What it does not do

- It does not repair. Each fix is a command for you to run: `by setup`, `by workspace trust`, `by services gc`, `by harnesses login ID`.
- It does not run a recipe's own doctor (`by recipe check NAME` does, and a smoke test), a harness, a model call or a sandbox. It starts no Microsandbox VM and no Substrate actor, and lists no Substrate workers; its one Substrate call reads the template.
- It does not look at `[connectors]`, `[sync]`, `[usage]`, trackers, the `gh` CLI, or the recipes other than the default provider's. Of `[serve]` it reads only whether the server file names a PostgreSQL database, for the `Mode:` line.
- The `Mode:` line is read from configuration. It connects to no database and no server, so `PostgreSQL` there means a server file names one, not that it answers.
- Not done: the harness rows were checked against fake harnesses and the nine real CLIs on this machine (no model call); no Claude Code that is really logged out was available, so the `loggedIn: false` case in the unit tests is written from the shape of the logged-in output, not recorded.
- `serve.config` and `serve.database` read the server file `[serve] config` names. They never start the server, open its stores or connect to its database: a wrong host, user or password shows when `by serve` starts.
- Nothing is checked on a server beyond `GET /v1/repos` and `GET /v1/problems`; `by --remote URL harnesses` and `by --remote URL services` show more.

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
- a configured server asked once with its token against a mock, a refused token, a missing token file, a repository it does not serve, a server that stopped, and an `ssh://` remote left alone, with `readiness` saying in each case whether the credentials or the control plane failed, from the same single request;
- `server.problems` against the same mock: one `GET /v1/problems` after `GET /v1/repos`, a listed problem as a warning naming it with its `next` as the fix, `not checked` and no request after a refused token or a stopped server, a server answering `404` for the route told to upgrade, and no row without a server or for an `ssh://` remote;
- Substrate with a missing bridge key: `readiness` fails the operator credentials and leaves the control plane, capacity and installed checks `unknown`; against the fake cluster (`crates/branchyard-cli/tests/substrate.rs`) the control plane `verified`, capacity `unknown`, and no actor created;
- the model gateway's key variables named and never shown, and an unpriced routed model.

Unit tests cover the rendering (the table never cuts a detail, the summary with to-dos, the mode line, the JSON shape, colour on the status only), each check's pure part (stale services, sandbox age against `max_kept_minutes`, the harness summary and rows and the default-harness row, a server URL without its credentials, `[secrets]` sources, key rows, trust rows, the `server.problems` row from each answer), and a request to a server that hangs ending at its deadline. The engine's read-only schema reader, its sandbox listing and its price lookup are tested in `crates/branchyard/src/doctor.rs`.
