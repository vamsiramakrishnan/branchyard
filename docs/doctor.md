# Doctor

`by doctor` answers one question: is this machine, and this repository, ready to use Branchyard? It runs a fixed list of checks and prints one row for each: a status (`ok`, `warn` or `fail`), what it found in one line, and, for every row that is not `ok`, what to do in one line. It reads; it repairs nothing, and it prints no secret.

```sh
by doctor                    # a table, then what to do about each row that is not ok
by doctor --json             # the same as JSON, for a script or a CI step
by doctor --path ~/src/app   # check that repository instead of the one around here
```

The checks reuse what `by` already knows rather than looking again: the loaders `by config validate` runs, the trust decision `by workspace trust` records, the inventory `by harnesses` shows, the engine's own provider check, the service registry `by services` lists, and the client `--remote` uses. When a row says something is wrong, a command that depends on it would say the same.

## Statuses and exit codes

| Status | Means |
|---|---|
| `ok` | Nothing to do. Also what a check says when the thing it checks is not configured (`server.reachable: not configured`). |
| `warn` | Worth looking at, or a check that could not run. Nothing that stops a command working. |
| `fail` | A command that needs this would fail. |

A check that cannot run because another failed is a `warn` whose detail begins `not checked:` and whose fix names the check to fix first (`fix repository.git first`). It is never an `ok`: the report does not claim what it did not look at. The text output leaves these rows out of its *To do* list, since the failed row above them says what to do.

| Exit | When |
|---|---|
| 0 | No row failed. Warnings do not fail. |
| 1 | At least one row failed. |
| 2 | A usage error, as for every `by` command (`by doctor --nonsense`). |

There is no separate status for "doctor itself could not run". A problem with finding something out is a row (`repository.git` fails when `--path` is not a directory), and the rest of the report still prints. The report is on standard output.

## The checks

Rows come in this order, and their names are stable: a script may match on them.

| Check | What it looks at | `warn` | `fail` |
|---|---|---|---|
| `repository.git` | The repository around the current directory (`git rev-parse --show-toplevel`), or `--path`, or `BRANCHYARD_ROOT` inside a harness; its branch | no commit yet: branches start from one | not in a repository, `--path` is not a directory, or git cannot run |
| `repository.state` | `.branchyard/` exists and its store opens, by `Yard::open`, the open every `by` command makes (it also recovers a branch whose engine stopped) | not opened on purpose (see `repository.schema`) | no `.branchyard/`: Branchyard has not been used here, with the hint to run `by init`; or the store does not open |
| `repository.schema` | The schema version recorded in `.branchyard/state.db` against the one this binary reads, read through a read-only connection | older: the next `by` command that opens the store upgrades it in place, and an older `by` cannot read it after | newer than this binary, a version it does not read, or a file that is not a readable database |
| `repository.services` | `.branchyard/registry.db`, the registry `by services` shows | a record is expired or reclaiming, or live with a lease that ran out: `by services gc` reclaims it | the registry cannot be read |
| `config.user` | The user file (`BRANCHYARD_USER_CONFIG`, else `~/.config/branchyard/config.toml`) loads and validates, strictly, as `by config validate` loads it | | the loader refused it; the row shows its message, which names the file and the line |
| `config.project` | `branchyard.toml` at or above the repository, the same way | | the same |
| `config.merged` | The two files merged with the `BRANCHYARD_*` variables that override `[remote]`, as `by config show` merges them | a file above did not load | the merge was refused |
| `config.workspace_trust` | `[workspace]` scripts: whether they are trusted, as they are now (the digest of the section against the one `by workspace trust` recorded) | untrusted, or changed since trusted: new branches are refused until `by workspace trust` | |
| `harnesses.installed` | What `by harnesses` detects, from the same cache: each installed harness, its version, its qualification (`branchyard::harnesses`), and whether it can run (on `PATH`, logged in, under its limit) | none installed, or none can run | |
| `harnesses.default` | `[defaults] harness`, or `claude-code` (what `by run` starts) when unset, is installed | unset and `claude-code` is not installed | set to a harness that is not installed |
| `harnesses.credentials` | Which of the usual credential variables are set (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `DEEPSEEK_API_KEY`, `GH_TOKEN` and the rest of the list `by init` detects), by name only; and each `[secrets]` entry's source (a variable, or an `@file`) | a `[secrets]` entry whose variable is unset or file is missing | |
| `provider.default` | `[defaults] provider`. Local always passes. Microsandbox goes through the engine's own check (a build with the `microsandbox` feature, an image). A recipe must be defined and trusted | a recipe that is not trusted as it is now; or the check could not run without the store | Microsandbox the engine refuses; a recipe that is not defined |
| `provider.sandboxes` | With Microsandbox as the default: the kept sandboxes and snapshots the store holds, the oldest kept one's age, and any older than `[microsandbox] max_kept_minutes` | a kept sandbox past `max_kept_minutes`: nothing destroys it until a turn ends on the provider or its branch takes its next turn; `by rm BRANCH` destroys it now | the rows cannot be read |
| `host.namespaces` | Whether this host can confine a local harness to the egress proxy in an unprivileged user and network namespace ([egress](egress.md)): the engine's own probe | it cannot (a kernel or security-module setting, `BRANCHYARD_EGRESS_NETNS=off`, or not Linux): a network policy would be advisory | `[network] enforce = "required"` with an allowlist, on a host that cannot: every branch with that policy is refused |
| `host.loopback` | A listener can be opened on `127.0.0.1`, which the egress proxy and the gateways use | it cannot | |
| `host.kvm` | Read-write access to `/dev/kvm`, which `--provider microsandbox` needs | Linux without it | |
| `server.reachable` | A server named by `--remote`, `BRANCHYARD_REMOTE` or `[remote] url` answers `GET /v1/repos` with the token in the token file, and serves the `repo` named, if one is | | no token file, an unreadable token or CA file, no answer within 5 seconds, a refusal, or the repository named is not served |
| `models.keys` | With `[models]`: each backend's key, as the gateway reads it (an entry of `[secrets]`, else a variable of that name), is set, by name | a backend that needs a key names none, or `[models]` names no backend | a key variable that is unset, or a key file that is missing |
| `models.prices` | With `[[models.routes]]`: each route that names one model has a price on every backend the gateway may send it to (its weighted backends and fallbacks), as the model that backend is asked for: the route's, or its `backend_model` where the call is translated; from `[models.prices]` or the built-in catalog | a routed model nothing prices on a backend it may reach: those calls are recorded unpriced, and a dollar budget cannot count them | |

A route whose model is a pattern (`claude-*`) is priced per model when a call is made; the row names such patterns as not checked. The Substrate provider is chosen by flags, never by a file, so there is nothing configured for doctor to check; no cheap reachability check for it exists.

## An example

A repository `by` has been used in, on a machine whose one harness is logged out:

```
STATUS  CHECK                   DETAIL
ok      repository.git          /home/me/app (on branch main)
ok      repository.state        /home/me/app/.branchyard opens; 3 branches
ok      repository.schema       schema 2, the version this binary reads
ok      repository.services     no registry yet: nothing has registered a service
ok      config.user             no user file at /home/me/.config/branchyard/config.toml (optional)
ok      config.project          /home/me/app/branchyard.toml loads and validates
ok      config.merged           the files merge with the BRANCHYARD_* variables
warn    config.workspace_trust  [workspace] in /home/me/app/branchyard.toml has scripts you have not trusted (digest 3f9a1c07b2e4); new branches are refused until you do
warn    harnesses.installed     claude-code 2.1.296 [qualified 9/9 on 2.1.283 (Claude Code)] - cannot run: claude-code is likely logged out (no ~/.claude/.credentials.json and no ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN); run by harnesses login claude-code
ok      harnesses.default       not set: `by run` without --harness uses claude-code, which is installed
ok      harnesses.credentials   set: GH_TOKEN (names only)
ok      provider.default        local (the default): harnesses run as your user in a worktree, with no isolation
ok      provider.sandboxes      no sandbox provider is configured, so no sandbox is kept
ok      host.namespaces         unprivileged user and network namespaces work: a local harness can be confined to the egress proxy
ok      host.loopback           a listener on 127.0.0.1 can be opened
warn    host.kvm                no read-write access to /dev/kvm: `--provider microsandbox` cannot start a microVM here
ok      server.reachable        not configured: no --remote, BRANCHYARD_REMOTE or [remote] url; commands run here
ok      models.keys             not configured: no [models]
ok      models.prices           not configured: no [models]

To do:
  config.workspace_trust: review it with `by workspace show`, then run `by workspace trust`
  harnesses.installed: fix what each says: `by harnesses login ID` for a logged-out harness
  host.kvm: load the kvm module and add your user to the kvm group; local branches need none

warn: 3 warnings, 16 ok
```

The exit status is 0. Where a row would fail, the summary reads `fail: 1 failed, 2 warnings, 16 ok` and the status is 1. The table never cuts a detail, so a configuration refusal is shown whole.

`--json` prints:

```json
{
  "checks": [
    {
      "name": "repository.git",
      "status": "ok",
      "detail": "/home/me/app (on branch main)",
      "fix": null
    },
    {
      "name": "repository.state",
      "status": "fail",
      "detail": "/home/me/app/.branchyard does not exist: Branchyard has not been used in this repository",
      "fix": "run `by init` to write its configuration; any `by` command in the repository, such as `by ls`, then creates .branchyard/"
    }
  ],
  "status": "fail"
}
```

`status` is the worst of the rows. `fix` is a string for every `warn` and `fail` and `null` for every `ok`; `detail` and `fix` are one line each. (The example shows two rows; the real list has every check above, in the order above.)

## What it reads, writes and costs

- **Offline.** The only network use is one `GET /v1/repos` to a server you configured, with a five-second deadline however the network behaves. An `ssh://` remote is not contacted (it starts its own server; `by remote ssh status` shows it).
- **Quick.** Harness detection comes from the cache `by harnesses` keeps beside the user configuration, so a run takes well under a second once it is warm; the first run, or one after the cache expires, asks each installed harness for its version (bounded, as `by harnesses` does). `by harnesses --refresh` refreshes the cache.
- **Changes nothing you keep.** It creates no `.branchyard/` and writes no exclude entry in a repository that has none, opens a `.branchyard/` that exists as every command does, and does not open one whose schema is older (opening would upgrade it) or newer. It runs no `[workspace]` script and no recipe. It opens a listener on loopback and starts one confined `/bin/sh -c 'exit 0'` for the namespace probe, both gone before it prints.
- **No secret.** Credentials and keys are reported by variable or file name. The server's token is sent to the server and printed nowhere.

## Options

| Option | Does |
|---|---|
| `--json` | Print the JSON above instead of the table. |
| `--path PATH` | Check the repository at or around `PATH` instead of the one around the current directory. The configuration files are looked for from `PATH` too. It is `--path` and not `--repo` because `--repo NAME` is the global option that names a repository on a server (`BRANCHYARD_REPO`). |
| `--remote`, `--token-file`, `--ca-file`, `--repo` | The global options, as for any command; here they say which server `server.reachable` asks. `by doctor` always looks at this machine and repository, with or without `--remote`; the server is one row. `[remote]` in the configuration fills what they leave unset. |

## What it does not do

- It does not repair. Each fix is a command for you to run: `by init`, `by workspace trust`, `by services gc`, `by harnesses login ID`.
- It does not run a recipe's own doctor (`by recipe check NAME` does, and a smoke test), a harness, a model call or a sandbox. It does not reach a Microsandbox or Substrate provider.
- It does not look at `[connectors]`, `[sync]`, `[serve]`, `[usage]`, trackers, the `gh` CLI, or the recipes other than the default provider's.
- Nothing is checked on a server beyond `GET /v1/repos`; `by --remote URL harnesses` and `by --remote URL services` show more.

## Adding a check

A check is a function from the shared context to rows, in `crates/branchyard-cli/src/doctor_cmd/checks.rs`, listed once in `CHECKS` in `crates/branchyard-cli/src/doctor_cmd.rs`. Give each row a stable `area.thing` name, an `ok` row no fix and every other row one line saying what to do, and use the engine's or the loader's own answer rather than a second implementation of it. A check that needs the store asks the context for it (`Context::store`), which opens it at most once and not at all when opening would change it.

## What is tested

`crates/branchyard-cli/tests/doctor.rs` runs the built `by` against temporary repositories:

- a repository without `.branchyard/` fails with the init hint, and the report creates neither `.branchyard/` nor an exclude entry;
- an initialized repository has no failure;
- a broken `branchyard.toml` and a broken user file each fail naming the file and line, in JSON and in the table, and what depends on them says `not checked`;
- untrusted `[workspace]` scripts warn, are `ok` after `by workspace trust`, warn again naming both digests after a change, and copy-only sections need no trust;
- a store at an older schema is reported and left at it, a newer one fails, and a file that is not a database fails naming it;
- a missing git repository (and a `--path` that is not a directory) fails, and `--path` checks another repository from outside it;
- the JSON has exactly the documented fields, unique dotted names, a fix on every row that is not `ok`, and the worst row as its status; the exit status is 0 with warnings only, 1 with a failure, and 2 for a usage error;
- credentials appear by name and their values are in no output, stdout or stderr;
- a provider that cannot run, an undefined or untrusted recipe, a default harness that is not installed, and `enforce = "required"` on a host that cannot confine (made deterministic with `BRANCHYARD_EGRESS_NETNS=off`);
- a configured server asked once with its token against a mock, a refused token, a missing token file, a repository it does not serve, a server that stopped, and an `ssh://` remote left alone;
- the model gateway's key variables named and never shown, and an unpriced routed model.

Unit tests cover the rendering (the table never cuts a detail, the summary, the JSON shape, colour on the status only), each check's pure part (stale services, sandbox age against `max_kept_minutes`, the harness and default-harness rows, `[secrets]` sources, key rows, trust rows), and a request to a server that hangs ending at its deadline. The engine's read-only schema reader, its sandbox listing and its price lookup are tested in `crates/branchyard/src/doctor.rs`.
