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

**Branchyard:** absent. A branch is a bare worktree, and `--check` runs only at merge time. Nothing copies untracked files, runs an install, allocates a port or tears anything down. This is the largest devex gap. **Take:**
- A `[workspace]` section in the new `branchyard.toml`:
  - `copy` (globs of untracked files to carry across);
  - `setup`, `run` (named, like Conductor's) and `teardown`.
- Branchyard-given variables `BRANCHYARD_BRANCH`, `BRANCHYARD_WORKTREE`, `BRANCHYARD_ROOT` and an allocated `BRANCHYARD_PORT`.
- A per-user override, as all three have.
- Setup is journaled like any other step, so a crash mid-install is recovered rather than repeated blindly.
- Superset's security advisory about lifecycle scripts (fixed in [PR #7830](https://github.com/superset-sh/superset/pull/7830)) is the warning that comes with this: a cloned repository's scripts must not run without a trust decision. Branchyard should ask once per repository and remember the answer, and servers should run only scripts their operator allowed.

### 3. The same prompt to several agents, compared side by side

Superset: press ⌘N again with the same prompt and a different agent, and you get parallel attempts to choose from ([first workspace](https://docs.superset.sh/first-workspace)). Conductor names the two patterns: several workspaces (fan out issues), or several agents in one workspace (an implementer and a reviewer on one branch) ([parallel agents](https://www.conductor.build/docs/concepts/parallel-agents)).

**Branchyard is ahead here:**
- `by fan` runs one prompt across harnesses, each on its own branch, under one budget.
- Rigs declare implementer and reviewer seats.
- Task graphs order dependent work.

**Take:** the review step after a fan-out. A `by compare` (or a view in `by watch`) should put the attempts side by side (diff stats, check results, cost, turns) and merge the chosen one with one keystroke.

### 4. Work survives closing the app

emdash autosaves terminal state and resumes agents where they left off ([tasks](https://emdash.com/docs/tasks)). Superset's terminals are backed by a daemon, so scrollback and processes survive restarts and updates.

**Branchyard is ahead on durability:**
- Leases, journaled steps, recovery after a crash.
- A server and workers that keep operations across restarts.
- Native session resume per harness.

**But what a person sees is weaker:** a turn whose engine died ends `interrupted`, and you resume it yourself. **Take:**
- `by watch` should show interrupted branches prominently, with a one-key resume (a `by send` to the branch, which resumes its native session).
- With a server running (`by serve` or `by worker`), local turns should outlive the terminal that started them. This is the daemon behaviour Superset has.

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

**Branchyard:** `by diff` and `by merge` with a validated `--check`, all local; nothing reaches a pull request. **Take:**
- `by pr <branch>`: push the branch, then open or update a pull request through `gh`, with a body built from the branch's task, turns and check results.
- `by pr <branch> --watch`: follow CI, and on failure or a review comment, `by send` the failure or comment back to the same branch. Branchyard's steering and inbox already deliver that message into a running turn.
- A merge-readiness line in `by show` and `by watch` (checks, CI, unresolved comments).

### 6. Undo one step, not the whole branch

Conductor snapshots each agent turn. Hovering a message and choosing revert discards that turn and everything after it ([checkpoints](https://docs.conductor.build/core/checkpoints), via a search summary). emdash and Superset offer only discarding uncommitted changes.

**Branchyard:** each turn's result is recorded in the event log, and `by fork` branches from a branch. There is no per-turn rewind. **Take:**
- Commit a checkpoint ref at each turn's end (`refs/branchyard/<branch>/turn-N`; cheap, since the turn already ends with a candidate commit).
- `by rewind <branch> --to N` (or `by fork <branch> --at N` for a non-destructive version).
- Rewinding a harness's native session is not always possible, so where resume cannot follow, the rewound branch starts a fresh session with a summary, and says so.

### 7. Try the agent's branch in the app already running

Conductor's Spotlight commits a workspace's tracked changes as a checkpoint and checks them out at the repository root, where the user's dev server and Docker stack are already running with hot reload. It syncs one way and restores the root when turned off ([spotlight testing](https://www.conductor.build/docs/reference/scripts/spotlight-testing)).

**Branchyard:** absent. **Take:** `by try <branch>` and `by try --off`.
- It applies the branch's diff to the main checkout only when that checkout is clean.
- Branchyard records what it changed, so `--off` restores the checkout exactly.
- It refuses when the user has uncommitted work.

### 8. One MCP definition, written into every agent's own config

emdash's Library holds a catalog of 54 MCP servers. Adding one writes it into each agent's native file (`~/.claude.json`, `~/.cursor/mcp.json`, `~/.copilot/mcp-config.json`, …) ([MCP](https://emdash.com/docs/library/mcp)).

**Branchyard:** `--mcp` already provisions an MCP server into each harness's native configuration, per branch, inside that branch's home ([provisioning](provisioning.md)), without touching the user's global files. **Take:** let `branchyard.toml` list MCP servers once, so every run gets them without flags, and let the setup interview offer a short curated list.

### 9. Start from the ticket

All three create a workspace straight from a GitHub issue, a Linear issue or a pull request (emdash also Jira, GitLab, Asana and others; Superset also from a Slack message). emdash's automations turn a cron schedule into ordinary tasks, with a history of runs.

**Branchyard:** absent. **Take:**
- `by run --issue <url|#n>`, which fetches the issue through `gh`, names the branch after it, uses the issue text as the prompt, and links the eventual pull request back to it.
- Scheduling belongs to the server (a webhook-triggered or scheduled operation) rather than a desktop app.

### 10. Notice when an agent needs you

- Superset: status in the sidebar (working, needs input, blocked, done), a completion sound and dock badges.
- emdash: notifications from lifecycle hooks (*unverified page*).

**Branchyard:**
- Records `awaiting_input`, stall and permission events.
- Delivers webhooks from the server.
- Shows statuses in `by watch`.
- Tells no one on the desktop. **Take:** an opt-in desktop notification (and terminal bell) from `by watch` and from a waiting `by run` when a branch asks, stalls, fails or finishes.

### 11. Leave for a real editor in one click

Superset and emdash open the workspace in VS Code, Cursor, JetBrains, Xcode or a terminal (⌘O in emdash). **Take:** `by open <branch> [--editor code|cursor|zed|…]`, using `$VISUAL` by default, and an `o` key in `by watch`.

### 12. Keyboard-first

- emdash: ⌘K palette; ⌘N new task; ⌘J terminal drawer; remappable ([shortcuts](https://emdash.com/docs/keyboard-shortcuts)).
- Superset: ⌘/ palette; remappable.
- Conductor: ⌘/ shortcut sheet; a key to cycle reasoning effort.

**Take:** the ratatui `by watch` under way should include:
- a `?` help sheet;
- `/` to filter;
- keys to send, steer, merge, open, fork and resume without leaving the view.

## What they struggle with, and Branchyard already handles

| Their weak spot | Evidence | Branchyard |
|---|---|---|
| No limit on spend; running N agents costs N times as much, untracked | Conductor review ([madewithlove](https://madewithlove.com/blog/conductor-running-multiple-ai-coding-agents-in-parallel/)); no cost page found in emdash's docs | Budgets per task, per fan-out and per delegation envelope; `cost_usd` on each branch; per-tenant `max_cost_usd` |
| Agents run with the user's full permissions ("how do you stop them going rogue?" went unanswered) | emdash Show HN ([47140322](https://news.ycombinator.com/item?id=47140322)) | Per-invocation permission policies, sandbox providers (Substrate, Microsandbox), secrets provisioned without appearing in prompts |
| Lifecycle scripts executed without a trust decision | Superset advisory, fixed in [PR #7830](https://github.com/superset-sh/superset/pull/7830) | Not applicable yet; design item 2 above with a trust step |
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
2. **Workspace lifecycle** (§2):
   - `copy`, `setup`, named `run` and `teardown`;
   - Branchyard-given variables with an allocated port;
   - a per-repository trust decision;
   - setup journaled and recovered.
3. **From branch to merged PR** (§5, §9):
   - `by run --issue`;
   - `by pr` and `by pr --watch`, which routes CI failures and review comments back into the branch;
   - a merge-readiness line.
4. **`by watch` as the cockpit** (§4, §10, §11, §12):
   - interrupted branches with one-key resume;
   - notifications;
   - `by open`;
   - a keyboard sheet and actions.
5. **Compare and choose** after a fan-out (§3).
6. **Per-turn checkpoints** with `by rewind` / `by fork --at` (§6), and **`by try`** (§7).
