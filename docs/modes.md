# Where harnesses run and where state lives

Branchyard has five places to stand, from least to most protected. Each one adds a guarantee and costs something. Take the lowest rung that gives you what you need, and confirm you are on it with the one command in the last column.

Two questions are answered together here, because they are asked together: *where does the harness process run* (and what can it reach), and *where do the branches' records live* (and who can reach them). The first rises through the rungs; the second changes at the last two.

| Rung | You want | Set (flag, then `branchyard.toml`) | Guaranteed, and not | Costs and needs | Confirm with |
|---|---|---|---|---|---|
| [**Local**](#local) | to try Branchyard; nothing in the way | nothing. `--provider local` / `[defaults] provider = "local"` says it | Each harness in its own worktree and process group; under `--permissions read-only` or `edit-worktree` on Linux, its writes held to the preset's scope ([file guard](#the-file-guard)). *Not* a sandbox: it runs as you, in your `HOME`, with your login, and can read what you can | none | `by doctor`: `provider.default` is `local (the default)`, `host.landlock` names the guard's ABI; the run prints `by: local mode`, and `by: guard: landlock (PRESET)` when guarded |
| [**Isolated**](#isolated) | the harness kept out of your login and dotfiles | `--isolated` / `[defaults] isolated = true`; `--isolated=false` turns it off for one run | A private `HOME` at `.branchyard/homes/BRANCH`, and no `ANTHROPIC*`, `CLAUDE*`, `OPENAI*`, `CODEX*` or `BRANCHYARD_*` variables. *Not* a sandbox: still your user, your files, your network, your other variables | none; the harness is then not logged in, so give it a secret ([provisioning](provisioning.md)) | the run prints `by: isolated mode`; its `provider` line in `by show BRANCH` says `local, isolated: private HOME, not a sandbox`; `by config show` says where `isolated` came from |
| [**Sandbox provider**](#sandbox) | the harness out of your filesystem and process space | `--provider microsandbox --image REF` / `[defaults] provider = "microsandbox"` and `[microsandbox] image = "REF"`; `--provider recipe:NAME` / `provider = "recipe"` and `recipe = "NAME"`; `--provider substrate` and its `--substrate-*` flags (no file form) | A microVM, a recipe's machine or an actor per turn, with only the worktree and the variables you pass. All three are **unqualified**: tested against fakes, never on a real runtime in this repository's tests | Microsandbox: KVM, a build with `--features microsandbox` (no released binary has it), Rust 1.94, `libcap-ng`, the `msb` runtime. Recipe: your scripts and an `ssh` target. Substrate: a cluster | `by doctor`: `provider.default` and `host.kvm`; `by up --plan`: the `provider` step; `by recipe check NAME` |
| [**Shared server**](#server) | branches that outlive your terminal, or a machine other than yours | on the host: `by serve`, with `[serve] config = "FILE"` in `branchyard.toml` for its file. On the client: `--remote URL --token-file FILE` (`--repo`, `--ca-file`) / `[remote] url`, `token_file`, `ca_file`, `repo` | One store (SQLite) for branches and operations; tokens with scopes and tenants; work survives your disconnecting. *Not* isolation: the server runs harnesses as its own user, and a token with `run` can make them do anything that user can | a host you are willing to hand over; TLS off loopback; the sandbox rungs above, if you want them on the server (`--allow-provider`) | `by doctor`: `server.reachable` (client side), `serve.config` and `serve.database` (the file `[serve] config` names); `by serve --check` |
| [**PostgreSQL**](#postgres) | several servers and `by worker` processes sharing one queue | `by serve --database postgres://USER@HOST/NAME` / `"database"` in the server's file | The same operations run once, on whichever worker claims them, across restarts. Nothing about where harnesses run changes | a database; a build with `--features postgres` (the released binaries have it; `cargo install` from the README does not) | `by doctor`: `serve.database` (names the store, does not connect); `by serve --check` refuses what the build cannot open |

The first three rows are about the harness. The last two are about the store and the people reaching it, and they combine with any harness row: a PostgreSQL server can run its harnesses locally, isolated or in a sandbox.

On the first two rows, `--permissions read-only` and `edit-worktree` add a [file guard](#the-file-guard) below the harness on Linux: the operating system holds it to the preset's write scope, whether or not the harness asks Branchyard before it writes.

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

**Not guaranteed:** anything beyond your operating-system user. The harness has your `HOME`, your login, your other variables, your filesystem and your network. A `--yes` run lets it use all of that without asking. Under `--permissions read-only` or `edit-worktree` on Linux the [file guard](#the-file-guard) limits where it can write; your `HOME` stays in its scope.

Before it starts, `by` says so, the first time in a repository: `by: local mode: harnesses run as your user, in your own HOME; no sandbox` (the marker is `.branchyard/local-mode-noted`, written once a harness the note was printed for has started, so a command that fails before that, such as a run on a base that does not exist, does not spend it). The other modes are said on every run.

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

Before it starts, `by` says: `by: isolated mode: harnesses run as your user with a private HOME under .branchyard/homes/; not a sandbox, so they can still read your files and reach the network`. Afterwards the record keeps it: `by show BRANCH` has `provider  local, isolated: private HOME, not a sandbox`, and `by ls --json`, `by show --json` and `by inspect --json` carry `"provider": "local"` and `"isolated": true`, with the turn's [`guard`](#the-file-guard) beside them.

## The file guard

A permission preset decides the tool calls a harness asks Branchyard about. Several harnesses run a write without asking: Pi, Antigravity, DeepSeek Harness, Hermes and OpenCode on its defaults never route a shell write to Branchyard, so for them `read-only` and `edit-worktree` used to decide nothing about files. On Linux with Landlock, Branchyard now also starts a local harness under a Landlock ruleset with the preset's write scope. The harness, and every process it starts, can then write only there, whatever tool it uses:

| Preset | The harness can write in | Not |
|---|---|---|
| `edit-worktree` | its worktree; its git directory and the repository's `.git/objects`, `.git/refs` and `.git/logs`, so a commit it makes lands; its `HOME`; its temporary directory (`.branchyard/tmp/BRANCH`, its `TMPDIR`); `/dev`; the [scratch areas](storage.md) it may reach | the rest of the repository (the main checkout, other branches' worktrees, `.branchyard/`), anything else on the machine |
| `read-only` | its `HOME`, its temporary directory, `/dev` | its worktree, the git directory (a read-only turn makes no commit), anything else |
| `full`, `--yes`, `--ask`, explicit rules | anywhere your user can: no guard | |

`HOME` is the branch's private home with `--isolated`, and **your own `HOME` otherwise**: without `--isolated` a guarded harness can still write your dotfiles. A planning turn (`--plan`) runs under `read-only`, and so under its guard. A delegated child gets its own guard, in its own worktree, from the policy it runs under.

What a refused write looks like: the operating system answers `EACCES` (`Permission denied`), and the harness reports it as any failed write. Pi 1.4.0 on 27 October 2026: `EACCES: permission denied, open '…/.branchyard/worktrees/outside.txt'`.

**Said and recorded.** Before a local run under one of the two presets, `by` prints one line, every run:

```text
by: guard: landlock (edit-worktree): the harness can write only in its worktree, its git directory, your own HOME, its temporary directory and /dev
```

The branch records what its last turn got: `by show` and `by inspect` have a `guard` line under `provider` (`landlock (edit-worktree, ABI 7): writes only in …`, or `none: <why>`), and `by ls --json`, `by show --json` and `by inspect --json` a `guard` field next to `provider` and `isolated`: `{"kind": "landlock", "preset": "edit-worktree", "abi": 7, "writable": [...]}`, `{"kind": "none", "reason": "..."}`, or `null` before a turn has started a harness. `by doctor`'s `host.landlock` row names the ABI, or why there is none.

**Where it cannot be held.** On a kernel without Landlock (before Linux 5.13, built without it, or with it left out of the `lsm=` boot parameter), on macOS, or with `BRANCHYARD_GUARD=off` in `by`'s environment, the turn runs unguarded, and `by` says so in one note:

```text
by: note: no file guard on this host (BRANCHYARD_GUARD=off turns it off): --permissions read-only decides only the tool calls the harness asks Branchyard about
```

and the record says `guard: none: this host cannot hold it: …`. A sandbox provider's harness records `none: the harness runs in a sandbox (…), which is its boundary`; Microsandbox, Substrate and a recipe's machine refuse an exec that asks for a write scope rather than run it unguarded (`ExecSpec::writable`).

**What it does not stop.**

- Reads. A guarded harness reads everything your user can: your SSH keys, other branches' worktrees, the repository.
- The network. That is the [egress policy](egress.md)'s, and confined only when one is set.
- Any one command. Inside its scope the harness writes what it likes; the guard is a scope, not a per-command decision. A command a preset denies still runs in a harness that does not ask.
- Writes in your own `HOME` in local mode (use `--isolated`), and git refs: with `.git/refs` in scope, a harness that runs git under `edit-worktree` can move another branch's ref, not only its own.
- A process the harness hands work to that is already running outside it: a daemon, a language server started earlier, a container engine.
- It does not leave room for a sandbox of the harness's own that makes a user namespace or mounts (bubblewrap, `unshare`): under the guard, writing `/proc/self/uid_map` is refused, and Landlock refuses `mount` to a restricted process (`unshare -rm mount -t tmpfs …` failed both ways on 27 October 2026). Such a sandbox fails rather than runs.

**How it is built.** [`crates/branchyard-runtime/src/landlock.rs`](../crates/branchyard-runtime/src/landlock.rs): the ruleset is built before the fork (each allowed path opened `O_PATH` and added beneath with every write-side right the kernel's ABI knows: writing, truncating, removing, making files, directories, links, sockets, FIFOs and devices, and `REFER` for renames between directories); between `fork` and `exec` the child sets `PR_SET_NO_NEW_PRIVS` and calls `landlock_restrict_self`, after the network namespace's own setup when egress is confined. Reads, execution, `ioctl` and the network are not handled. Support is probed once per process by restricting `/bin/sh` and seeing a write to `/dev/null` refused. `PR_SET_NO_NEW_PRIVS`, which Landlock needs, also means a set-user-ID program (`sudo`) runs without its privileges inside a guarded harness. The engine derives the scope from the turn's preset in [`crates/branchyard/src/guard.rs`](../crates/branchyard/src/guard.rs).

**Delegation under the guard.** A delegating harness needs nothing more: the `by` its shell runs (and `by mcp`) only connects to the engine's broker over a Unix socket, which Landlock's file rights do not cover, and the child's turn runs on a thread of the engine, outside the parent's restriction, under its own guard.

Status, 27 October 2026: built; hermetic, and run live once. Hermetic: `/bin/sh` and the fake ACP agent under the guard (`branchyard-runtime` `tests/guard.rs`, `tests/guard_off.rs`, the `write_scope` provider check), the engine (`tests/guard.rs`: a commit the harness makes under `edit-worktree` lands) and the built `by` (`branchyard-cli` `tests/guard.rs`: the lines, the record, `--yes`, `BRANCHYARD_GUARD=off`, `by doctor`, and a harness that runs `by spawn --wait` from inside the guard while its child is guarded in its own worktree). Live: Pi 1.4.0 on `zai/glm-5.3-flash` (Z.ai), `--isolated --allow-unapproved-tools`, Linux 6.18 with Landlock ABI 7, from this repository's `by`. Under `read-only`, asked to create `hello.txt`: `The write failed: the current directory (…/.branchyard/worktrees/ro) is not writable — permission denied (EACCES)`, and the branch ended `no changes`. Under `edit-worktree`, asked to write `hello.txt` and `../outside.txt`: `Successfully wrote to hello.txt`, then `EACCES: permission denied, open '…/.branchyard/worktrees/outside.txt'`, and the branch ended `ready` with one file. The same `read-only` prompt with `BRANCHYARD_GUARD=off` created `hello.txt` and ended `ready`: what every Pi run under `read-only` did before. The three runs took 15, 19 and 15 seconds and cost $0.0005, $0.0004 and $0.0001 as Pi reported; the quoted lines, copied from their output, are the record. Not done: the other four harnesses that write without asking (Antigravity, DeepSeek Harness, Hermes, OpenCode) and the ones that ask (Claude Code, Codex, Gemini CLI) were not run under the guard; Codex's own Linux sandbox under it was not tried; no kernel older than ABI 7 was tried (ABI 1 and 2 hosts get fewer handled rights and refuse renames between directories); macOS has no guard.

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
by: cannot reach http://127.0.0.1:8421: Connection refused (os error 111)
  next: start a server with `by serve` on that host, or fix the URL, token or CA file ([remote] url, token_file, ca_file)
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
- The file guard has no flag of its own: the preset decides it, and `BRANCHYARD_GUARD=off` is the only way to run a preset unguarded on a host that has Landlock. It guards writes only; a read scope (keeping a harness out of `~/.ssh`) and guarding `--ask` and a denying default (which name no preset) are not built.
- The sandbox's address for the host is three keys with three names (`[connectors] sandbox_gateway`, `[models] sandbox_host`, `[connectors] effects_sandbox_host`). They are not merged.
- `--isolated` and `--provider` are two controls for one axis. A single `isolation = "none" | "private-home" | "sandbox"` would be clearer; it is a design change, not made here.
- There is no `by doctor --modes` that prints the highest rung this build and host can reach, with the command for the next. The table above is that, by hand.
- `by serve --check` does not connect to PostgreSQL. A server file that names a database nothing listens on passes it.
- `[microsandbox]` accepts the flags' names (`memory`, `keep_sandbox`, `sandbox_snapshots`) as well as its own; the flags do not accept the file's (`--memory-mib`). Substrate has no file section at all.
