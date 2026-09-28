# Branchyard

**A Rust SDK for building meta-harnesses that control other coding harnesses on servers.**

A meta-harness decides how to divide work, which harnesses to use, when to create children, and which results to pursue. Branchyard supplies the control operations: execution, resource access, durable state, and validated integration.

The topology develops during execution. You define capabilities, budgets, and acceptance rules. The meta-harness creates and revises its collaborators as it discovers work.

> **Early development.** This repository contains the researched design, pinned upstream control sources, and tested Rust crates: harness protocol drivers for Claude Code, Codex, Antigravity, Pi, Amp and ACP agents, a local-mode SDK engine with the `by` command, sandbox capability admission, and Microsandbox and Agent Substrate providers. None of these is qualified against a live runtime yet. The local engine is tested only against a fake agent. A first server exposes the local engine over an authenticated HTTP API ([remote mode](#remote-mode)); it runs harnesses without isolation, and the node service and sandboxed execution are specified but not implemented.

## What you can build

A parent running Claude Code could delegate a parser change to Codex, request a review from Gemini CLI, and create another child when a dependency emerges. Each child receives a workspace, a resource budget, and scoped access. Children can delegate further when authorized. Their changes return as candidates for validation and integration.

The system is designed to support:

- **Dynamic delegation:** Create children, exchange messages, and change dependencies at runtime.
- **Server execution:** Run harnesses in isolated sandboxes with explicit compute, storage, and network limits.
- **Selective sharing:** Use private workspaces, shared read-only components, or coordinated mutable resources.
- **Durable supervision:** Preserve task state across client disconnects and reconcile failed execution attempts.
- **Validated merging:** Check exact candidate changes and promote them only against the expected target revision.

A task, a conversation, a sandbox, and a code branch have separate identities. Forking a conversation does not automatically copy its filesystem or credentials.

## Quick start

```sh
cargo install --locked --path crates/branchyard-cli   # installs `by`
cd path/to/your/repo
by init                                               # set up by interview
by init project --defaults                            # or: one topic, every default
```

`by init` asks what to set up and interviews you in the terminal: it detects your installed harnesses, credential variables (by name only), the repository's check command, docker and KVM, shows every file it would write as a diff checked by the loader that reads it, and writes only after you agree. Topics: `project` (`branchyard.toml`: default harness, model, limits, permissions, isolation, check, secrets by name, a server to use), `server` (a server configuration with hashed per-tenant credentials, 0600 token files and quotas), `rig`, `deploy` (compose with PostgreSQL) and `plugin`.

Or let your coding harness do it. `by init plugin` installs the `setup` skill into Claude Code or Codex (or load the whole plugin with `claude --plugin-dir plugins/branchyard`); then ask it to "set up Branchyard" or run `/branchyard:setup`. It drives the same interview through `by init TOPIC --json --next`, asks you each batch with its own question tool, shows the dry-run diff, and applies after you confirm. See [setup](docs/setup.md) for both front-ends, the protocol and `branchyard.toml`; `by config show` prints every effective value and where it came from.

## Local mode

Local mode runs the engine in-process and each harness as a local process in its own git worktree under `.branchyard/`. It needs no server, but it provides **no isolation beyond your operating-system user**: by default a harness runs with your environment, your `HOME` and your own harness login, and can read and write whatever you can. Every `CLAUDE*` variable except Claude Code configuration (provider selection, credentials, TLS client identity, limits) is removed, so a harness never runs under the identity of a Claude Code session that launched `by`. `--isolated` scrubs credentials and uses a private `HOME`, so the harness is then usually not logged in; `--secret` [provisions](docs/provisioning.md) a credential into that home.

```sh
by harnesses                                          # installed harnesses and their qualification
by run "Make the flaky parser test deterministic" --check "cargo test" --max-minutes 20 --ask
by fan "Make the flaky parser test deterministic" --harness claude-code,codex --check "cargo test" --yes
by ls
by diff make-the-flaky-parser-test-deterministic-codex
by log make-the-flaky-parser-test-deterministic-codex
by merge make-the-flaky-parser-test-deterministic-codex   # runs the check on the exact merge, then moves the current branch
by rm make-the-flaky-parser-test-deterministic-claude-code
```

Every tool permission request reaches Branchyard. The Antigravity, Pi and Amp profiles cannot route them, so `by` refuses them unless you pass `--allow-unapproved-tools` (see [harness integration](docs/harness-integration.md#implemented-drivers)). `--ask` prompts on the terminal, `--yes` allows each one, and with neither flag and no terminal they are denied. `by log` shows each decision. These commands are tested end to end against a fake ACP agent. Against a real harness, one `by run` → `by diff` → `by merge` has run with Claude Code 2.1.283 ([validation](docs/validation.md)); the rest is on the [live testing checklist](docs/testing-live.md). Resuming or forking a session in another worktree may fail for harnesses that keep sessions per directory, such as Claude Code; the branch then reports the failure rather than starting over silently.

State is durable in `.branchyard/state.db` (SQLite). A turn runs under its branch's lease, so two `by` processes never drive one branch, and `by cancel <branch>` stops a running turn from any terminal. If `by` is killed mid-turn, the next `by` command on the repository recovers the branch: it kills the harness's process group when its pid and start time still match, and ends the branch `interrupted`, saying whether the prompt had been submitted. A prompt is never submitted again. See [durability](docs/durability.md) for the journal, leases, recovery rules and limits.

## Remote mode

`by serve` (or the `branchyard-server` binary) serves repositories over an authenticated HTTP JSON API with Server-Sent Events, running the same engine in-process. `by --remote URL` runs every command against it with the same output; `branchyard-client` is the typed Rust client. [Surfaces](docs/surfaces.md) lists every operation on every surface, SDK, `by`, `by --remote`, HTTP, client and delegation, and what each refuses.

```sh
cd path/to/your/repo
by serve                                   # 127.0.0.1:8421; creates .branchyard/server/token
export BRANCHYARD_REMOTE=http://127.0.0.1:8421
export BRANCHYARD_TOKEN_FILE=path/to/your/repo/.branchyard/server/token
by run "Make the flaky parser test deterministic" --check "cargo test" --yes
by ls
by merge make-the-flaky-parser-test-deterministic
```

For more than one user, TLS, PostgreSQL, quotas or webhooks, `by init server` writes a configuration with a hashed credential per tenant and each token in a 0600 file, checks it with the server's own loader, and prints the `by serve --config … --check` and `by serve --config …` to run.

Work runs on the server: interrupting `by` stops watching, not the turn, `by cancel` stops the turn, and a retried request with the same idempotency key never runs twice. Operation status and the activity feed survive a server restart; a turn still running when the server stops is recorded as interrupted, and its branch is recovered when the server starts again; an operation still queued runs then. Plain HTTP binds only to loopback unless TLS is configured or `--insecure-bind` is given. By default the server uses the local process provider, so **harnesses run as the server's user with no isolation**, and every token holder can direct them. Its operator can allow the Microsandbox and Substrate providers (`--allow-provider`), delegation for its harnesses and remote `by spawn`, `inspect`, `events`, `integrate` and `children` (`--allow-delegation`), and unapproved tools (`--allow-unapproved-tools`); `by --remote` then takes the same flags as local `by`. `by serve --database postgres://…` keeps branch state, operations and their dispatch queue in PostgreSQL (the `postgres` feature), where several servers and `by worker` processes may share them; an operation admitted before a crash runs once, on whichever worker claims it. See [the server reference](docs/server.md) for the API, authentication, deployment and what is durable.

## Command line

`by --help` lists the commands by group; `by help <command>` (or `by <command> --help`, `-h` for a summary) shows a command's options under headings such as *Checks and limits*, *Permissions*, *Launch*, *Provisioning* and *Global options*, with examples for the main commands. Nested commands have their own help: `by help graph apply`, `by artifact publish --help`. A mistyped command or flag gets a suggestion (`by mrege` → `merge`), and a usage error exits with status 2, a failed command with 1.

The global options choose where commands run, and may come before or after the command; each falls back to its variable, and a flag wins over its variable. Under both sits the configuration, `branchyard.toml` in the repository and `~/.config/branchyard/config.toml` ([setup](docs/setup.md#configuration)): it fills only what the command line and the variables left unset, so the order is flags, then variables, then the project file, then the user file:

| Option | Variable |
|---|---|
| `--remote URL` | `BRANCHYARD_REMOTE` |
| `--token-file FILE` | `BRANCHYARD_TOKEN_FILE` |
| `--repo NAME` | `BRANCHYARD_REPO` |
| `--ca-file FILE` | `BRANCHYARD_CA_FILE` |

Setup is two commands in the *Shell and setup* group: `by init [TOPIC] [--json] [--next | --dry-run | --apply [--force]] [--answers FILE|-] [--defaults]`, where clap refuses two steps at once, `--force` without `--apply`, `--answers` without a step and a step without a topic (all exit 2), and `by config show|path|validate [FILE]|schema [--json]`.

Short forms: `-n` for `--name`, `-b` for `--base`, `-y` for `--yes`, `-f` for `log --follow`, `-o` for `artifact get|export --out`, `-V` for `--version`. Durations take a unit: `--max-minutes 90s`, `--stall-after 2h`, `watch --interval 250ms` (a bare number keeps its old unit); `--budget-usd` also takes `$2.50`. `--check` and `--command` are split like a POSIX shell, including `#` comments.

Shell completions and a man page are generated from the same definitions, including `init`'s topics (`by init <Tab>` offers `project`, `server`, `rig`, `deploy` and `plugin`) and `config`'s actions:

```sh
by completions bash > ~/.local/share/bash-completion/completions/by
by completions zsh > "${fpath[1]}/_by"
by completions fish > ~/.config/fish/completions/by.fish
by completions powershell >> $PROFILE      # or elvish
by man | man -l -
```

`by serve` and `by worker` hand everything after them to the server's own parser, so `by serve --help` is the server's help and `--repo NAME=PATH` there is the server's flag, not the global one; `branchyard.toml`'s `[serve] config` becomes their `--config` unless they name one (`-c FILE` too), and `by serve --config FILE --check` validates a configuration without serving. `branchyard-server`, `branchyard-bridge`, `branchyard-herdr`, `branchyard-mcp` and `branchyard-qualify` parse their command lines the same way (clap), each with `--help`.

## Watching branches

`by watch` shows every branch as a tree, forks under their parents, with status, harness, current activity (the tool running, a pending permission request, the last line of the message), turns, cost and age. `by watch --once` prints it once:

```text
by watch · /src/app

BRANCH        HARNESS      STATUS      TURNS   COST  AGE  ACTIVITY
parser        claude-code  running         2  $0.41   3m  ▸ Bash · Running the parser tests
└ parser-alt  codex        ready           1  $0.12   1m  Rewrote the tokenizer loop
docs          gemini-cli   no changes      1      -   9m  The docs already cover this

3 branches, 1 running, $0.53 reported
```

On a terminal it is a live dashboard (ratatui): the tree with each status in its own color (an interrupted branch in black on yellow, since its work is stopped mid-turn), and beside or below it the selected branch's detail: its latest events, cost and tokens, unread inbox messages, children, prompt and the commands to run next. `j`/`k` or the arrows move, Enter focuses the selected branch's subtree, `/` filters by name, harness, status or prompt, `?` lists every key, and `q`, Esc or Ctrl-C exits and restores the terminal (as does a panic). Piped, it prints one line per change instead. It reads event logs incrementally, and works the same with `--remote`, where it follows the server's event stream.

It is also a cockpit: keys act on the selected branch by running the `by` command they name, with the same `--remote` and other global flags, and the footer lists the ones that apply to it.

| Key | Does | Runs |
|---|---|---|
| `s` | Send a follow-up prompt, typed in a box | `by send` in the background |
| `S` | Steer the running turn | `by send --steer` |
| `R` | Resume an interrupted branch in its own session, with one key | `by send` with a "continue where you left off" prompt |
| `x` | Cancel the running turn and its descendants', after a yes | `by cancel` |
| `m` | Merge, after a yes; a pane then shows the result of the check | `by merge` |
| `f` | Fork, with a prompt | `by fork` in the background |
| `d` / `l` | The candidate's diff, or the whole log (following new events), in a scrollable pane | `by diff`, `by log` |
| `y` / `Y` | Copy the branch's name, or its worktree's path, through the terminal (OSC 52) | |
| `p` `o` `r` `c` `t` | Reserved for `by pr`, `by open`, `by rewind`, `by compare` and `by try` | |

Commands that run a turn start detached (their output goes to `.branchyard/watch/` in the repository), so they carry on if the dashboard quits; a result or a refusal (such as `R` on a branch that is not interrupted) appears in the status line. With no terminal to ask on, those turns get the permissions `branchyard.toml` sets, and requests are otherwise denied. Remotely, an action that needs this machine (copying a worktree's path) is refused with the reason. The keys come from one table, `crates/branchyard-cli/src/watch/actions.rs`, which also generates the `?` sheet.

`by watch` also tells you when a branch needs you or ends, and so does a waiting `by run`, `by fan`, `by send` or `by fork`: when a tool asks for permission, a branch asks or escalates a question, a turn stalls, or a branch fails, is interrupted, or finishes. Each event is said once, with a terminal bell and a desktop-notification escape, OSC 9 (iTerm2, WezTerm, kitty, Ghostty, Windows Terminal) or OSC 777 (foot, urxvt, VTE terminals such as GNOME Terminal), passed through tmux; it needs no D-Bus. History `by watch` reads when it starts is not news. `[notify] desktop = true` in `branchyard.toml` also runs `notify-send` (or `osascript` on macOS), `[notify] terminal` picks the escape (`auto`, `osc9`, `osc777`, `bell`, `none`), and `--no-notify` or `[notify] enabled = false` turns it off. Escapes go only to a terminal: `by watch`'s own, or a waiting command's stderr.

`by log --follow <branch>` prints a branch's events as they are recorded. In [Herdr](https://github.com/herdrdev/herdr), the [Branchyard plugin](plugins/herdr/README.md) follows a server's event stream and gives each branch a tab running `by log --follow`, with the branch's state in Herdr's agent sidebar (`working`, `blocked` on a waiting permission request, `idle` with what happened) and actions to merge, cancel or send to the focused branch. It is tested against a fake `herdr`, not yet against Herdr itself.

## Sandbox providers

A harness runs through a sandbox provider. The default **local** provider is the local mode above. The **Microsandbox** provider runs each turn's harness in a microVM booted from an OCI image with the harness installed: the branch's worktree is mounted at `/workspace`, the harness gets only `HOME` and the variables you name with `--pass-env`, and the microVM is destroyed when the turn ends.

```sh
cargo +1.94 install --locked --path crates/branchyard-cli --features microsandbox
by run "Fix the flaky parser test" --provider microsandbox --image ghcr.io/you/claude-code:2.1 \
  --cpus 2 --memory 4096 --pass-env ANTHROPIC_API_KEY --check "cargo test" --yes
```

It needs Linux with KVM and the `msb` 0.7.3 runtime, and the `microsandbox` cargo feature, which is off by default because the pinned SDK needs Rust 1.94 while the workspace pins 1.90. It is **unqualified**: its unit tests pass, but its KVM tests have not run. See [sandbox providers](docs/providers.md) for the contract, each provider's guarantees, and how to run those tests.

The **Agent Substrate** provider, in the default build, runs each turn's harness in an [Agent Substrate](docs/substrate.md) actor on Kubernetes. Substrate has no exec API, so the actor's template runs `branchyard-bridge`, which starts the harness for connections that arrive through Substrate's router carrying a credential the host signs for that attempt. An actor cannot mount the worktree: it is copied in and the result brought back as git bundles, and applied to the worktree's files so the candidate is recorded as usual.

```sh
branchyard-bridge keygen --out bridge.key       # its public key goes in the actor template
by run "Fix the flaky parser test" --provider substrate \
  --substrate-endpoint https://substrate.example --substrate-ca ca.pem \
  --substrate-router 'wss://router.example/{atespace}/{actor}/' --substrate-template by-claude \
  --substrate-key bridge.key --pass-env ANTHROPIC_API_KEY --check "cargo test" --yes
```

Both connections use TLS (plain HTTP only to loopback unless `--substrate-insecure`), commits the harness makes come back as commits, and the bridge can be the container's process 1 and run the harness as another user. It is **unqualified**: it has run only against an in-process fake cluster, and the router's addressing and TLS handling are assumptions. See [Agent Substrate](docs/substrate.md) and [live testing](docs/testing-live.md#6-agent-substrate-cluster).

## Delegation

A harness can act as a meta-harness. With `--delegate`, the harness runs `by` in its own shell to create and coordinate child branches, within an envelope of depth, width, harnesses and budget:

```sh
by run "Split the parser rewrite: delegate the tokenizer to codex and the formatter to yourself, then integrate both" \
  --delegate --budget-usd 3 --yes
# inside the harness, as its own branch:
#   by spawn "Port the tokenizer to the new API; run its tests" --name tokenizer --harness codex --budget-usd 1
#   by inspect tokenizer --json
#   by integrate tokenizer          # merges into the parent's branch, never into yours
by ls                               # the tree
```

The same operations are a Python module, a Rust `Delegate`, and MCP tools (`by mcp`), with one authority model: a per-turn token that lets a branch act only on its descendants. In local mode that stops mistakes, not a hostile harness. See [delegation](docs/delegation.md).

Children may depend on their siblings: `by spawn --depends-on tokenizer "..."` creates a child that waits and starts once `tokenizer` has settled (or, with `--after integrated`, once it was integrated), and `by graph apply` changes a branch's graph of children in one atomic proposal, refused whole with `stale_revision` when the graph moved on. A failed prerequisite blocks its dependents. See [task graphs](docs/graph.md).

## Rigs

A rig declares a team in a TOML file: a root seat and the seats below it, each with a harness, model, budget, check, policy and standing instructions. `by rig run` starts the root; its harness fills the other seats with `by spawn --seat`, and the envelope derived from the tree bounds them:

```sh
by rig check examples/rigs/feature.toml      # validate strictly and print the plan (--json for the plan itself)
by rig run examples/rigs/feature.toml "Add a --json flag to the export command"
# inside the lead's harness:
#   by spawn --seat implementer "Add the flag and its tests"
#   by integrate feature-implementer
```

A seat's `escalates_to` names an ancestor seat a branch there may escalate to, besides its own parent (always allowed); fields Branchyard still cannot honor, such as sibling messaging or a permission bypass, are refused by name at plan time. `by --remote` runs a rig on a server that allows delegation. See [rigs](docs/rigs.md).

## Architecture

Clients submit work and observe results. All managed harness execution happens on servers.

```mermaid
flowchart TD
    Client["Rust SDK / CLI / web"] --> Server["Branchyard server"]
    Server --> State["PostgreSQL and durable commands"]
    Server --> Nodes["Execution nodes"]
    Nodes --> Sandboxes["Isolated harness sandboxes"]
    Sandboxes -->|"Delegated control requests"| Server
    Nodes --> Artifacts["Artifacts and candidates"]
    Artifacts --> Checks["Validation and guarded promotion"]
    Server --> Checks
```

The planned Rust services use Tokio, Axum/Tower, SQLx/PostgreSQL, PGMQ, Cedar, OpenDAL, and Tonic. Existing sandbox runtimes supply virtualization and guest execution. The first provider to qualify is the [Microsandbox open runtime](https://microsandbox.dev/) on Linux servers. Access to its private-beta cloud service is not a dependency. [Agent Substrate](docs/substrate.md) is a second candidate for operators who already run Kubernetes.

Fast startup comes from prepared images, cached repository objects, private writable state, and available server capacity. Warm and cold startup paths will be measured separately. No latency guarantee is claimed yet.

## Harness interfaces

| Interface | Purpose |
|---|---|
| Rust SDK and server API | Durable task, graph, resource, and integration operations |
| ACP | A common client protocol for controlling compatible harness sessions |
| Native drivers | Harness-specific session, turn, permission, and lifecycle controls |
| MCP tools or thin CLI | Let a harness call Branchyard's control operations |

Use one ACP client alongside native drivers where required. Codex's App Server, Claude's Agent SDK, Antigravity's streaming CLI, and Pi-family RPC interfaces need their own qualified profiles. A structured JSON stream alone does not establish permission control or reliable recovery.

Drivers for Claude Code, Codex, Antigravity, Pi, Amp and ten ACP harnesses are [implemented](docs/harness-integration.md#implemented-drivers): 15 of the 16 targets have a default profile, and Aider, a batch process, has none. Both Claude Code profiles have passed [live protocol qualification](docs/qualification/README.md); none is yet qualified inside a sandbox. The Antigravity and Pi drivers replay transcripts recorded without a model call; the Amp driver rests on documentation only; all three are unqualified. The [integration design](docs/harness-integration.md) covers **16 harnesses**: Claude Code, Codex, Antigravity, Oh My Pi, DeepSeek Harness, Gemini CLI, OpenCode, Pi, Goose, Aider, Cursor, GitHub Copilot, Amp, Qwen Code, Kimi CLI, and Hermes. This is a researched target matrix, not a claim of deployed support.

The generated [compatibility matrix](docs/compatibility.md) lists every profile's capabilities and live qualification result.

## What is in this commit

| Component | Status |
|---|---|
| `branchyard-controls` | One harness identity registry across Herdr, Scion and the integration matrix, and Herdr's official-agent-source check that validates it; the CLI resume-recipe builder once here was evaluated and removed as unused (no profile needed it; see [vendoring](docs/vendoring.md#herdr-reuse-the-official-agent-source-registry-only)); 7 tests pass |
| `branchyard` | The local-mode SDK engine: tasks, branches as git worktrees, forks, budgets, per-invocation permission policies, an event log per branch read from cursors, validated merges, harness-to-harness messages ([delegation](docs/delegation.md#inbox): ask, report, escalate, answer, delivered by steering a running turn or at a turn's start), [task graphs](docs/graph.md) (dependencies between children started durably by whichever engine settles a prerequisite, atomic graph proposals, scratch-area bindings), and durable execution on SQLite or, with the `postgres` feature, PostgreSQL (leases, journaled steps, cancellation, crash recovery; see [durability](docs/durability.md)), and turns in Substrate actors, each harness's home [provisioned](docs/provisioning.md) before its turn; 191 hermetic tests against a fake ACP agent, including a killed engine, turns in a fake Substrate cluster, one storage conformance suite, messages steered into a running turn, portable artifact bundles (a deterministic tar, verified member by member, tampered/missing/extra members refused), and dependents started across processes and after a crash, none against a real harness, and 21 more on PostgreSQL (including six engines racing to start one dependent) |
| `branchyard-harness` | Sans-IO protocol drivers: Claude Code stream-json, Codex App Server, Antigravity stream-json, Pi RPC, Amp stream-json, and ACP v1 for ten more harnesses; 15 of 16 targets have a default profile; every unsupported or partial capability carries a reason, quoted from the driver's own refusal message or docs, rendered in [compatibility](docs/compatibility.md) and looked up for admission errors; 98 tests, including replays of recorded Claude Code, Codex, Antigravity and Pi sessions and of documentation-derived Amp sessions, a conformance contract run against all 17 profiles, and each profile's steering-boundary evidence checked against `docs/harness-integration.md`; both Claude Code profiles pass live protocol qualification |
| `branchyard-provision` | Harness home provisioning translated from Scion's provisioners: a sans-IO planner per harness (Claude Code, Codex, Gemini CLI, OpenCode, GitHub Copilot CLI, Hermes, Antigravity) for secrets, MCP servers, instructions, model, reasoning effort and telemetry, and an executor that merges into native configuration and writes secrets 0600 into the branch's private home only; each plan says how every secret reaches the harness and whether its tools inherit it, nothing secret goes on a command line, and removing a branch removes the credentials it wrote; 83 tests, including Scion's cases and golden files; Claude Code's key and MCP delivery checked against its binary offline, the rest unverified against real harnesses; see [provisioning](docs/provisioning.md) |
| `branchyard-qualify` | Runs driver qualification scenarios against real harness binaries; see [driver qualification](docs/qualification/README.md) |
| `branchyard-workspace` | Git worktree branches, candidate commits and validated merges: compare-and-swap on the target, checks in a temporary worktree, conflicts returned for repair; 18 tests |
| `branchyard-runtime` | Runs a driver against a harness process through any sandbox provider, and the local provider: own process group, scrubbed environment, private home, teardown that names and kills surviving descendants; 30 hermetic tests, including the provider conformance checks, against a fake ACP agent |
| `branchyard-cli` | The `by` command on the SDK: `run`, `fan`, `send`, `fork`, `ls`, `show`, `diff`, `log` (with `--follow`), `merge`, `rm`, `cancel`, `send --steer`, `harnesses`, `watch`, `serve`, `rig`, `artifact`, `scratch`, `graph`, and the delegation commands, each also in remote mode, with a [clap](https://docs.rs/clap) command line, shell completions and a man page; `init` (the setup wizard and its JSON protocol) and `config`, with `branchyard.toml` defaults under the flags; 136 tests, 49 of them running the built binary against temporary repositories, a spawned server, a fake Substrate cluster and a fake ACP agent, and 1 more on PostgreSQL |
| `branchyard-setup` | The [setup](docs/setup.md) interview without I/O: declarative questions per topic (project, server, rig, deploy, plugin) with conditions, rules and defaults detected through an injected probe, batches for a harness's question tool, plans of files with diffs and validator verdicts, secrets never in any output; `branchyard.toml`'s format, layering and JSON Schema; 29 tests |
| `branchyard-server` | The server: bearer-token authentication with tenant identity (principals with scopes and repository ownership, hashed credentials, `token new`) and per-tenant quotas counted in the admission's transaction, durable operations with idempotency keys, admitted as a durable enqueue and run by workers under fenced leases, cancellation, steering a running turn, a resumable SSE activity feed read from the engine's store, recovery, one server per data directory or several (and `by worker` processes) on one PostgreSQL database, TLS and graceful shutdown, operator opt-ins for providers, delegation and unapproved tools, secrets resolved from the operator's own table, delegation endpoints including graph proposals (a spawn that waits is queued work like any other, and every server and worker resumes graphs on its recovery tick), rig seats checked on submission, artifacts and scratch areas over HTTP (`--max-artifact-bytes`), and a PostgreSQL store, and `--check` to validate a configuration without serving; 87 tests, 43 over real HTTP against a fake ACP agent (including adversarial webhook receivers that hang, close without answering or refuse forever, none of which delay an operation or the feed, and identity, scopes, tenant isolation and quotas: `tests/tenants.rs`), and 12 more on PostgreSQL (including a spawn that waits run by a worker alone, and two servers and a worker starting one dependent once) (with the feature, 1 SQLite-only parity test is left out); see [the server reference](docs/server.md) |
| `branchyard-client` | The remote SDK: typed blocking client for every endpoint, including artifacts and scratch areas (digest-verified downloads), SSE parsing and reconnect by cursor; 13 tests (14 with `--features schema`) |
| `branchyard-herdr` | The [Herdr plugin](plugins/herdr/README.md)'s binary: a bridge from the server's event stream to one Herdr tab per branch and `herdr pane report-agent` states, with merge, cancel and send actions; 11 tests, 2 of them against a spawned server, the fake ACP agent and a fake `herdr`; not run against a real Herdr |
| `branchyard-mcp` | Branchyard's delegation tools over MCP on stdio (`by mcp`), for harnesses whose shell is restricted; the same operations and token as `by spawn` and the SDKs; 11 tests |
| `branchyard-sandbox` | The vendor-independent `SandboxProvider` contract, provider conformance checks, and capability admission; unsupported requirements are rejected, never weakened; 10 tests |
| `branchyard-microsandbox` | A [Microsandbox](https://github.com/superradcompany/microsandbox) provider over its public SDK 0.7.3, behind the off-by-default `microsandbox` feature (the SDK needs Rust 1.94); 11 mapping tests, 4 more with the SDK, and 14 ignored tests for a KVM host; **unqualified** |
| `branchyard-substrate` | An [Agent Substrate](https://github.com/agent-substrate/substrate) `SandboxProvider` over a client generated from its unmodified proto, exec through the bridge, TLS on both hops, UID fencing before and after each call, quiescence checks, git transfer that keeps the harness's commits, the bridge's actor template, and a fake cluster (optionally over TLS) for tests; 41 tests, including the conformance checks, against the fake, and 4 ignored tests for a cluster; **unqualified** |
| `branchyard-bridge` | The in-sandbox exec bridge for runtimes without an exec API: a versioned frame protocol over WebSocket, optionally over TLS, Ed25519-signed per-attempt credentials with tamper-evident state, process groups with teardown, reaping and signal handling as process 1, execs as another user, file and tree transfer, and its host-side client; 27 tests, 2 of them only as root |
| Scion controls | Nine provisioners at `d9b9e6a`, adjacent helpers/configuration, the authoring guide, and tests; eight suites run 261 tests, 260 passing and 1 skipped; seven provisioners translated into `branchyard-provision` |
| Herdr controls | Original resume source (kept down to the official-agent-source registry check) and 22 terminal-observation manifests |
| OpenRig controls | Launch/readiness contract and configuration fragments; not a standalone adapter |
| Warp controls | Separate AGPL source references for process supervision; excluded from the Rust build |
| Architecture and plan | Server design, harness contracts, implementation milestones, and release gates |

All **86 vendored files** are pinned to upstream revisions, licenses, Git blob IDs, and SHA-256 hashes. `vendor/` is a pinned reference snapshot: a file may carry a local patch only when `vendor.patches.json` records it with its reason and upstream commit (none does today), and `tools/verify_vendor.py` checks every other file against its pin. Adaptations built into Branchyard are recorded outside `vendor/`. [Vendoring decisions](docs/vendoring.md) explain their intended use. [Replicas](https://replicas.dev/) remains a product reference; no licensed runtime source was identified to copy.

Scion's Claude provisioner and its model-alias tests disagreed at the previous pin; at `d9b9e6a` all 13 pass, and CI checks that no incompatibility reappears. See [validation](docs/validation.md).

## Try the current crates

Use the pinned Rust toolchain and Python 3.12. These commands validate the foundation without provider credentials or a sandbox host. Only `cargo fetch` uses the network:

```sh
cargo fetch --locked
cargo test --workspace --locked --offline
cargo run --locked --offline -p branchyard-controls --example plan_resume
python3 tools/verify_vendor.py
python3 tools/verify_derivatives.py
python3 tools/test_scion.py --qualified
python3 tools/check_scion_compatibility.py
```

The example prints a resume argument vector. It does not launch a harness. The crate does not authorize sessions; callers must verify tenant, workspace, session ownership, and executable compatibility before using a recipe.

## Build next

Start with one complete remote task: shared contracts, a qualified sandbox provider, durable commands, one harness driver, and artifact capture. Then add dynamic children, resource sharing, guarded integration, and additional profiles.

- [Architecture](docs/design.md): ownership, topology, compute, networking, storage, budgets, and recovery.
- [Harness integration](docs/harness-integration.md): interfaces, callback placement, session semantics, and qualification.
- [Implementation plan](docs/implementation-plan.md): ordered milestones and acceptance gates.
- [Comparison](docs/comparison.md): Scion, OpenRig and Herdr against Branchyard, and what to absorb from each.
- [Lifecycle](docs/lifecycle.md): stall detection, webhook notifications and reincarnation.
- [Distribution](docs/distribution.md): installing the skill for Claude Code and Codex, and reproducible plugin/SDK archives.
- [Deploying `by serve`](docs/deploy.md): the container image, a PostgreSQL compose recipe, and a host preflight report.
- [Contributing](CONTRIBUTING.md): implementation boundaries and validation workflow.

## License

Branchyard-authored code is **Apache-2.0**. Vendored components retain their upstream licenses. `vendor/warp-agpl/` contains AGPL source references and is not linked into the Apache-licensed crate. The repository therefore contains multiple licenses. See [third-party notices](THIRD_PARTY.md).
