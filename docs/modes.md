# Where harnesses run and where state lives

Branchyard has five places to stand, from least to most protected. Each one adds a guarantee and costs something. Take the lowest rung that gives you what you need, and confirm you are on it with the one command in the last column.

Two questions are answered together here, because they are asked together: *where does the harness process run* (and what can it reach), and *where do the branches' records live* (and who can reach them). The first rises through the rungs; the second changes at the last two.

| Rung | You want | Set (flag, then `branchyard.toml`) | Guaranteed, and not | Costs and needs | Confirm with |
|---|---|---|---|---|---|
| [**Local**](#local) | to try Branchyard; nothing in the way | nothing. `--provider local` / `[defaults] provider = "local"` says it | Each harness in its own worktree and process group. *Not* a sandbox: it runs as you, in your `HOME`, with your login, and can read what you can | none | `by doctor`: `provider.default` is `local (the default)`; the run prints `by: local mode` |
| [**Isolated**](#isolated) | the harness kept out of your login and dotfiles | `--isolated` / `[defaults] isolated = true`; `--isolated=false` turns it off for one run | A private `HOME` at `.branchyard/homes/BRANCH`, and no `ANTHROPIC*`, `CLAUDE*`, `OPENAI*`, `CODEX*` or `BRANCHYARD_*` variables. *Not* a sandbox: still your user, your files, your network, your other variables | none; the harness is then not logged in, so give it a secret ([provisioning](provisioning.md)) | the run prints `by: isolated mode`; its `provider` line in `by show BRANCH` says `local, isolated: private HOME, not a sandbox`; `by config show` says where `isolated` came from |
| [**Sandbox provider**](#sandbox) | the harness out of your filesystem and process space | `--provider microsandbox --image REF` / `[defaults] provider = "microsandbox"` and `[microsandbox] image = "REF"`; `--provider recipe:NAME` / `provider = "recipe"` and `recipe = "NAME"`; `--provider substrate` and its `--substrate-*` flags (no file form) | A microVM, a recipe's machine or an actor per turn, with only the worktree and the variables you pass. All three are **unqualified**: tested against fakes, never on a real runtime in this repository's tests | Microsandbox: KVM, a build with `--features microsandbox` (no released binary has it), Rust 1.94, `libcap-ng`, the `msb` runtime. Recipe: your scripts and an `ssh` target. Substrate: a cluster | `by doctor`: `provider.default` and `host.kvm`; `by up --plan`: the `provider` step; `by recipe check NAME` |
| [**Shared server**](#server) | branches that outlive your terminal, or a machine other than yours | on the host: `by serve`, with `[serve] config = "FILE"` in `branchyard.toml` for its file. On the client: `--remote URL --token-file FILE` (`--repo`, `--ca-file`) / `[remote] url`, `token_file`, `ca_file`, `repo` | One store (SQLite) for branches and operations; tokens with scopes and tenants; work survives your disconnecting. *Not* isolation: the server runs harnesses as its own user, and a token with `run` can make them do anything that user can | a host you are willing to hand over; TLS off loopback; the sandbox rungs above, if you want them on the server (`--allow-provider`) | `by doctor`: `server.reachable` (client side), `serve.config` and `serve.database` (the file `[serve] config` names); `by serve --check` |
| [**PostgreSQL**](#postgres) | several servers and `by worker` processes sharing one queue | `by serve --database postgres://USER@HOST/NAME` / `"database"` in the server's file | The same operations run once, on whichever worker claims them, across restarts. Nothing about where harnesses run changes | a database; a build with `--features postgres` (the released binaries have it; `cargo install` from the README does not) | `by doctor`: `serve.database` (names the store, does not connect); `by serve --check` refuses what the build cannot open |

The first three rows are about the harness. The last two are about the store and the people reaching it, and they combine with any harness row: a PostgreSQL server can run its harnesses locally, isolated or in a sandbox.

## Which build reaches which rung

This is the part that surprises people. Which rungs you can reach depends on the binary you have.

| You have | Local | Isolated | Recipe, Substrate | Microsandbox | Server, SQLite | PostgreSQL |
|---|---|---|---|---|---|---|
| a release archive or `install.sh` (`by` and `branchyard-server`) | yes | yes | yes | **no** | yes | yes |
| the README's `cargo install --locked --path crates/branchyard-cli` | yes | yes | yes | **no** | yes | **no** |

A rung a build cannot reach is refused with the command that fixes it, and nothing is created. These are the commands, from a checkout of this repository:

```sh
cargo install --locked --path crates/branchyard-cli --features postgres       # PostgreSQL stores
cargo install --locked --path crates/branchyard-cli --features microsandbox   # microVMs (needs libcap-ng-dev, Rust 1.94)
cargo install --locked --path crates/branchyard-cli --features microsandbox,postgres   # both
cargo install --locked --path crates/branchyard-server --features postgres    # only the branchyard-server binary
```

`by init` knows what its own build can do. On a build without the SDK it does not offer Microsandbox, and says in its plan what to run if the host has KVM; on a build without PostgreSQL, `by init server` labels PostgreSQL "needs a build with --features postgres". It reads the build as two setup facts, `build.microsandbox` and `build.postgres` ([setup](setup.md)).

Running a microVM also needs the `msb` runtime at the SDK's exact version and `/dev/kvm` open to you; [sandbox providers](providers.md#microsandbox-provider) has the steps. A machine without KVM can build the feature and run its unit tests, but not a microVM.

## Local

```sh
by run "Fix the flaky parser test" --check "cargo test"
```

Local mode runs the engine in your process and each harness as a child of it, in its own git worktree under `.branchyard/` and its own process group. Branches are recorded in `.branchyard/state.db`.

**Guaranteed:** one worktree and one process group per branch, so a branch's files and processes are its own, and `by cancel` reaches the group. The environment loses the variables that mark the Claude Code session that started `by`, so a harness never runs as that session.

**Not guaranteed:** anything beyond your operating-system user. The harness has your `HOME`, your login, your other variables, your filesystem and your network. A `--yes` run lets it use all of that without asking.

Before it starts, `by` says so: `by: local mode: harnesses run as your user, in your own HOME; no sandbox`.

## Isolated

```sh
by run "Fix the flaky parser test" --isolated --secret ANTHROPIC_API_KEY
```

or, for every run in the repository:

```toml
[defaults]
isolated = true
```

`--isolated=false` turns it off for one run when the file turns it on. `--isolated=yes|no|true|false|on|off|1|0` are the accepted values; bare `--isolated` means yes.

**Guaranteed:** the harness's `HOME` is `.branchyard/homes/BRANCH`, a directory only that branch (and its forks) uses, so it sees none of your dotfiles or logins. Variables whose names start with `ANTHROPIC`, `CLAUDE`, `OPENAI`, `CODEX` or `BRANCHYARD_` are removed. Secrets you give it (`--secret`, `[secrets]`) are written 0600 into that home and nowhere else.

**Not guaranteed:** that is all it does. It is a private `HOME`, not a sandbox. The harness runs as your user: it can read your repository's parent directories, your SSH keys if it knows the path, anything on the network, and every variable that is not on that list (`GEMINI_API_KEY`, `GITHUB_TOKEN`, `AWS_*`). The harness is usually not logged in, because its login lived in the home it no longer has; `by harnesses login ID` and `[secrets]` are how it gets one.

Before it starts, `by` says: `by: isolated mode: harnesses run as your user with a private HOME under .branchyard/homes/; not a sandbox, so they can still read your files and reach the network`. Afterwards the record keeps it: `by show BRANCH` has `provider  local, isolated: private HOME, not a sandbox`, and `by ls --json`, `by show --json` and `by inspect --json` carry `"provider": "local"` and `"isolated": true`.

## Sandbox

A sandbox provider runs each turn's harness somewhere that is not your process space. Three exist; all three are **unqualified**: they run against fakes in this repository's tests (the Microsandbox mapping, a fake Substrate cluster, a recipe's scripts), and none has run a harness on a real runtime in them. See [validation](validation.md) before you rely on one.

| Provider | Flag | File | Prerequisites |
|---|---|---|---|
| Microsandbox | `--provider microsandbox --image REF [--cpus N] [--memory MIB] [--pass-env NAME,...]` | `[defaults] provider = "microsandbox"` and `[microsandbox] image`, `cpus`, `memory_mib` (or `memory`), `pass_env`, `keep` (or `keep_sandbox`), `snapshots` (or `sandbox_snapshots`) | KVM; a build with `--features microsandbox`; `msb`. An image that has the harness installed |
| Recipe | `--provider recipe:NAME` | `[defaults] provider = "recipe"` and `recipe = "NAME"`, with `[recipes.NAME]` ([recipes](recipes.md)) | Your create, exec, suspend, resume and destroy commands, trusted with `by recipe trust NAME` |
| Agent Substrate | `--provider substrate` with `--substrate-*` flags ([substrate](substrate.md)) | none: the file takes no `substrate` | A cluster and a template that runs the bridge |

```sh
by run "Fix the flaky parser test" --provider microsandbox --image ghcr.io/me/claude:1 --pass-env ANTHROPIC_API_KEY
```

**Guaranteed, when the provider works:** the harness sees the worktree (mounted or copied), a home of its own and the variables you named, and not your filesystem or your login. A kept sandbox, snapshots and live branching are options of the lifecycle ([sandbox snapshots](sandbox-snapshots.md)).

**Not guaranteed:** that the provider is sound. They are unqualified. Network egress policy is enforced only for local branches on Linux with user namespaces; sandboxes declare none ([egress](egress.md)). A sandbox that cannot reach the host runs a delegating turn without the delegation tools, and the turn says so.

Before it starts, `by` says: `by: sandbox mode: harnesses run in Microsandbox microVMs from IMAGE; the worktree is mounted at /workspace`. On a build without the SDK the same command stops before creating anything:

```text
by: unsupported: this build has no Microsandbox support; rebuild from a checkout with `cargo install --locked --path crates/branchyard-cli --features microsandbox` (it needs Rust 1.94 or newer and libcap-ng-dev to build, and KVM to run a microVM); see docs/modes.md#sandbox
```

To check a configured default before you run anything: `by doctor` (`provider.default`: "this build can run it", or the command above; `host.kvm`) and `by up --plan` (the `provider` step, ready or blocked, changing nothing).

## Server

A server keeps branches, runs the work and outlives your terminal. It does not isolate anything: with the local provider it runs harnesses as the user it runs as.

On the host, in the repository:

```sh
by init server                       # an interview that writes .branchyard/server.json and token files
by serve --config .branchyard/server.json --check   # load and validate it; serves nothing
by serve --config .branchyard/server.json
```

or just `by serve`, which serves the current repository on loopback and makes a token at `.branchyard/server/token` the first time (and says where, each time it starts). `[serve] config = ".branchyard/server.json"` in `branchyard.toml` makes a bare `by serve` read that file.

Where you work:

```sh
by --remote http://127.0.0.1:8421 --token-file .branchyard/server/token ls
```

```toml
[remote]
url = "http://127.0.0.1:8421"
token_file = ".branchyard/server/token"   # or .branchyard/tokens/admin.token, from `by init server`
repo = "app"                              # when the server serves several
# ca_file = "ca.pem"                      # for a private certificate authority
```

`BRANCHYARD_REMOTE`, `BRANCHYARD_TOKEN_FILE`, `BRANCHYARD_REPO` and `BRANCHYARD_CA_FILE` are the same four, above the file. `--remote ssh://HOST/PATH` starts a server on the host for you and reaches it through a forwarded socket ([remote over ssh](remote-ssh.md)).

**Guaranteed:** operations are admitted once and survive a restart; tokens carry scopes (`read`, `run`, `merge`, `admin`), tenants and quotas; plain HTTP is refused off loopback.

**Not guaranteed:** isolation. A `run` token on a repository can make a harness do anything the server's user can; scopes bound what a credential reaches, not what a harness does. `--allow-provider microsandbox,substrate` lets a request name a sandbox, the server's operator chooses it, and `local` stays allowed ([server](server.md#security)).

When the server does not answer, `by` ends with the way out:

```text
by: cannot reach http://127.0.0.1:8421: Connection refused
Next: start a server with `by serve` on that host, or fix the URL, token or CA file ([remote] url, token_file, ca_file)
```

`by doctor` asks a configured server once (`server.reachable`), and reads the file `[serve] config` names without starting anything: `serve.config` is `by serve --check` on it, and `serve.database` says which store it names.

## Postgres

SQLite keeps one server's branches and operations. PostgreSQL lets several servers and `by worker` processes share them: an operation admitted before a crash runs once, on whichever worker claims it.

```sh
by serve --config .branchyard/server.json --database postgres://branchyard@db.internal/branchyard
by worker --config .branchyard/server.json --database postgres://branchyard@db.internal/branchyard --label gpu
```

or `"database": "postgres://…"` in the server's file. The URL is stored in that file, so it holds no password: use a `.pgpass` file. [Server](server.md) has the schema, the scheduling and the labels.

**Needs the `postgres` feature.** The released binaries have it. A `cargo install` from the README does not, and then `by serve` would refuse to start. `by serve --check` says so first, without connecting to anything:

```text
$ by serve --config .branchyard/server.json --check
by serve: this build has no PostgreSQL support; use a prebuilt release, which has it, or rebuild from a checkout with `cargo install --locked --path crates/branchyard-cli --features postgres` (`--path crates/branchyard-server` for branchyard-server); see docs/modes.md#postgres
```

`--check` and `serve.database` read the file and the build; they do not open the database. A wrong host, user or password shows when `by serve` connects.

## Moving between rungs

| From | To | Change |
|---|---|---|
| Local | Isolated | add `--isolated`, or `isolated = true` under `[defaults]`; then `--secret NAME` for the harness's login |
| Isolated | Local, for one run | `--isolated=false` |
| Either | Sandbox | build with the feature if you need it; add `--provider ...` or `[defaults] provider`; run `by doctor` first |
| Any | Server | `by serve` on the host; `[remote]` or `--remote` here. The harnesses then run there, as that host's user |
| Server | PostgreSQL | a database, a build that has the feature, `--database` or `"database"`; `by serve --check` |

The branch records which one a branch ran in, and `by show` says so. Where it ran does not change afterwards: `by send` continues a branch where it started unless you name another provider.

## Not done

- The sandbox rungs have not run a harness on a real runtime from this repository's tests. Microsandbox compiles and passes its unit tests; its KVM tests are `#[ignore]`d. No Substrate cluster has been used.
- No released binary has the Microsandbox SDK. Shipping one needs `libcap-ng` at build time and a story for KVM and `msb` in `by up`; that is a decision, not a fix. Until then, the rung is reachable only from a source build.
- The sandbox's address for the host is three keys with three names (`[connectors] sandbox_gateway`, `[models] sandbox_host`, `[connectors] effects_sandbox_host`). They are not merged.
- `--isolated` and `--provider` are two controls for one axis. A single `isolation = "none" | "private-home" | "sandbox"` would be clearer; it is a design change, not made here.
- There is no `by doctor --modes` that prints the highest rung this build and host can reach, with the command for the next. The table above is that, by hand.
- `by serve --check` does not connect to PostgreSQL. A server file that names a database nothing listens on passes it.
- `[microsandbox]` accepts the flags' names (`memory`, `keep_sandbox`, `sandbox_snapshots`) as well as its own; the flags do not accept the file's (`--memory-mib`). Substrate has no file section at all.
