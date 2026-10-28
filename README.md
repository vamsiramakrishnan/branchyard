# Branchyard

**Coordinate agent harnesses for code and knowledge work, and validate what they return.**

`by` starts a task on a new git branch, in a worktree of its own, with the coding harness you choose: Claude Code, Codex, Gemini CLI and others. The harness works there while you do something else. You read the diff, and `by merge` runs your check on the exact merge before it moves your branch. A harness can delegate to child branches, several harnesses can attempt the same task, and a branch outlives the terminal that started it. Each harness keeps its own planning, native subagents and task graph. For research and other knowledge work, a required JSON deliverable can complete without a Git change; [cooperative teams](docs/cooperative-teams.md) covers native leads, seats and frozen handoffs. The same engine is a Rust and Python SDK ([SDKs](docs/sdk.md)), and `by serve` offers it over HTTP.

> **Status: early development.** Tests include hermetic fixtures and dated live runtime and harness qualification. Read [the remote validation record](docs/remote-validation-2026-10-28.md) for the tested environments and remaining limits, and [status](docs/status.md) for the wider project.

## Install

Install the released binaries and enter the guided setup:

```sh
curl -fsSL https://github.com/vamsiramakrishnan/branchyard/releases/latest/download/install.sh | sh -s -- --setup
```

The installer downloads verified prebuilt binaries and starts the guided setup. No Rust toolchain or source checkout is needed. Use `--no-setup` for binaries alone and `--version VERSION` to pin a release. Linux hosts that need local Microsandbox support use `--runtime`; [distribution](docs/distribution.md) describes the native prerequisites and source-build alternative.

## Start here

You need git, a repository, and a harness installed and logged in (`by harnesses` lists what this machine has).

```sh
cd your-repo
by setup                                               # detect your harnesses, write branchyard.toml, install hooks
by run "Make the flaky parser test deterministic" --check "cargo test" --yes   # a branch, a harness, a check
by ls                                                  # your branches and their state
by merge make-the-flaky-parser-test-deterministic      # run the check on the exact merge, then merge
```

A real run spends what the harness's login or key costs; `--budget-usd 1` limits reported cost. Unknown usage is not a hard spending guarantee ([cost limits](docs/delegation.md#what-a-cost-limit-guarantees)).

`by setup` asks only what it cannot detect; `by setup --yes` asks nothing, and `by init project --defaults --apply` writes just the project file. It also installs Branchyard's hooks, so a session you start in a harness hears when its branches finish. Codex runs no hook until you trust it: start `codex` once and choose "Trust all and continue". `by init plugin codex` installs only Codex's hooks; its skills come with the Codex plugin ([setup](docs/setup.md#one-command), [distribution](docs/distribution.md#install-per-harness)).

**Try it without a model.** The fake agent is a scripted stand-in for a harness, built from the clone:

```sh
cargo build --locked -p branchyard-runtime --bin fake-acp-agent    # in the clone
FAKE=$PWD/target/debug/fake-acp-agent
cd your-repo
by run "WRITE hello.txt=hi" --name hello --harness gemini-cli --command "$FAKE" --yes
by ls && by merge hello                                # hello.txt arrives on your branch
```

## Where harnesses run

| Mode | The harness runs | Read |
|---|---|---|
| Local | as your user, in a git worktree; no isolation beyond your operating-system user | [modes](docs/modes.md#local) |
| Isolated | locally, with a private `HOME` and scrubbed credentials (`--isolated`, `--secret`) | [modes](docs/modes.md#isolated) |
| Sandbox | in a Microsandbox microVM or an Agent Substrate actor (`--provider`) | [modes](docs/modes.md#sandbox) |
| Server | in `by serve`, driven with `by --remote`, as the server's user | [modes](docs/modes.md#server) |
| PostgreSQL | as a server, with state shared by several servers and workers | [modes](docs/modes.md#postgres) |

The source build above reaches local, isolated and server mode. Substrate is in it too but needs a Kubernetes cluster; Microsandbox needs `--features microsandbox` and a KVM host, and PostgreSQL needs `--features postgres`. The providers are unqualified: [status](docs/status.md#subsystems).

## Common tasks

| I want to | Go to |
|---|---|
| Set up for the harnesses I have | `by setup`: [setup](docs/setup.md#one-command), [doctor](docs/doctor.md), [up](docs/up.md) |
| Run one task on several harnesses and pick a result | `by fan`: [fleet](docs/fleet.md), [checkpoints and compare](docs/checkpoints.md) |
| Coordinate code and knowledge workers from an existing lead | [cooperative teams](docs/cooperative-teams.md) |
| Delegate from inside a harness session | [delegation](docs/delegation.md), [rigs](docs/rigs.md) |
| Open a pull request from an issue | `by run --issue 42`, `by pr`: [pull requests](docs/pull-requests.md) |
| Watch branches and steer them | `by watch`: [watching branches](docs/watch.md) |
| Run a server for a team | `by init server`, `by serve`: [server](docs/server.md), [deploy](docs/deploy.md) |
| Run with isolation | `--isolated`: [modes](docs/modes.md#isolated), [provisioning](docs/provisioning.md) |
| Debug a stuck branch | [troubleshooting](docs/troubleshooting.md) |
| Use the Python SDK | [SDKs](docs/sdk.md) |
| Add a harness | [adding a harness](docs/adding-a-harness.md), [writing a driver](docs/writing-a-driver.md) |
| See what hooks tell a session | [harness hooks](docs/harness-hooks.md) |
| Upgrade | [upgrade](docs/upgrade.md) |

## Harnesses

Branchyard drives Claude Code, Codex, Antigravity, Pi, Amp and, through ACP, Gemini CLI, Oh My Pi, DeepSeek Harness, OpenCode, Goose, Cursor, GitHub Copilot, Qwen Code, Kimi CLI and Hermes. Aider has no driver. A driver is not support: [the generated table in docs/status.md](docs/status.md#harness-drivers) says which have passed live qualification, which only part of it, and which have not run against the real harness. `by harnesses` shows what is installed here.

## Documentation

- [docs/README.md](docs/README.md): every page, by what you want to do.
- [Getting started](docs/getting-started.md): the first hour, step by step.
- [Status](docs/status.md) and [roadmap](docs/roadmap.md): what is built and run, and what comes next.
- [SDKs](docs/sdk.md), [harness hooks](docs/harness-hooks.md) and [writing a driver](docs/writing-a-driver.md).
- [Contributing](CONTRIBUTING.md): the checks to run before you push.

## License

Branchyard-authored code is **Apache-2.0**. Vendored components retain their upstream licenses. `vendor/warp-agpl/` contains AGPL source references and is not linked into the Apache-licensed crate. The repository therefore contains multiple licenses. See [third-party notices](THIRD_PARTY.md).
