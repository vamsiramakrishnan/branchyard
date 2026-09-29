# Sandbox snapshots

Git is how Branchyard branches code: every turn ends in a [checkpoint](checkpoints.md) ref, and a fork, a rewind, a delegated child or a fan starts its worktree from a commit. A sandbox provider can branch more than code. Microsandbox can pause a running microVM, and branch a running or paused one into children that keep its memory and processes; Agent Substrate can pause an actor, suspend it into a snapshot, tag the snapshot and create new actors from the tag. This page describes the layer that uses those operations *under* the git-based ones: a branch keeps its sandbox between turns, each checkpoint takes a provider snapshot too, and a new branch starts from the snapshot of the checkpoint its worktree starts from. Git and the `[workspace]` setup stay the universal fallback, and every turn says which path it took.

> **Status.** Implemented and tested against fake providers only: `branchyard_sandbox::fake::FakeProvider` standing in for Microsandbox, and Substrate's in-process fake control plane. Nothing here has run on a KVM host or a Substrate cluster; every provider-level behaviour is **unqualified**. The Microsandbox operations are declared only behind an opt-in (`live_branch = true`). The ignored tests that would qualify them are listed [below](#qualifying-it).

## What it is for

- **A warm sandbox per branch.** Without it, every turn boots a fresh microVM or actor and destroys it at the end. With `keep = "pause"`, the sandbox is paused instead and the next turn resumes it: toolchains, caches, a database or dev server started by setup, anything outside the worktree, are still there.
- **Setup once.** `[workspace]` setup runs inside the sandbox, so what it installs outside the worktree lives in the sandbox and in its snapshots. A fork from a snapshot inherits it instead of running it again; `by fan` runs it once for every harness.
- **Forks and rewinds that keep the environment.** `by fork --at N`, `Branch::fork`, a delegated child, a rig seat, a graph dependent and `by rewind --to N` get a sandbox branched from the provider snapshot of the checkpoint their worktree starts from, rebound to their own worktree and home.

This is the pattern the mario-never-dies probe of Microsandbox exercises: a paused sandbox branched into children that keep its processes (the same PIDs), whose writes stay private, while the paused source stays paused.

## The provider contract

`SandboxProvider` ([`provider.rs`](../crates/branchyard-sandbox/src/provider.rs)) gains four optional operations, each returning `Unsupported` by default and declared in `Capabilities`:

| Operation | What it does | Declared by |
|---|---|---|
| `pause(name)` | Freeze a running sandbox in place, keeping memory and processes. Pausing a paused one is not an error | `Capabilities::pause` |
| `resume(name)` | Continue a paused sandbox, or one a checkpoint left stopped, ready for `exec`, from this process or another | `Capabilities::pause` |
| `branch_live(source, children)` | One new sandbox per `SandboxSpec` from a running or paused source, each with the source's memory, processes and root disk, and with its **own** name and mounts: a child's worktree and home are rebound through its spec. The source keeps its state; a paused one stays paused, so every child is captured at one point. One result per child | `Capabilities::live_branch` |
| `release_checkpoint(checkpoint)` | Release what the provider holds for a checkpoint | a declared checkpoint |

`SandboxSpec::persist` asks that a sandbox outlive the provider value and its process (Microsandbox: created detached), so a later `by send` resumes it. `SandboxState::Paused` is a state of its own.

The engine chooses by capability, never by a provider's name: `Capabilities::has(Feature)` with `LIVE_BRANCH`, `PAUSE` and `FULL_SNAPSHOT` (a checkpoint of `SnapshotScope::Full`), and `Requirements` and `admit` take `pause` and `live_branch` like the other operations.

## Per provider

| | Microsandbox (`live_branch = true`) | Agent Substrate | Fake (tests) |
|---|---|---|---|
| Pause, resume | `Sandbox::pause()`/`resume()`, or through `Sandbox::get(name)` and `connect()` from another process | `PauseActor` (the attempt is ended first); `ResumeActor`, a new attempt credential and the bridge's health check | a state change; host processes are not stopped |
| Live branch | `SandboxHandle::branch(name).volume(..).branch()` per child, or one `branch_many(names)` when every child has the same mounts (the SDK applies one set of mounts to a whole batch). Local only; a child keeps its source's CPUs and memory, so an image or limits in a child's spec are refused | not declared: Substrate has no fork of a running actor | copies the root filesystem |
| Snapshot of a kept sandbox | a live branch of the paused sandbox into a paused child named `<sandbox>-t<N>` | `SuspendActor` (a paused actor's node-local snapshot is uploaded), then `CreateTag` | a paused child |
| New sandbox from a snapshot | `branch_live` of that paused child | `CreateActor` with `source_tag`, created stopped, then `ResumeActor`: not live | `branch_live` |
| Full checkpoints | `Snapshot::builder(..).from_sandbox(..).full().create()`, branched with `Sandbox::restore(..).forked()` | the template's commit scope | declared |
| Release | destroy the child; `Snapshot::remove` for a checkpoint | `DeleteTag` | remove |
| Declared by default | exec and disk checkpoints only | exec, ingress, pause, checkpoint and branch with the template's scope | as constructed |

Children of a Microsandbox branch do not inherit host mounts or published ports; each is rebound to its own worktree at `/workspace`, its home at `/branchyard/home`, the repository's git directory read-only, and its scratch areas. Substrate actors mount nothing: a resumed or branched actor's old worktree repository and the files git sees in it are removed (ignored files, such as what setup installed, stay), its home is removed, and this host's worktree and home are sent again as bundles and trees.

## Keeping a branch's sandbox

```toml
# branchyard.toml
[microsandbox]
image = "ghcr.io/you/claude-code:2.1"
live_branch = true      # opt in to pause, live branch and full snapshots (unqualified)
keep = "pause"          # "destroy" (the default) or "pause"
snapshots = 3           # checkpoints that also keep a provider snapshot (default 3; 0 for none)
max_paused = 4          # kept sandboxes per repository before the least recently used goes (default 4)
```

The same as flags, on `by run`, `by fan` and `by fork` with `--provider microsandbox` or `--provider substrate`: `--keep-sandbox pause`, `--sandbox-snapshots N`, `--max-paused N`, and `--live-branch` (Microsandbox only). In the SDK they are fields of `SandboxOptions` and `SubstrateOptions` (`keep: SandboxKeep`, `snapshots`, `max_paused`, and `SandboxOptions::live_branch`), stored with the branch like the rest of its provider.

When a turn ends and the branch keeps its sandbox:

1. If the provider cannot pause, the sandbox is destroyed as before and the turn records `sandbox: not kept (provider can't pause: …)`.
2. Otherwise the step `sandbox_park` is journaled, the sandbox is recorded in the store (a `kept` row: branch, incarnation, provider, name, last used), and paused. A pause that fails removes the row and destroys the sandbox. The turn records `sandbox: kept paused for the next turn`. A Substrate actor's worktree and home come back first, as always.
3. **Eviction.** If the store now holds more than `max_paused` kept sandboxes of the same provider (for Substrate, the same endpoint, atespace and template), the least recently used are destroyed, except this branch's, each recorded on this turn's log as `sandbox: destroyed <branch>'s kept … sandbox …, the least recently used, to stay within max_paused`. On PostgreSQL the count is per repository scope.

The next turn **takes** the row (deletes it; whoever deletes it owns the sandbox, so a resuming turn and an eviction never both act on it) and resumes the sandbox: `sandbox: resumed the kept … sandbox …`. A row whose sandbox no longer exists, or cannot be resumed, falls back to a fresh sandbox, recorded as `sandbox: fresh (its kept sandbox … no longer exists)`. `by rm` destroys the kept sandbox and releases every snapshot; `by merge` destroys the kept sandbox (a merged branch takes no more turns) and keeps the snapshots for forks until the branch is removed; a rewind destroys it (below).

The rows are a store table, `sandboxes` in SQLite and `by_sandboxes` in PostgreSQL, deleted with their branch. They are not part of the branch's record, so any engine, in any process, finds them.

## A snapshot with every checkpoint

After a turn's checkpoint ref is written (the `checkpoint` step), a branch with a kept sandbox also takes a provider snapshot when the provider can, journaled as the step `sandbox_snapshot` before anything is taken:

- live branch and pause declared: the paused sandbox is live-branched into a paused child named for the checkpoint;
- otherwise a checkpoint the provider can also branch from (a full one preferred): Substrate suspends the paused actor and tags it;
- otherwise nothing, recorded as `sandbox: no snapshot with checkpoint N (provider can't branch: …)`.

The snapshot is recorded on the checkpoint event (`Checkpoint::sandbox`, additive, in `schema/contract.json`):

| Field | Meaning |
|---|---|
| `provider` | `microsandbox` or `substrate` |
| `handle` | the paused sandbox's name, or the checkpoint reference (a Substrate tag) |
| `scope` | `full` or `disk` |
| `consistency` | `crash` (always, today) or `application` |
| `method` | `live_branch` or `checkpoint` |

and as a `snapshot` row in the store with the checkpoint's commit. Only the newest `snapshots` per branch are kept; older ones are released and recorded as `sandbox: released checkpoint N's … snapshot …`. An engine that stops mid-snapshot leaves a pending `sandbox_snapshot` step; recovery destroys the planned child.

A snapshot holds the sandbox, not the code: bound worktrees and homes are on this host and are not in a Microsandbox snapshot. The worktree of anything branched from it comes from git.

## Using the snapshots

A branch created from another's checkpoint records a seed: the source branch, the checkpoint (when known) and the commit its worktree starts from. Its first turn without a kept sandbox branches from the source's snapshot at that commit, on the same provider, and clears the seed:

| Where | Seed |
|---|---|
| `by fork --at N`, `Branch::fork_at` | the parent's checkpoint N |
| `by fork`, `Branch::fork` | the parent's current checkpoint, at its candidate |
| A delegated child, a rig seat, a graph dependent | the parent's snapshot at the child's base, resolved when the child starts ([graph](graph.md)) |
| `by rewind --to N`, `Branch::rewind` | the branch's own checkpoint N; its kept sandbox, which holds a later turn's state, is destroyed (`sandbox: not kept (the branch was rewound to checkpoint N)`) |
| `by reincarnate` | none |

Each is tried in order, and the first that works is used:

1. **Branch from the matching snapshot**: `branch_live` of the paused snapshot child (Microsandbox), or `branch` from the tag and `resume` (Substrate), with the child's own name and mounts. Recorded as `sandbox: branched from <branch>'s checkpoint N (microsandbox live branch)`.
2. **Today's path**: a fresh sandbox, the worktree from git, and the `[workspace]` setup, with the reason: `sandbox: fresh (provider can't branch: …)`, `(<branch> has no provider snapshot at checkpoint N)`, `(could not branch from …: …)`.

The event is `Activity::Sandbox(SandboxEvent::Started { provider, sandbox, origin })` with `origin` `fresh` (and its `reason`), `resumed`, `branched` (`branch`, `turn`, `method`) or `prepared`; `by` prints its line as the turn starts, and `by log --json` shows it as `{"activity": "sandbox", ...}`. It is the sandbox counterpart of the [session decision](checkpoints.md#sessions-native-where-it-ended-otherwise-a-summary), which is made independently: a branched sandbox does not continue the harness's session, and the harness is never cloned live (it is not running when a turn ends).

## Setup inside the sandbox

A sandboxed branch's `[workspace]` setup runs in its turn's sandbox, through the provider's `exec`, with `sh -c` in the worktree as the sandbox sees it (`/workspace`, or Substrate's workdir), after the sandbox exists and before the harness starts. Files are still copied on this host, into the worktree the sandbox mounts or is sent. Scripts get `BRANCHYARD_BRANCH`, `BRANCHYARD_WORKTREE` (the path in the sandbox) and `BRANCHYARD_PORT`, but not `BRANCHYARD_ROOT`, a host path. Teardown runs the same way at removal, in the branch's kept sandbox or a fresh one. A local branch's scripts run on the host exactly as before; trust, the journaled `setup` step (intent before effect) and its recovery are unchanged. The `workspace` event says where they ran (`ran_in`: `host` or `sandbox`).

What setup leaves in the worktree that git does not track (`node_modules/`, `.venv/`), and each copied file it changes (a `.env` it appends to), is recorded with the branch (`WorkspaceState::produced`). A branch whose sandbox was branched from another branch that ran the same copy and setup **inherits** it instead of running it: on Microsandbox those paths are copied from the source's worktree into its own, replacing the branch's own copy of a copied file (they are valid, since every sandbox sees its worktree at `/workspace`); on Substrate they came with the snapshot. The `workspace` event names the source (`inherited_from`). The source's worktree is read as it is when the child starts, not as of the checkpoint.

Without `keep = "pause"`, what setup installs *outside* the worktree and home goes with the sandbox at the end of the first turn, and setup does not run again: install into the worktree or home, or keep the sandbox.

## `by fan`: setup once

When every branch of a fan runs on one provider whose placement mounts the worktree (Microsandbox), that provider declares live branch, and the workspace has setup commands:

1. A sandbox is prepared on the first branch's worktree and home (attached to this process, so an engine that stops mid-fan leaves nothing behind), and the first branch's setup runs in it once, as its journaled `setup` step.
2. It is paused, and one `branch_live` makes a sandbox per branch, each rebound to its own worktree and home. Each branch's `sandbox` step is journaled first, so recovery can destroy it.
3. Every other branch inherits the setup: what it produced is copied into its worktree before any harness starts. The prepared sandbox is destroyed.

Each branch's turn records `sandbox: branched from the fan's prepared sandbox, set up once in <first>'s worktree (microsandbox live branch)`. If the provider cannot live-branch, each branch runs its own setup in its own sandbox and says so (`sandbox: fresh (fan setup runs in each branch: provider can't live-branch: …)`); if preparing fails, the reason is recorded the same way.

## Durability

| Step | Intent | Recovery |
|---|---|---|
| `sandbox` | the sandbox a turn resumes, branches or creates, before it does | destroys it (Substrate: brings the work back first), unless the turn had parked it, and removes a kept row naming it |
| `sandbox_park` | the sandbox being kept | a finished park is left alone: the next turn resumes it |
| `sandbox_snapshot` | the snapshot child to be made | a pending one is destroyed |
| `setup` | unchanged | unchanged; a sandboxed setup's processes go with the sandbox |

## Code and tests

| Where | What |
|---|---|
| [`branchyard-sandbox`](../crates/branchyard-sandbox/src/provider.rs) | the optional operations, `Feature`, `SandboxSpec::persist`, `SandboxState::Paused`; [`fake.rs`](../crates/branchyard-sandbox/src/fake.rs) |
| [`branchyard-microsandbox`](../crates/branchyard-microsandbox/src/provider.rs), [`plan.rs`](../crates/branchyard-microsandbox/src/plan.rs) | the SDK mapping, `capabilities_with(live_branch)`, `live_child`, `one_batch` |
| [`branchyard-substrate`](../crates/branchyard-substrate/src/provider.rs) | `PauseActor`, `ResumeActor`, suspend-then-tag, `DeleteTag`; `transfer::clear_for_push` |
| [`snapshots.rs`](../crates/branchyard/src/snapshots.rs) | keep, park, evict, snapshot, prune, seeds, acquire, inheritance |
| [`placement.rs`](../crates/branchyard/src/placement.rs), [`workspace.rs`](../crates/branchyard/src/workspace.rs), [`run.rs`](../crates/branchyard/src/run.rs) | the turn's sandbox, setup in it, the fan |

Tested hermetically:

- `branchyard-sandbox`: capability features and admission; the fake's live branch (root filesystem copied, writes private, a paused source staying paused, each child's own mounts), checkpoints and their release, undeclared operations refused.
- `branchyard-microsandbox`: the opt-in declarations, admission, children rebound and batched only with equal mounts, a child's image or limits refused, and, without KVM, the provider refusing pause and live branch when not opted in.
- `branchyard-substrate` against its fake cluster: pause and resume, a paused actor's checkpoint suspending then tagging, a branch created stopped from the tag and resumed while the source stays suspended, live branch refused, a released tag deleted.
- The engine against the fake provider (`crates/branchyard/tests/snapshots.rs`): a kept sandbox resumed across turns with a snapshot per checkpoint and everything released on removal; snapshots pruned to K; `fork --at N` branching from checkpoint N's snapshot with its own worktree mounted; a provider that cannot pause or branch falling back with the reason; a rewind restoring from its own snapshot; eviction beyond `max_paused`, least recently used first; a vanished kept sandbox replaced by a fresh one, recorded as such; setup exec'd inside the sandbox, seen by the harness, and inherited by a fork; a fan running setup once and one `branch_live` for every branch, a copied `.env` setup appended to reaching every member as setup left it, and running it per branch when the provider cannot live-branch; a delegated child starting from its parent's snapshot; the path following declared capabilities; a sandboxed teardown getting the branch's port.
- The engine against the Substrate fake cluster: an actor kept paused, resumed with the worktree sent again, tagged at each checkpoint, a fork created from the tag while the source stays suspended, and actors and tags deleted on removal.
- The store: the rows on SQLite and PostgreSQL (the shared conformance suite, including four engines racing to take one row), and on PostgreSQL a kept sandbox resumed and forked from by a second engine; recovery leaving a parked sandbox and destroying an unparked one.

## Qualifying it

Nothing above is evidence about a real provider. The ignored tests in [`tests/microsandbox.rs`](../crates/branchyard-microsandbox/tests/microsandbox.rs) mirror the mario-never-dies probe and need Linux with KVM and the `msb` 0.7.3 runtime ([providers](providers.md#prerequisites-and-the-ignored-tests)):

- `a_live_branch_child_keeps_the_sources_processes`: a process started in the paused source is alive, with the same PID, in the child;
- `a_live_branch_childs_writes_are_private_and_the_paused_source_stays_paused`: two children with their own workspaces; one's root-disk write is not seen by the other, the source stays paused and then resumes with its own state;
- `a_persisted_sandbox_is_resumed_by_another_provider`: a detached, paused sandbox resumed by a new provider value, as a later `by send` would.

Until they pass on a KVM host, keep `live_branch` off in anything that matters; with it off, Microsandbox declares neither pause nor live branch, and every path above falls back. Substrate's pause, suspend-from-pause, tag and create-from-tag semantics are the fake's reading of the proto and must be checked on a cluster ([Substrate qualification](substrate.md#qualification-steps)).

Not done:

- A fork of a *running* sandbox. Snapshots are taken at the end of a turn, with no harness running; a fork of a branch mid-turn uses its last checkpoint's snapshot.
- Setup outputs are copied from the source's worktree as it is when the child starts, not as of the checkpoint; ports a setup published are not rebound.
- Substrate fans run setup per branch (no live branch), and a Substrate teardown gets a fresh actor when none is kept.
- Microsandbox `branch_many` cannot give each child its own mounts, so children with different worktrees are branched one by one from the paused source.
