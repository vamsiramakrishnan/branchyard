# Prepared environments

A new branch's [workspace setup](workspace.md) usually installs the same dependencies the last branch installed: `pnpm install` on the same lockfile, `uv sync` on the same `uv.lock`. With `prepare = true`, setup runs once per **environment key** and every later branch with that key starts from what it produced, cloned where the filesystem can, linked where you ask, or, for a sandboxed branch, branched from a provider snapshot of a sandbox it ran in. A build that fails never replaces the last good one. This is Branchyard's version of Cursor's and Codex's cached environments and Devin's snapshots, with Orca's worktree sharing for directories.

> **Status.** Implemented and tested hermetically on Linux (ext4) against the fake ACP agent and, for sandboxes, the fake provider. On this host `FICLONE` is refused, so every restore exercised the byte-copy fallback; the clone path (Btrfs, XFS, bcachefs) and macOS `clonefile` have not run. Sandbox environments are **unqualified** against a real provider, as everything in [sandbox snapshots](sandbox-snapshots.md) is.

## The file

```toml
[workspace]
copy = [".env"]
setup = "pnpm install --frozen-lockfile"
prepare = true                              # setup once per environment key
inputs = ["pnpm-lock.yaml", "package.json"]  # what keys it (default: the common lockfiles present)
share = ["node_modules"]                    # linked from the environment, not copied
```

| Key | What it does |
|---|---|
| `prepare` | Run `setup` once per environment key and keep what it produced; a new branch with the same key restores it and runs no setup. Needs `setup`. |
| `inputs` | Globs, relative to the repository root, of the files setup reads. Their content is part of the key. Without it: `package.json`, `pnpm-lock.yaml`, `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, `bun.lock`, `bun.lockb`, `Cargo.lock`, `rust-toolchain.toml`, `pyproject.toml`, `uv.lock`, `poetry.lock`, `Pipfile.lock`, `requirements.txt`, `go.mod`, `go.sum`, `Gemfile.lock`, `composer.lock`, `mix.lock`, `flake.lock`, `.tool-versions`, `.nvmrc`, `.node-version` and `.python-version`, each where present at the top of the worktree. Checked like `copy` globs. |
| `share` | Literal relative paths (no globs) of directories setup produces that branches **link to** in the environment instead of getting their own copy: one install serves every branch. On this host only. |

`inputs` and `share` without `prepare` are errors, and `by config validate` says so. Turning `prepare` on (or changing `inputs` or `share`) changes the section's [trust](workspace.md#trust) digest, so the scripts are asked about again; a section without `prepare` keeps the digest it was trusted with.

## The key

An environment's **recipe** is a hash of its setup commands, its copy globs, its input globs and where it is built (`host`, or one sandbox provider: Microsandbox, or Substrate at its endpoint, atespace and template). Its **key** is the recipe with the content (BLAKE3) of every input file, read from the new branch's worktree, so from the commit it starts at. Change a lockfile, a setup command or a copy glob and the key changes; change nothing and every branch has the same key. `share` does not change the key: it changes how a result is placed, not what setup builds. `by env show` prints a key's inputs and their digests.

## What a branch does

Setup still runs inside the turn, as the journaled `setup` step, after files are copied. With `prepare`, it first looks under `.branchyard/environments/`:

| The key has | What happens | The `workspace` event's `environment` |
|---|---|---|
| A good environment | Every path setup produced is restored into the worktree: cloned where the filesystem can (`FICLONE` on Linux, `clonefile` on macOS), copied byte for byte where it cannot, and each `share` directory linked. Setup does not run. | `origin: restored`, `method` `clone`, `copy` or `link`, `built_by` |
| Nothing, and no one building it | The branch takes the key's lock, runs setup in its own worktree, and captures what it produced (see below), then restores it into its own worktree the same way. | `origin: built` |
| Nothing, but another branch is building it | It waits for the lock (polling, cancellable, at most as long as setup may run), then restores what was built. A fan of local branches builds once. | `origin: restored` |
| A recorded failure, and a good environment of the same recipe | The newest good one is restored, and the branch says so: `environment: using the last good build … (environment … failed to build in c: …; run `by env rebuild` to try again)`. Setup does not run again. | `origin: last_good`, `used`, `reason` |

**What setup produced** is what the workspace layer already records as `produced`: what setup left in the worktree that git does not track (a wholly new directory once, such as `node_modules`), and each copied file it changed (a `.env` it appended to). Capturing moves those paths into `.branchyard/environments/.staging-<key>-…/tree/`, writes a manifest, and renames the staging directory to `.branchyard/environments/<key>/`; only then does the environment exist. A restore replaces whatever the worktree has at those paths and git does not track.

**A failed build never replaces a good one.** When setup fails, the key is recorded as failed (`<key>.failed.json`, with the reason and the branch). If a good environment of the same recipe exists, the branch clears what its failed setup left, restores that one, records both events (the failed setup with its output, then the fallback) and goes on; otherwise it fails as any failed setup does. Later branches with the failed key restore the last good one without running setup again until `by env rebuild` succeeds.

**Shared directories** are symbolic links into the environment. They are left out of every snapshot of the branch, even when no ignore rule covers them (an ignore rule `node_modules/` does not match a link), removed (only if they are links) before the worktree is deleted, and an environment a branch links into is never pruned. Everything else restored is the branch's own: its edits never reach another branch.

### `.worktreeinclude`

A `.worktreeinclude` at the repository root (the cross-tool convention Claude Code and Orca use) names ignored files and directories to carry into every new worktree, one per line, `#` comments allowed. Branchyard honours it in the copy phase of every branch created with a workspace: `by` gives a new branch an empty one when the repository has a `.worktreeinclude` and no `[workspace]`, an SDK caller passes `TaskOptions::workspace` (an empty `WorkspaceSpec` will do), and a server does so for the repositories whose scripts it runs. Each entry that exists at the root **and** that git ignores is copied like a `copy` glob's match, with the same refusals (links, escapes, `.git`, `.branchyard`), and left out of snapshots. Globs and negations (`*`, `?`, `!`) are skipped and named in the event, as are entries git does not ignore; the file is ignored above 256 KiB, and entries after the first 1000. Nothing in it is a script, so it needs no trust. Copies are clones where the filesystem can.

## Sandboxes

A sandboxed branch's environment is the sandbox itself. Its key's place is the provider, so it never shares an environment with this host (an install for the sandbox's architecture is not one for the host's).

- **Built.** After setup succeeds in a branch's sandbox, if the key has no environment and nobody holds its lock, the running sandbox is live-branched into a paused child (`by-env-<key>-…`), which is kept as the key's environment, recorded with its provider options; what setup produced in a mounted worktree (Microsandbox) is captured on this host as for a local branch, and copied back. All of it is the journaled `environment` step.
- **Used.** A later branch of the key, whose setup has not run, gets its sandbox by `branch_live` from that snapshot, rebound to its own worktree and home; it is recorded as `sandbox: branched from prepared environment …` (`SandboxOrigin::Environment`), and its setup is inherited: what the environment holds for the worktree is copied in (Microsandbox) or came with the snapshot (Substrate). A kept sandbox or a fork's seed snapshot comes first; the environment is the third choice, before a fresh sandbox.
- **Last good.** A sandboxed setup that fails records the key's failure and fails that branch (its sandbox cannot be swapped mid-turn). Later branches of the key branch from the newest good environment of the same recipe and say so (`sandbox: branched from the last good environment …: environment … failed to build …`).
- **A fan** whose key already has an environment does not prepare its own sandbox: every member branches from the environment. Without one, the fan sets up once as before ([setup once](sandbox-snapshots.md#by-fan-setup-once)), and that setup builds the environment.
- **Providers.** Only a provider that can live-branch keeps an environment: a snapshot must not disturb the running sandbox, and a provider that can only checkpoint pauses it (a Substrate actor's attempt ends). Without live branch the event says `environment: … not kept (provider can't …)` and nothing changes. `share` does not apply in a sandbox (a link into this host's `.branchyard` would not resolve there).

## Commands

```sh
by env list                   # every environment and failure; the current key marked
by env show [KEY]             # inputs, setup, what it holds, its snapshot (default: the current key)
by env rebuild                # build the current key now, from HEAD; a failure keeps the last good one
by env prune [KEY...] [--keep N] [--older-than DAYS]
```

All take `--json`, and all act on a local repository (`--remote` is refused: a server's environments are on its host). `rebuild` runs setup, so it needs the section to be [trusted](workspace.md#trust) and never runs from a harness on a branch: it checks out `HEAD` into a temporary worktree (`.branchyard/environments/.build-…`), copies files, runs setup, and captures the result as the key's environment, replacing an existing one only on success. `rebuild` builds this host's environment; a sandbox environment is rebuilt by the next sandboxed branch after `by env prune KEY`.

**Pruning** removes a good environment that is beyond the newest `--keep` (default 3) of its recipe, or unused (no branch restored it) for more than `--older-than` days (default 14), and recorded failures older than that; it never removes the newest good environment of a recipe (the last good build), nor one a branch's worktree links into, and says which it kept and why. Named keys (or their first characters) are removed even when newest. A provider snapshot is released with its environment. Leftovers of stopped builds (`.staging-`, `.old-`, `.build-` directories whose key nobody is building) go too. Every successful build prunes with the defaults.

## SDK

| Call | What it does |
|---|---|
| `WorkspaceSpec::prepare`, `inputs`, `share` | The section's keys, stored with the branch |
| `Yard::environments()` | `Vec<EnvironmentInfo>`: key, recipe, place, state (`good` or `failed`), inputs with digests, setup, produced, built by and when, last used, reason, snapshot |
| `Yard::environment_key(&spec)` | The key a branch created now from the repository's checkout would have |
| `Yard::rebuild_environment(&spec)` | `EnvironmentBuild`: the environment, or `None` with setup's report |
| `Yard::prune_environments(keep, max_age, only)` | `EnvironmentsPruned`: removed and kept, each with why |
| `WorkspaceReport::environment` | `EnvironmentUse`: key, `origin` (`built`, `restored`, `last_good`, `not_kept`), `used`, `method`, `built_by`, `shared`, `reason`; in `schema/contract.json` |
| `SandboxOrigin::Environment` | `key`, `used`, `method`, `reason` |

The SDK takes no trust decision: `TaskOptions::workspace` runs what its caller passes, `prepare` included. A server prepares environments for a repository whose `[workspace]` it runs (`allow_workspace_scripts`), in that repository's `.branchyard/environments/`.

## Durability

| Step | Intent | Recovery |
|---|---|---|
| `setup` | unchanged, plus `prepare` | unchanged: the next turn runs copy and setup (or the restore) again from the start |
| `environment` | the key, the staging directory, the worktree, what setup produced, and for a sandbox the planned snapshot | removes the staging directory and destroys the planned snapshot; `removed the half-built environment …` in the recovery reason |

A restore cut short leaves the branch's workspace not ready; its next turn restores again, replacing what the first attempt placed. What a cut-short capture had moved out of the worktree is not moved back: setup did not complete, so the next turn runs it (or restores the key's environment, if another branch built it meanwhile). The key's lock is an advisory `flock` on `.branchyard/environments/.<key>.lock`, released when its process dies. `rebuild` is not journaled: a rebuild cut short leaves a `.build-` worktree and a staging directory that `prune` removes.

## Limits

- **Relocation.** A restored copy keeps file contents as they were in the worktree that built them. Tools that write that worktree's absolute path into what they produce (a Python virtual environment's scripts, some native builds) are not relocatable: share such a directory, rebuild per branch, or install outside the worktree in a kept sandbox.
- **Copies cost what they copy.** Where the filesystem cannot clone (ext4, tmpfs), a restore is a byte-for-byte copy of everything setup produced, the building branch's included: share large directories, or put `.branchyard/` on a filesystem that clones.
- **One host, one checkout.** Environments are files in the checkout's `.branchyard/`, not store rows: a server and its `by worker`s share them only on the same checkout, and the lock is not reliable over NFS.
- **Not tested.** A filesystem that clones (Btrfs, XFS, bcachefs), macOS, a real package manager, a real Microsandbox or Substrate sandbox.
- **Substrate** keeps no environment (no live branch); its branches set up as before.

## Code and tests

| Where | What |
|---|---|
| [`environments.rs`](../crates/branchyard/src/environments.rs) | keys, the lock, build, capture, restore, last good, sandbox snapshots, recovery, prune, rebuild |
| [`workspace.rs`](../crates/branchyard/src/workspace.rs) | the turn's setup with `prepare`; `.worktreeinclude` in the copy phase |
| [`snapshots.rs`](../crates/branchyard/src/snapshots.rs), [`placement.rs`](../crates/branchyard/src/placement.rs) | branching a sandbox from an environment; taking and releasing its snapshot |
| [`branchyard-workspace` `include.rs`, `materialize.rs`](../crates/branchyard-workspace/src/materialize.rs) | `.worktreeinclude` resolution; clone, copy and link, ported from Orca (MIT) and pinned in [`vendor/orca`](../vendor/orca) ([third-party](../THIRD_PARTY.md)) |
| [`env_cmd.rs`](../crates/branchyard-cli/src/env_cmd.rs) | `by env` |

Tested hermetically:

- `branchyard-workspace`: `.worktreeinclude` parsing, unsafe and pattern entries refused, only ignored existing entries resolved (a real repository); copies keeping modes and inner links and never overwriting, independent of their source; shared paths as links; only links removed.
- `branchyard-setup`: `prepare`, `inputs` and `share` parsed and rendered back, every refusal, and the digest unchanged without `prepare`.
- The engine (`crates/branchyard/tests/environments.rs`): setup once per key, a second branch restored with its own copy and no setup, a send running nothing, a new lockfile a new key; shared directories linked, absent from candidates although not ignored, one install serving both branches, never pruned while linked, unlinked on removal; a failed build falling back to the last good one with both events, the next branch not retrying and naming `by env rebuild`, a failed rebuild keeping the good one and a fixed one replacing the failure; a failed build with no good one failing the branch; a fan of two local branches building once; `.worktreeinclude`; pruning keeping the newest of a recipe; and an engine killed with SIGKILL, its pending `environment` step's staging directory removed by recovery.
- Sandboxes (`crates/branchyard/tests/snapshots.rs`, the fake provider): setup once in a sandbox kept as a paused snapshot, a later sandbox branched from it with setup's work inside and outside the worktree and no setup, a failed build recorded and the next branch using the last good snapshot with the reason, pruning releasing the snapshot; a provider without live branch keeping nothing, with the reason.
- The CLI (`crates/branchyard-cli/tests/workspace.rs`, the built `by`): `by env list`, `show` (by key prefix), `rebuild` refused until trusted then building, a branch restored with its shared link, `prune` keeping a linked environment and removing it once its branch is gone, and `--remote` refused.
