# Workspace lifecycle

A new branch is a fresh git worktree: no `.env`, no `node_modules`, no running database. `[workspace]` in `branchyard.toml` makes each new worktree ready before the harness's first turn, and cleans up when the branch is removed. It is Branchyard's answer to emdash's `.emdash.json`, Superset's `.superset/config.json` and Conductor's scripts ([devex](devex.md#2-one-committed-file-makes-every-new-worktree-ready-to-work)), with two differences: a repository's scripts never run until you trust them, and setup is a journaled step, so a crash mid-install is recovered rather than forgotten.

## The file

```toml
[workspace]
copy = [".env", ".env.*", "config/*.local.json"]   # untracked files carried from the repository root
setup = "pnpm install --frozen-lockfile"             # or a list, run in order
teardown = 'docker compose -p "by-$BRANCHYARD_BRANCH" down'

[workspace.run.dev]
command = "PORT=$BRANCHYARD_PORT pnpm dev"
default = true

[workspace.run.worker]
command = ["pnpm build", "pnpm worker"]
```

| Key | What it does |
|---|---|
| `copy` | Globs relative to the repository root. Each regular file they match that git does not track is copied into the new worktree, at the same path; a matched directory is copied whole. See [what is copied](#what-is-copied). |
| `setup` | One command or a list. Each runs with `sh -c` in the new worktree, in order, before the branch's first turn; the first that fails stops the rest and fails the branch. |
| `run.NAME` | A named command (`command`, one or a list) for `by workspace run`; `default = true` marks the one run when no name is given (at most one). Never run by the engine. |
| `teardown` | One command or a list, run in the worktree when the branch is removed (`by rm`, `by merge --rm`, `Yard::remove`, `DELETE …/branches/{branch}`). Best-effort: a failure is recorded and the removal goes on. |

The table is read strictly with the rest of the file: an unknown key, an empty command, a run script's name other than letters, digits, `-` and `_`, two defaults, or a copy glob that is absolute, starts with `~`, contains `..`, or reaches into `.git` or `.branchyard` is an error naming its key, and `by config validate` reports it. `by init project` suggests a section from the repository's files ([setup](setup.md#topics)): a lockfile's install command (`pnpm install --frozen-lockfile`, `yarn install --frozen-lockfile`, `bun install --frozen-lockfile`, `npm ci`, or `npm install`), `cargo fetch`, `uv sync`, `poetry install` or a virtual environment for `pyproject.toml`, `go mod download`, a Compose stack per branch (`docker compose -p "by-…" up -d`, and `down` as teardown), `PORT=$BRANCHYARD_PORT … dev` as the run script, and the `.env` files it finds to copy.

An empty `[workspace]` table is allowed: it copies and runs nothing, and still reserves each new branch a port.

### Your own override

The user configuration (`~/.config/branchyard/config.toml`) may replace one repository's section, keyed by the repository root:

```toml
[projects."/home/me/src/app".workspace]
setup = "npm ci --prefer-offline"
```

Precedence is whole-section, not key by key: when the user file has an entry for the repository's canonical root (`~/` expands to your home), it replaces the repository's `[workspace]` entirely, and needs no trust (it is your file). Otherwise the repository's own section applies. `[workspace]` at the top of the user file, and `[projects]` in a repository's `branchyard.toml`, are refused: each belongs in the other file. The server reads only the repository's file.

## What a script gets

Setup, teardown and run scripts, and the harness on every turn, get:

| Variable | Value |
|---|---|
| `BRANCHYARD_BRANCH` | The branch's name. It is also how `by` inside a harness knows it runs on a branch: a script that calls `by` is treated the same way (no configuration files, no trust decisions, delegation only with a token). |
| `BRANCHYARD_WORKTREE` | The branch's worktree, an absolute path. |
| `BRANCHYARD_ROOT` | The repository root, where `.branchyard/` is. |
| `BRANCHYARD_PORT` | A TCP port reserved for the branch, from 20000 to 29999. |

A local or isolated branch's scripts run as you, on the host, with your environment minus every inherited `BRANCHYARD_*` variable (an outer harness's delegation token included) and the launching Claude Code session's variables: an isolated branch's harness is isolated, its setup is not. A sandboxed branch's (Microsandbox or Substrate) setup and teardown run **inside its sandbox**, through the provider's `exec`, with `sh -c` in the worktree as the sandbox sees it, so what setup installs outside the worktree lives in the sandbox and its [snapshots](sandbox-snapshots.md#setup-inside-the-sandbox); they get `BRANCHYARD_BRANCH`, `BRANCHYARD_WORKTREE` (the path in the sandbox) and `BRANCHYARD_PORT`, not `BRANCHYARD_ROOT`, and only the sandbox's own environment besides. Copying is still done on the host, into the worktree the sandbox mounts or is sent. A sandboxed harness does not get `BRANCHYARD_WORKTREE` or `BRANCHYARD_PORT`, whose values are host paths and ports.

### The port

The port is reserved in the store when the branch's setup first runs (or when `by workspace run` or `Yard::workspace_env` first asks), in one transaction that picks the first port, from a starting point hashed from the repository and branch, that no branch in the store holds and nothing on 127.0.0.1 is listening on. It is the same on every later turn and after the engine restarts, and it is released in the transaction that deletes the branch. On SQLite ports are unique per repository; on PostgreSQL they are unique across every repository in the database, so repositories served from one host never collide. Two repositories with separate SQLite stores on one host rely on the hashed start and the listening check, which a server that is not running yet cannot see.

## Trust

A cloned repository's scripts are code you have not read. Superset's advisory about exactly this ([PR #7830](https://github.com/superset-sh/superset/pull/7830)) is why no script from a repository runs until you trust that repository's section:

- The trust record is per user, in `trusted-workspaces.json` beside the user configuration (`BRANCHYARD_TRUST_FILE` names another), mode 0600. It maps a repository's canonical root to the SHA-256 of its `[workspace]` section in canonical form (every glob and command in order; one command and a list of one are the same). Changing any glob or command changes the digest: the scripts are refused again until you trust them again.
- A section with no commands (only `copy`) needs no trust, and neither does your own user file's entry.
- `by run`, `fan`, `fork`, `reincarnate` and `rig run`, which create branches, check it before creating anything. On a terminal they show the scripts and ask once (`trust these scripts for …? [y/N]`); a yes is recorded, a no creates nothing. Without a terminal they refuse, exit 1, and point to `by workspace trust`. `by send` creates no worktree and asks nothing.
- `by workspace trust` trusts the section as it is now; `by workspace untrust` forgets the decision; `by workspace show` shows the section, where it came from, its digest and whether it is trusted.
- A harness on a branch (`BRANCHYARD_BRANCH` set) can never trust anything, and its `by` reads no configuration: a delegated child gets its parent's workspace, as decided when the parent was created.
- The SDK takes no decision: `TaskOptions::workspace` runs what its caller passes.
- A server runs a repository's scripts only when its operator allows them in the server's configuration, never because a request asks (a request with a `workspace` field is refused as an unknown field):

  ```json
  { "allow_workspace_scripts": ["app"] }
  ```

  `true` allows every served repository, a list names some; naming a repository the server does not serve is a configuration error. For an allowed repository, the server reads `[workspace]` from its `branchyard.toml` when it creates a branch (task, fan, fork, reincarnation). For every other repository, its yard refuses scripts outright (`Yard::deny_workspace_scripts`): a branch that still needs setup fails saying why, and a teardown is skipped and recorded, even for a branch created locally with trusted scripts.

## Lifecycle

| When | What happens |
|---|---|
| A branch's worktree is created: `run`, `fan`, `fork`, `reincarnate`, a delegated `spawn`, a rig seat, a dependent started by its task graph | The workspace is stored with the branch: the caller's, or for a fork, reincarnation or delegated child without one, its parent's. Before the first turn's harness starts, in the turn: the port is reserved, files are copied, setup runs. Each phase is an `Activity::Workspace` event (`workspace` in `by log --json`). |
| Setup fails, a copy glob is refused, or a file cannot be copied | The branch ends `failed` (`workspace setup failed: \`…\` exited with status 7; its output is in the branch's log`), with the command's combined output, at most its last 16 KiB, in the event. No prompt was submitted. A cancel during setup kills it and ends the branch `interrupted`. Setup runs at most 30 minutes. |
| A later turn (`by send`) | Nothing, once setup has completed. A branch whose setup did not complete runs copy and setup again, from the start, first. |
| A sandboxed branch whose sandbox was branched from another's provider snapshot (a fork, rewind or delegated child), or a `by fan` whose setup ran once in a prepared sandbox | Setup does not run: the branch inherits the source's, when it has the same copy and setup. What that setup left in the source's worktree that git does not track, and each copied file it changed, is copied into this one (Microsandbox), or came with the snapshot (Substrate); the `workspace` event names the source (`inherited_from`). See [sandbox snapshots](sandbox-snapshots.md). |
| `by workspace run [BRANCH] [NAME]` | The named run script (or the default one, or the only one) runs in the branch's worktree in the foreground with its output on your terminal, and its exit status is `by`'s. `--detach` starts it in its own process group with output to `.branchyard/logs/BRANCH.NAME.log` and returns its pid. Each start and end is a `run` event in the branch's log. Inside a harness the branch defaults to `$BRANCHYARD_BRANCH`. The run script must be trusted. |
| The branch is removed | Teardown runs in the worktree (at most 10 minutes) before it is deleted, as part of the journaled removal; its report is recorded, printed by `by rm`, and returned by `Yard::remove_reporting`. The port is released with the branch. |

Background processes a setup command starts in its own process group are stopped when the command ends (they would hold its output open); start long-running services with a run script, or with a tool that daemonizes (`docker compose up -d`).

### What is copied

- Only paths inside the repository: a glob that could leave it is refused when the file is read, and again by the engine; a match reached through a symbolic link to a directory outside the repository is refused.
- Symbolic links are never copied, and never followed: each is refused and named in the event. Copy the file itself, or create the link in setup.
- Files git tracks are skipped: the worktree already has the branch's version, and a local edit at the root must not leak into it. `.git` and `.branchyard` are never copied.
- A destination whose directory in the worktree is a link, or that is itself a link or a directory, is refused.
- Copied files are left out of every snapshot of the branch, even when no ignore rule covers them, so a `.env` never reaches a candidate or a merge. A directory a glob matched is recorded (and left out) as one path when the branch tracks nothing in it; otherwise each copied file is, so the agent's edits to tracked files beside them still reach the candidate. Files setup creates are not left out: ignore them in `.gitignore` as you would in any checkout (`node_modules/`, `.venv/`).

## Durability

Copy and setup are one journaled step, `setup`, in the turn that runs them ([durable execution](durability.md#journaled-steps)): its intent (globs, commands, port, the spawn marker and host) is recorded before anything is copied, its outcome after. Every setup command is recorded as a process of the turn and carries the turn's `BRANCHYARD_SPAWN` marker. When an engine dies mid-setup, recovery kills the setup's process group and everything carrying its marker, records `… before the harness was started; the turn never ran; its workspace setup was cut short and runs again, from the start, before its next turn`, and ends the branch `interrupted`. It does not run setup itself. The branch's record still says its workspace is not ready, so its next turn (`by send`, which starts a fresh session because no prompt was ever submitted) runs copy and setup again before the prompt. Setup commands must therefore be idempotent, which install commands are. The alternative, failing the branch, would leave a person to recreate it for what is usually a killed terminal.

A removal is journaled already; a teardown cut short by a crash is not repeated by recovery, and repeating the removal runs it again.

## Surfaces

| | SDK | by | by --remote | HTTP |
|---|---|---|---|---|
| Create branches with a workspace | `TaskOptions::workspace` | from `branchyard.toml`, once trusted | the server's own configuration | the server's own configuration |
| Trust | the caller's decision | `by workspace trust`, `untrust`, a terminal prompt | refused: the operator decides (`allow_workspace_scripts`) | n/a |
| Show | `Yard::workspace(branch)` | `by workspace show [BRANCH] [--json]` | refused | branch events |
| Run a named script | `Yard::workspace_env` for the variables | `by workspace run [BRANCH] [NAME] [--detach]` | refused | n/a |
| Teardown | `Yard::remove_reporting` | `by rm`, `by merge --rm` | `by rm`, `by merge --rm` (the server's decision) | `DELETE …/branches/{branch}` |

## What is tested

- **Config** (`branchyard-setup`): the section parses and renders back, run-script choice, every refusal (escaping globs, empty commands, bad names, two defaults, unknown keys, relative `[projects]` keys), the digest changing with any glob or command and not with one-versus-list spelling, the user entry replacing the repository's, each section in its own file, and both surviving the key-by-key merge. The project topic's detection (lockfiles, Cargo, Compose, `.env` files), its plan parsed by the real parser, and a golden batch (`tests/golden/project-workspace.json`); the schema regenerated and compared.
- **Store** (`conformance.rs`, SQLite and PostgreSQL): a branch's port stable, never shared, `usable` honored, wrapping, none left when no port is usable, released with the branch, and six engines reserving at once getting six ports.
- **Engine** (`crates/branchyard/tests/workspace.rs`, the fake ACP agent): copying (ignored, untracked and nested files; a tracked file left alone; a symbolic link, a linked directory outside the repository and a `..` glob refused) with copied files absent from the candidate, and a copied directory the branch also tracks files in never hiding the agent's edits to them; a refused copy failing the branch before its harness starts; setup's variables (branch, worktree, root, port, no delegation token) and the harness's; a failing setup failing the branch with its exit code and output and no prompt; the port stable across turns and a reopened store, distinct between branches and for a fork, and setup run once per worktree; teardown's variables and output on removal, a failed teardown not stopping it, the port released and the event kept in the feed; a yard denying scripts; a delegated child set up in its own worktree with its own port; and an engine killed with SIGKILL mid-setup: its setup's process killed by recovery, the branch `interrupted` with the reason, and the next turn running setup again, then the prompt. On PostgreSQL (`tests/postgres.rs`): ports distinct and stable across engines, and teardown on removal.
- **CLI** (`crates/branchyard-cli/tests/workspace.rs`, the built `by`): untrusted scripts refused without a terminal and nothing created, `by workspace trust` (a 0600 record), a changed script refused again, a send not asking, `untrust`; copy-only and a user entry needing no trust, and misplaced sections refused by `by config validate`; a harness never trusting; `by workspace run` by default and by name, a failing script's exit status, an unknown name, `$BRANCHYARD_BRANCH` inside a harness, `--detach`, and every phase in `by log --json`; teardown on `by rm` and `by merge --rm`; and on a pseudo-terminal, the prompt answered yes (remembered) and no (nothing created). Argument parsing for `by workspace` and `merge --rm`.
- **Server** (`crates/branchyard-server/tests/workspace.rs`): without `allow_workspace_scripts` a repository's section is ignored and a request's `workspace` field refused; with it, setup and copy run and teardown runs on `DELETE`; a locally created branch's teardown is skipped by a server that does not allow scripts; a configuration naming a repository it does not serve is refused, and `true` parses.
- **In a sandbox** (`crates/branchyard/tests/snapshots.rs`, a fake provider standing in for Microsandbox): setup exec'd in the sandbox with `sh -c` in `/workspace`, told the worktree's sandbox path, its outputs inside and outside the worktree seen by the harness, inherited by a fork branched from the branch's snapshot, and run once for a fan (per branch when the provider cannot live-branch), a copied `.env` it appended to reaching every fan member as it left it; teardown in the sandbox told the branch's port.
- **Not tested**: macOS (process groups and the start-time check use `ps` there), a real package manager or Docker, a real harness, a real Microsandbox or Substrate sandbox, and concurrent reservations from separate SQLite stores on one host.
