# Branchyard for Herdr

A [Herdr](https://github.com/herdrdev/herdr) plugin that presents a Branchyard server's branches in Herdr: one tab per branch following its event log, and the branch's state in Herdr's agent sidebar, so you can see which branch is working, which needs you and which is done. From a branch's tab, Herdr actions merge it, cancel it or send it another prompt.

**Untested against a real Herdr.** It is tested against a fake `herdr` that records its calls (see [Tests](#tests)), written from Herdr's documentation and source at `fff6c82` (0.9.1). The check to run by hand is in [live testing](../../docs/testing-live.md#8-herdr-plugin-no-model-calls).

## How it works

Branchyard runs harnesses on protocol pipes, not terminals, so a branch cannot be a Herdr pane with a live harness in it ([comparison](../../docs/comparison.md#herdr-as-a-client)). The plugin shows Branchyard's view instead:

- **The bridge** (`branchyard-herdr bridge`, in its own tab) opens the server's activity feed, `GET /v1/repos/{repo}/events/stream` ([server](../../docs/server.md#event-stream)), lists the branches, then applies every feed entry after the stream's starting cursor.
- **One tab per branch**, opened without taking focus in the bridge's workspace, runs `by log --follow <branch>` and is named `by: <branch>`. It opens the first time the branch has something to report; a branch merged before the bridge saw it gets none.
- **State**: on each change the bridge runs `herdr pane report-agent <pane> --source custom:branchyard --agent branchyard --state … --message … --seq …` on the branch's pane:

  | Branchyard | Herdr | Message |
  |---|---|---|
  | `running` | `working` | |
  | `running`, a permission request waiting for an answer | `blocked` | `waiting for permission: <tool>` |
  | `ready` | `idle` | `ready to merge` |
  | `no_changes` | `idle` | `no changes` |
  | `merged` | `idle` | `merged into <target>` |
  | `failed` | `idle` | `failed: <reason>` |
  | `interrupted` | `idle` | `interrupted` |
  | `budget_exceeded` | `idle` | `budget exceeded: <limit>` |

  Changes are debounced (250 ms by default) and only a changed report is sent, so a permission request answered at once by the policy never shows. `--seq` is milliseconds since the epoch, strictly increasing, so Herdr ignores a stale report.
- **Reconnects**: when the stream drops, as when the server restarts, the bridge reconnects after the last cursor it applied, with backoff up to 5 seconds: nothing is missed or applied twice. If the server's feed was reset (`cursor_out_of_range`), it lists the branches again.
- **Panes are reused**: the branch-to-pane map is kept in `panes.json` in the plugin's Herdr state directory. A restarted bridge checks each saved pane with `herdr pane get` before using it, and reports every branch's current state once. A pane you close is opened again at the branch's next change.

## Install

The plugin needs Rust (the pinned toolchain) and Herdr 0.9.1 or newer, on Linux or macOS. From GitHub:

```sh
herdr plugin install <owner>/branchyard/plugins/herdr
```

Herdr clones the repository and runs the manifest's build step, `cargo build --release --locked -p branchyard-herdr -p branchyard-cli`, which puts `branchyard-herdr` and `by` in the checkout's `target/release`. From a local checkout, build first, since `plugin link` does not build:

```sh
cargo build --release --locked -p branchyard-herdr -p branchyard-cli
herdr plugin link "$PWD/plugins/herdr"
```

`bin/branchyard-herdr` runs `$BRANCHYARD_HERDR_BIN`, else `target/release/branchyard-herdr` in the checkout, else `branchyard-herdr` on `PATH`; `by` is `$BRANCHYARD_BY`, else the checkout's `target/release/by`, else `by` beside the binary or on `PATH`. The `by` must be the server's version.

## Configure

Herdr starts plugin commands with its own server's environment, which usually lacks Branchyard's settings. Put them in `config.env` in the directory `herdr plugin config-dir branchyard` prints:

```sh
BRANCHYARD_REMOTE=http://127.0.0.1:8421
BRANCHYARD_TOKEN_FILE=/home/me/src/app/.branchyard/server/token
# Needed when the server serves several repositories:
BRANCHYARD_REPO=app
# BRANCHYARD_CA_FILE=/path/to/ca.pem
# Optional:
# BRANCHYARD_BY=/path/to/by
# BRANCHYARD_HERDR_DEBOUNCE_MS=250
# BRANCHYARD_HERDR_WORKSPACE=<workspace id for branch tabs>
# BRANCHYARD_HERDR_SEND_ARGS=--yes
```

These are the variables `by --remote` reads ([server](../../docs/server.md#remote-mode-in-by)); the process environment wins over the file, and `branchyard-herdr --remote URL --token-file FILE --repo NAME --ca-file FILE` wins over both. The plugin passes them to each `by` it starts. For a repository on this machine, run `by serve` in it and point the plugin at `http://127.0.0.1:8421`: the plugin only speaks to servers.

## Use

```sh
herdr plugin action invoke branchyard.start     # or bind it, below
```

opens the bridge in a new tab, which prints one line per report. `herdr plugin pane open --plugin branchyard --entrypoint bridge --placement tab` does the same. Bind the actions to keys in Herdr's configuration:

```toml
[[keys.command]]
key = "prefix+m"
type = "plugin_action"
command = "branchyard.merge"
description = "merge this branch"
```

| Action | Does |
|---|---|
| `branchyard.start` | Opens the bridge in a new tab |
| `branchyard.merge` | `by merge <branch>` for the branch in the focused pane; the result is a Herdr notification |
| `branchyard.cancel` | `by cancel <branch>`; the branch ends `interrupted` |
| `branchyard.send` | Opens a popup that reads one prompt and runs `by send <branch> <prompt>` plus `BRANCHYARD_HERDR_SEND_ARGS` |

The branch is the one the bridge opened the focused pane for, looked up in `panes.json`; an action from any other pane shows a notification and does nothing. In remote mode `by send` without `--yes` denies the harness's permission requests; set `BRANCHYARD_HERDR_SEND_ARGS=--yes` to allow them. The popup shows `by send` until the turn ends; Ctrl-C closes it and stops watching, not the turn, which the branch's tab keeps showing.

## Limits

- **Not run against a real Herdr.** The Herdr calls follow its CLI source and documentation at `fff6c82`; details the source leaves to runtime, such as how a custom-source report shows in the sidebar for a pane whose process is `by`, are unverified.
- **Server only.** It follows a Branchyard server, not a local repository directly; run `by serve` for local work.
- **`blocked` is rare.** A remote turn cannot ask (`--ask` is refused remotely), so a request waits only when a local `by run --ask` works on the served repository, whose activity reaches the feed within half a second.
- **Every unmerged branch gets a tab** when the bridge starts, including old failed ones; remove branches you no longer want with `by rm`. A removed branch's tab stays until you close it, since removal is not in the feed.
- **One bridge per server and repository.** Two bridges would each open tabs. The bridge does not release its reports when it stops, so a pane keeps its last state until the bridge runs again.
- **Moved panes.** A pane moved to another workspace gets a new ID; its action lookup then fails until the bridge opens a new pane for the branch.
- **Linux and macOS.** The launcher is a POSIX shell script.

## Tests

`crates/branchyard-herdr` has unit tests for the state mapping and configuration, a check of this manifest against the rules Herdr's loader applies (`src/app/api/plugins/manifest.rs`), and an end-to-end test: `by serve` with the fake ACP agent, and a fake `herdr` first on `PATH` that records every call and hands out pane IDs. It creates branches that end ready, failed, interrupted by the cancel action, merged by the merge action and sent a prompt through the popup, and asserts one tab per branch, the reported states, no repeated report, a reconnect after the server restarts on the same port that resumes after its cursor without reporting or opening anything again, a closed pane opened again, and a restarted bridge that checks and reuses its panes.
