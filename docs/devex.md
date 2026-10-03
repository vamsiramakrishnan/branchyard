# Developer experience: emdash, Superset and Conductor

emdash, Superset and Conductor are desktop workbenches for running several coding agents side by side, each in its own git worktree. Branchyard is a different kind of thing: an SDK, CLI and server that a meta-harness (or a person) drives. The engine underneath is comparable, though: branches as worktrees, parallel agents, review and merge. And these three have put far more care into the moments a person actually lives through. This page records the mechanisms that make them pleasant to use, where Branchyard stands on each, and what to take.

Surveyed on 27 September 2026 from each project's own docs, releases and issue trackers; claims marked *unverified* could not be confirmed against a primary source.

## The three

| | emdash | Superset | Conductor |
|---|---|---|---|
| What | Desktop "agentic development environment" (Electron) | Desktop "agentic IDE" (Electron), plus CLI, TypeScript SDK, MCP server, iPhone app | Native macOS app |
| Source | [generalaction/emdash](https://github.com/generalaction/emdash), Apache-2.0 | [superset-sh/superset](https://github.com/superset-sh/superset), Elastic License 2.0 | Closed; binaries and changelog in `meltylabs/conductor-releases` |
| Platforms | macOS, Windows, Linux | macOS; Linux AppImage experimental; no Windows | macOS only |
| Activity | v1.2.7, 2026-09-27; 5.9k stars | v1.30.2, 2026-09-22, plus daily canary; 14.7k stars | 0.87.5, 2026-09-25; several releases a week |
| Price | Free | Free locally; paid tier for remote/iPhone features (*details unverified*) | Free locally; Pro $50/month (cloud workspaces, multiplayer, API), Teams $60/user/month |
| Agents | Drives the CLIs already installed (Claude Code, Codex, Cursor, OpenCode, Amp, Copilot and more; "25+") | Launches any agent CLI with the user's own subscription | Claude Code, Codex, Cursor, OpenCode, using each tool's own login |

## The mechanisms that make them delightful

### 1. The first five minutes ask for nothing new

- **emdash**
  - Detects whichever agent CLIs are installed and lists them as providers when you create a task.
  - Needs no sign-in: "You can use Emdash locally without signing in or connecting GitHub" ([installation](https://emdash.com/docs/installation)).
- **Conductor**
  - Checks `gh auth status` and `claude /login` or `codex login` on first run, and reuses them ([first workspace](https://www.conductor.build/docs/first-workspace)).
  - Inherits MCP servers and slash commands from Claude Code's and Codex's own configuration instead of reimplementing them ([MCP](https://www.conductor.build/docs/reference/mcp)).
- **Superset**
  - A bare `superset` opens an arrow-key command browser with guided prompts.
  - Before running, it shows the exact shell command it is about to run ([CLI getting started](https://docs.superset.sh/cli/getting-started)).

**Branchyard:** `by harnesses` detects installed harnesses, and provisioning reuses an existing login where a harness allows it ([provisioning](provisioning.md)). There is no guided start: today a new user reads docs and writes JSON and TOML by hand. **Take:** the setup interview under way (`by init`, and the `setup` skill that lets Claude Code or Codex conduct it) should open with what it detected, ask only about what it could not, and show the exact commands and files before acting, as Superset does.

### 2. One committed file makes every new worktree ready to work

All three solve the same pain, which reviewers of Conductor call its main friction: a fresh worktree has no `.env`, no `node_modules` and no running dev server ([madewithlove](https://madewithlove.com/blog/conductor-running-multiple-ai-coding-agents-in-parallel/)).

| | File | Hooks | Environment given to scripts | Local override |
|---|---|---|---|---|
| emdash | `.emdash.json` | `scripts.setup`, `run`, `teardown`; `preservePatterns` (for example `.env`, copied into each worktree); `shellSetup` (for example `nvm use`) | `EMDASH_TASK_ID`, `EMDASH_TASK_PATH`, `EMDASH_PORT` | Personal settings > `.emdash.json` > host > built-in ([project config](https://emdash.com/docs/project-config)) |
| Superset | `.superset/config.json` | `setup`, `teardown`, `run` (arrays of commands) | `SUPERSET_WORKSPACE_NAME`, `SUPERSET_ROOT_PATH` | `~/.superset/projects/<mirrored path>/config.json` ([setup and teardown](https://docs.superset.sh/setup-teardown-scripts)) |
| Conductor | `.conductor/settings.toml` (was `conductor.json`) | `setup`, `archive`, several named `run` scripts (`[scripts.run.web]`, `[scripts.run.worker]`) with `available_in = ["local", "cloud"]` and a concurrent or sequential run mode | `CONDUCTOR_PORT` (an allocated port), `CONDUCTOR_WORKSPACE_PATH`, `CONDUCTOR_WORKSPACE_NAME`, `CONDUCTOR_ROOT_PATH` | ([scripts](https://www.conductor.build/docs/reference/scripts)) |

**Branchyard: done** ([workspace](workspace.md)). `[workspace]` in `branchyard.toml` has:
- `copy`: globs of untracked files carried from the repository root into each new worktree, never outside it, never a symbolic link, never tracked files, and never committed from the branch;
- `setup` (one command or a list), named `[workspace.run.NAME]` scripts for `by workspace run` (a `default`), and `teardown` on `by rm` and `by merge --rm`.

Scripts and the harness get `BRANCHYARD_BRANCH`, `BRANCHYARD_WORKTREE`, `BRANCHYARD_ROOT` and `BRANCHYARD_PORT`, a port reserved per branch in the store (SQLite or PostgreSQL), stable across turns and restarts, and released with the branch. The user file's `[projects."<root>".workspace]` replaces a repository's section, as Superset's mirrored path does. Setup is a journaled step: a crash mid-install kills what it started, ends the branch `interrupted`, and its next turn runs setup again from the start. A repository's scripts never run until you trust them: a per-user record keyed by repository root and the section's digest, asked once on a terminal or given with `by workspace trust`, and asked again when the scripts change; a server runs them only for repositories its operator lists in `allow_workspace_scripts`, never because a request asks. `by init project` suggests the section from lockfiles, `Cargo.toml`, `pyproject.toml`, `go.mod`, a Compose file and `.env` files, or imports it from a committed `.emdash.json`, `orca.yaml`, `.superset/config.json` or `.conductor/settings.toml` ([setup](setup.md#importing-another-tools-workspace-configuration)).

**Still open:**
- Conductor's `available_in`. **Done since** (Wave 2, [workspace](workspace.md#listening-ports)): its concurrent run mode, `by workspace run BRANCH A B --detach` starting several scripts at once, each with its own port; and the ports each branch's processes listen on, attributed by Branchyard's variables, working directory, process tree or command line (ported from Orca), in `by workspace ports`, `by show` and `by watch`'s detail pane, where `b` opens one in a browser and `K` stops them.
- An emdash-style `shellSetup` (for example `nvm use`) applied to the harness's own shell.
- Setup inside a sandbox: scripts run on the host, as you, even for isolated or sandboxed branches, and the Substrate provider's bundle does not carry files setup creates that git ignores.
- Tested hermetically only: no real package manager, Docker stack or harness, and not on macOS.

### 3. The same prompt to several agents, compared side by side

Superset: press ⌘N again with the same prompt and a different agent, and you get parallel attempts to choose from ([first workspace](https://docs.superset.sh/first-workspace)). Conductor names the two patterns: several workspaces (fan out issues), or several agents in one workspace (an implementer and a reviewer on one branch) ([parallel agents](https://www.conductor.build/docs/concepts/parallel-agents)).

**Branchyard is ahead here:**
- `by fan` runs one prompt across harnesses, each on its own branch, under one budget.
- Rigs declare implementer and reviewer seats.
- Task graphs order dependent work.

**Take:** the review step after a fan-out. A `by compare` (or a view in `by watch`) should put the attempts side by side (diff stats, check results, cost, turns) and merge the chosen one with one keystroke.

**Done** ([checkpoints](checkpoints.md#compare-attempts)): `by compare <branch>...` or `--fan <name>` shows status, turns, cost, tokens, time, check, diff stats and the files only each attempt changed (`--json` too); `--check` runs each check on its exact candidate; `--diff A B`; `--pick` merges through the validated merge, and `--discard-others` removes the rest after confirmation. Works with `by --remote` except `--check` and `--diff`. In `by watch`, `c` shows the selected branch beside its siblings (the rest of its fan-out, or its parent's other children) in a pane, locally and remotely. **Remains:** picking from that pane with one keystroke; `by compare --pick` does it from the shell.

### 4. Work survives closing the app

emdash autosaves terminal state and resumes agents where they left off ([tasks](https://emdash.com/docs/tasks)). Superset's terminals are backed by a daemon, so scrollback and processes survive restarts and updates.

**Branchyard is ahead on durability:**
- Leases, journaled steps, recovery after a crash.
- A server and workers that keep operations across restarts.
- Native session resume per harness.

**But what a person sees is weaker:** a turn whose engine died ends `interrupted`, and you resume it yourself. **Take:**
- `by watch` should show interrupted branches prominently, with a one-key resume (a `by send` to the branch, which resumes its native session).
- With a server running (`by serve` or `by worker`), local turns should outlive the terminal that started them. This is the daemon behaviour Superset has.

**Status:** the first is done. `by watch` draws an interrupted branch in black on yellow, counts them in its header, and `R` resumes the selected one with one key (`by send` with a "continue where you left off" prompt, which resumes the harness's native session). A turn started from `by watch` runs in its own process group, detached from the dashboard, so it outlives `by watch` and the terminal. What remains is the second: `by run` in a terminal still ties its turn to that terminal unless it goes through a server.

### 5. Review, PR and CI without leaving the tool

- **emdash:**
  - "Commit & Create PR" or "Push & Create PR" from the commit card.
  - A Checks tab that polls GitHub Actions for the PR's head commit ([diff view](https://emdash.com/docs/diff-view), [CI checks](https://emdash.com/docs/ci-checks)).
- **Superset:**
  - Stage, commit, push and create the PR from the diff viewer.
  - Route PR review comments back to the agent that wrote the code as a revision task.
- **Conductor:**
  - A Checks tab that combines git status, PR state, CI, review threads and todos into one merge-readiness gate.
  - GitHub review comments appear inline, and resolving them updates the gate ([checks](https://www.conductor.build/docs/reference/checks), [diff viewer](https://www.conductor.build/docs/reference/diff-viewer)).

**Branchyard (done, [pull requests](pull-requests.md)):**
- `by pr <branch>` runs the branch's check on its candidate, pushes it (`--git-remote`, default `origin`), and opens or updates its pull request through `gh`, with a body built from the task, linked issue, turns, cost, check result and diffstat. Running it again updates the same pull request.
- `by pr <branch> --watch` follows the pull request with backoff. A failed CI check (with a bounded `gh run view --log-failed` tail), a review, a comment or an unresolved review comment is sent back into the branch once, by steering its running turn or with `by send`'s path, and the next candidate is pushed. Every step is an event in `by log`, and what was delivered is kept there, so a restarted watch repeats nothing.
- `by show` (and `--json`) has a merge-readiness line: local check, pull-request state, CI summary, unresolved threads, mergeability and review decision, from the last observation; `--refresh` asks GitHub first.

In `by watch`, `p` runs `by pr` after a yes and shows its output, `P` starts `by pr --watch` in the background, and the detail pane has the readiness line. **Since:** the watch answers "Addressed in <commit>" to, and resolves, the review threads a pushed fix addressed, and `by review` (`v` in `by watch`) sends comments written on the diff in an editor as one prompt ([pull requests](pull-requests.md#by-review)). **What remains:** GitHub only, local mode only; nothing tested against GitHub itself.

### 6. Undo one step, not the whole branch

Conductor snapshots each agent turn. Hovering a message and choosing revert discards that turn and everything after it ([checkpoints](https://docs.conductor.build/core/checkpoints), via a search summary). emdash and Superset offer only discarding uncommitted changes.

**Branchyard:** each turn's result is recorded in the event log, and `by fork` branches from a branch. There is no per-turn rewind. **Take:**
- Commit a checkpoint ref at each turn's end (`refs/branchyard/<branch>/turn-N`; cheap, since the turn already ends with a candidate commit).
- `by rewind <branch> --to N` (or `by fork <branch> --at N` for a non-destructive version).
- Rewinding a harness's native session is not always possible, so where resume cannot follow, the rewound branch starts a fresh session with a summary, and says so.

**Done** ([checkpoints](checkpoints.md)): every turn records `refs/branchyard/<branch>/<incarnation>/turn-N` as a journaled step and a `Checkpoint` event, listed by `by show` and `by log` and removed with the branch. `by fork <branch> --at N` branches from any checkpoint; `by rewind <branch> --to N` resets the branch, journaled so recovery finishes one cut short, and keeps later checkpoints so it can be undone by rewinding forward. The harness's own session continues only when it ended at that checkpoint; otherwise a fresh session starts with a summary of the turns before, and the `Rewound` or `ForkedAt` event and the output say which. In `by watch`, `r` lists the checkpoints, takes a number and asks before rewinding, and the detail pane shows the checkpoint the branch is at. **Remains:** rewind and `fork --at` through the server API.

### 7. Try the agent's branch in the app already running

Conductor's Spotlight commits a workspace's tracked changes as a checkpoint and checks them out at the repository root, where the user's dev server and Docker stack are already running with hot reload. It syncs one way and restores the root when turned off ([spotlight testing](https://www.conductor.build/docs/reference/scripts/spotlight-testing)).

**Branchyard:** absent. **Take:** `by try <branch>` and `by try --off`.
- It applies the branch's diff to the main checkout only when that checkout is clean.
- Branchyard records what it changed, so `--off` restores the checkout exactly.
- It refuses when the user has uncommitted work.

**Done** ([checkpoints](checkpoints.md#try-a-branch-in-this-checkout)): `by try <branch>` applies the candidate's diff with `git apply` (all or nothing) to a clean checkout only, saving each touched path's prior entry and permission bits in `.branchyard/try/state.json` first; `by try --off` restores them byte for byte, refusing when a tried file changed since unless `--force`; `by try <other>` swaps; `--status` reports; a try cut short is rolled back by the next call. Local mode only. **Done too:** `t` in `by watch` tries the selected branch after a yes, and on the tried branch restores the checkout.

### 8. One MCP definition, written into every agent's own config

emdash's Library holds a catalog of 54 MCP servers. Adding one writes it into each agent's native file (`~/.claude.json`, `~/.cursor/mcp.json`, `~/.copilot/mcp-config.json`, …) ([MCP](https://emdash.com/docs/library/mcp)).

**Branchyard:** `--mcp` already provisions an MCP server into each harness's native configuration, per branch, inside that branch's home ([provisioning](provisioning.md)), without touching the user's global files. **Take:** let `branchyard.toml` list MCP servers once, so every run gets them without flags, and let the setup interview offer a short curated list.

### 9. Start from the ticket

All three create a workspace straight from a GitHub issue, a Linear issue or a pull request (emdash also Jira, GitLab, Asana and others; Superset also from a Slack message). emdash's automations turn a cron schedule into ordinary tasks, with a history of runs.

**Branchyard (done, [pull requests](pull-requests.md#starting-from-an-issue)):** `by run --issue <url|#n|n> ["more instructions"]` (and `by fan`, `by spawn`) fetch the issue through `gh`, name the branch `issue-<n>-<slug>`, use the issue under a header as the prompt, and record the link, so the pull request `by pr` opens says `Closes #n`.

**Since** (Wave 2, [other trackers](pull-requests.md#other-trackers)): `--issue linear:KEY`, `jira:KEY`, `gitlab:GROUP/PROJECT#N` or the issue's URL, fetched from Linear's GraphQL API, Jira's REST v3 (its rich text rendered as Markdown, ported from Orca) and GitLab's REST API (mapped as emdash's issue plugins map them), with tokens from the environment or a connector gateway tool, and the pull request naming the issue the way each tracker understands; and `--pr N`, starting a branch from a GitHub pull request's head, which `by pr` then updates ([starting from a pull request](pull-requests.md#starting-from-a-pull-request)). Tested against local mock servers only.

**What remains:**
- Asana and the other trackers emdash reads.
- Scheduling belongs to the server (a webhook-triggered or scheduled operation) rather than a desktop app.

### 10. Notice when an agent needs you

- Superset: status in the sidebar (working, needs input, blocked, done), a completion sound and dock badges.
- emdash: notifications from lifecycle hooks (*unverified page*).

**Branchyard:**
- Records `awaiting_input`, stall and permission events.
- Delivers webhooks from the server.
- Shows statuses in `by watch`.
- Tells no one on the desktop. **Take:** an opt-in desktop notification (and terminal bell) from `by watch` and from a waiting `by run` when a branch asks, stalls, fails or finishes.

**Status:** done. `by watch` and a waiting `by run`, `by fan`, `by send` or `by fork` ring the terminal bell and write an OSC 9 or OSC 777 desktop-notification escape (chosen from the terminal, passed through tmux) when a tool waits for permission, a branch asks or escalates, a turn stalls, or a branch fails, is blocked, is interrupted or finishes; each event once, and never for the history `by watch` reads at start. `[notify] desktop = true` adds `notify-send` or `osascript`; `--no-notify` or `[notify] enabled = false` turns it off. On by default, since an escape a terminal does not know is ignored. Not done: a sound of its own, or a badge.

### 11. Leave for a real editor in one click

Superset and emdash open the workspace in VS Code, Cursor, JetBrains, Xcode or a terminal (⌘O in emdash).

**Branchyard (done, [pull requests](pull-requests.md#by-open)):** `by open <branch> [--editor code|cursor|zed|…] [--print]`, using `$VISUAL`, then `$EDITOR`, and refusing with the known names when none is set. `o` in `by watch` opens the selected branch's worktree the same way, leaving the dashboard's screen for a terminal editor (vim, nvim, emacs, hx, …) until it exits.

### 12. Keyboard-first

- emdash: ⌘K palette; ⌘N new task; ⌘J terminal drawer; remappable ([shortcuts](https://emdash.com/docs/keyboard-shortcuts)).
- Superset: ⌘/ palette; remappable.
- Conductor: ⌘/ shortcut sheet; a key to cycle reasoning effort.

**Take:** the ratatui `by watch` under way should include:
- a `?` help sheet;
- `/` to filter;
- keys to send, steer, merge, open, fork and resume without leaving the view.

**Status:** done. `by watch` has the `?` sheet and `/` filter, and keys on the selected branch: `s` send (an input box), `S` steer, `R` resume, `x` cancel and `m` merge (each after a yes; the merge's check result is shown), `f` fork, `d` diff and `l` log in scrollable panes, `y`/`Y` copy the name or worktree path, `p`/`P` `by pr` (and `--watch`), `o` `by open`, `r` `by rewind` (from the checkpoint list), `c` `by compare` and `t` `by try` (a toggle). Each runs the existing `by` command. The keys live in one table (`crates/branchyard-cli/src/watch/actions.rs`) that drives the handling, the footer and the `?` sheet. Not done: a command palette and remappable keys.

## What they struggle with, and Branchyard already handles

| Their weak spot | Evidence | Branchyard |
|---|---|---|
| No limit on spend; running N agents costs N times as much, untracked | Conductor review ([madewithlove](https://madewithlove.com/blog/conductor-running-multiple-ai-coding-agents-in-parallel/)); no cost page found in emdash's docs | Budgets per task, per fan-out and per delegation envelope; `cost_usd` on each branch; per-tenant `max_cost_usd` |
| Agents run with the user's full permissions ("how do you stop them going rogue?" went unanswered) | emdash Show HN ([47140322](https://news.ycombinator.com/item?id=47140322)) | Per-invocation permission policies, sandbox providers (Substrate, Microsandbox), secrets provisioned without appearing in prompts |
| Lifecycle scripts executed without a trust decision | Superset advisory, fixed in [PR #7830](https://github.com/superset-sh/superset/pull/7830) | A repository's `[workspace]` scripts run only once trusted per repository and content; servers run only what their operator allows ([workspace](workspace.md#trust)) |
| Slows down at scale ("78 tasks and the UI is crawling") | emdash Show HN ([47140322](https://news.ycombinator.com/item?id=47140322)) | A store and server built for many branches; several servers on PostgreSQL |
| Closing a tab kills the agent | Superset [#3240](https://github.com/superset-sh/superset/issues/3240) | Turns are durable and recoverable |
| Tied to one machine or one operating system | Conductor macOS only; Superset without Windows | CLI and server on Linux and macOS; remote mode; a server others can share |
| Agents can't coordinate | None of the three offers delegation, messaging or dependencies between agents | Delegation trees, inbox with steering, task graphs, rigs |

The recurring sceptical comment about all three: native agent CLIs keep absorbing orchestration, so a separate workbench may lose its reason to exist ([emdash Show HN](https://news.ycombinator.com/item?id=47140322), [Conductor Show HN](https://news.ycombinator.com/item?id=44594584)). This is why Branchyard's plugin shape matters. Branchyard lives inside the harness as a skill and MCP tools, and as a durable server behind it, rather than competing with the harness for the user's window.

## Plan

In order of what a user would feel first:

1. **Guided setup:**
   - `by init` and the `setup` skill (under way), which detect first and show files and commands before acting (§1).
   - `branchyard.toml` holding defaults, MCP servers (§8) and the workspace lifecycle below.
2. **Workspace lifecycle** (§2), **done** ([workspace](workspace.md)):
   - `copy`, `setup`, named `run` and `teardown`;
   - Branchyard-given variables with an allocated port;
   - a per-repository trust decision;
   - setup journaled and recovered.

   Left: a `shellSetup` for the harness, setup inside a sandbox. Concurrent run scripts and listening ports in `by watch` are done (Wave 2).
3. **From branch to merged PR** (§5, §9), done ([pull requests](pull-requests.md)):
   - `by run --issue`;
   - `by pr` and `by pr --watch`, which routes CI failures and review comments back into the branch;
   - a merge-readiness line in `by show`; `by open` (§11) came with it.
4. **`by watch` as the cockpit** (§4, §10, §11, §12):
   - interrupted branches with one-key resume (done);
   - notifications (done);
   - `by open` (done, the `o` key);
   - a keyboard sheet and actions (done), with `p`/`P` for pull requests, `r` rewind, `c` compare and `t` try bound after the three branches were integrated, and checkpoint and merge readiness in the detail pane.
5. **Compare and choose** after a fan-out (§3). Done: `by compare` ([checkpoints](checkpoints.md#compare-attempts)), and `c` in `by watch` compares the selected branch with its siblings.
6. **Per-turn checkpoints** with `by rewind` / `by fork --at` (§6), and **`by try`** (§7). Done in local mode ([checkpoints](checkpoints.md)), with `r` and `t` in `by watch`; remote rewind and `fork --at` remain.

**Where the plan stands** after the checkpoints, pull-request and cockpit branches were integrated: items 3 to 6 are done in local mode, and `by watch` binds every command they added. Item 1 has `by init`, the `setup` skill and `branchyard.toml` with `[mcp]` servers ([setup](setup.md)). Item 2, the workspace lifecycle, is the one left, in its own branch, to be integrated after these. What remains inside the done items is listed under each section above: remote rewind and `fork --at`, picking from the compare pane, and replying to review threads. Wave 2 added Linear, Jira and GitLab issues and `--pr` (§9), concurrent run scripts and listening ports (§2), and, from Orca, quota meters per login that the router and guard act on, and adopting a Claude Code or Codex session already on the machine as a branch ([usage](usage.md)).
