# Warm pools

A new branch waits for two things before its harness starts: a worktree (`git worktree add`, a checkout of the whole tree) and its [prepared environment](environments.md) (a restore of what setup produced, a copy where the filesystem cannot clone). A **warm pool** does both ahead of time. It keeps a configured number of ready worktrees per repository, each at the base with the current environment in place. A new branch takes one, renames it to its own path, and starts. This is Branchyard's version of Devin's and Codex's warm starts and Cursor's background agents.

> **Status.** Implemented and tested hermetically on Linux against the fake ACP agent, on SQLite and PostgreSQL 16. Worktree slots only: sandboxes are **not** pre-booted (see [Sandboxes](#sandboxes)). No real package manager, no filesystem that clones, and no timing claim: the tests assert that a branch used a slot, not that it started faster.

## The file

```toml
[workspace]
setup = "pnpm install --frozen-lockfile"
prepare = true
share = ["node_modules"]

[workspace.pool]
size = 2                 # ready worktrees kept (0 to 32)
labels = ["linux"]       # only a by serve / by worker carrying these keeps it filled
max_age_minutes = 1440   # a ready worktree older than this is discarded
max_behind = 20          # commits the base may move past a slot and it still be taken
base = "main"            # where slots are made (default: HEAD of the checkout)
```

| Key | What it does |
|---|---|
| `size` | Ready slots kept. Zero keeps none; slots already made are still taken. |
| `labels` | A `by serve` or `by worker` keeps the pool filled only when it carries every label ([worker labels](server.md#worker-labels)). A local `by env pool fill` ignores them. Claims ignore them: a slot is a directory on this host, and any branch made here may take it. |
| `max_age_minutes` | How long a ready slot is kept. Default one day. |
| `max_behind` | How many commits the base a branch asks for may be ahead of a slot. The slot is brought forward when it is taken. Default 20. A commit that changes an environment input always makes a slot stale. |
| `base` | The revision new slots are made at. Default `HEAD` of the checkout. |

`[workspace.pool]` does not change the section's [trust](workspace.md#trust) digest: a pool runs nothing setup does not. It works without `prepare` too; then a slot is just a checked-out worktree, and setup runs in the turn as before.

## Slots

A **slot** is a detached worktree under `.branchyard/pool/<id>`, at the pool's base, with the prepared environment restored into it as a branch's setup would restore it (cloned or copied, `share` directories linked). A pool is identified by its **recipe**: a hash of the setup commands, the copy and input globs, `prepare` and `share`. Change setup and the pool is a new one; the old one's slots are discarded by the next fill.

Each slot is a row in the store: `pool_slots` in `.branchyard/state.db`, or `by_pool_slots` in the server's PostgreSQL database. A row names its place (host name and checkout), its recipe, its state, its commit, its environment and the process making or taking it.

| State | Meaning |
|---|---|
| `filling` | Its worktree is being made by the process its row names. |
| `ready` | Made and unclaimed. |
| `claimed` | Taken by the process its row names: for a branch, or to be removed. Never ready again. |

A row changes state only by compare-and-set (`UPDATE … WHERE state = 'ready'`). Of several engines claiming one slot, in one process or several, on SQLite or PostgreSQL, exactly one gets it.

## Taking a slot

A new top-level branch created on this host (not sandboxed, not a fork, child or rewind) whose workspace has a pool tries the pool before it makes a worktree. This is part of the journaled `create` step:

1. Ready slots of its recipe at this place, those at its base first, then the oldest.
2. A slot is passed over, and taken for removal, when it is stale: its recipe differs, it is older than `max_age_minutes`, its environment is no longer there, or its commit is not an ancestor of the branch's base, or is more than `max_behind` commits behind it, or the commits between change an environment input (a new key).
3. The first fitting slot is claimed by compare-and-set. Its worktree must be at the commit its row says, with no change to tracked files; otherwise it is removed and the next is tried.
4. `git worktree move` renames it to the branch's path, and `git switch -c by/<name> <base>` creates the branch there. A slot behind the base is brought forward by that checkout. The row is deleted.
5. The branch's setup finds its key's environment already in the worktree and restores nothing. When the key differs (it cannot, by step 2, but a copy glob could change an input), what the slot held is cleared first and the usual restore or build runs.

With no fitting slot, or when the move or checkout fails, the branch's worktree is made as before. Either way the setup's `workspace` event says what happened in `pool`:

| Field | Meaning |
|---|---|
| `slot` | The slot taken (a **hit**); absent on a **miss** |
| `reason` | Why no slot was taken (`no ready slot`, `no ready slot fits (1 passed over: …)`, `could not take slot …`), or how the slot was brought forward (`brought forward 2 commits`) |
| `requested_ms` | When the branch was asked for: start latency runs from here to its first prompt |
| `worktree_ms` | How long taking the slot, or making the worktree without one, took |

`by log` adds `worktree from warm pool slot s…` or `no warm pool slot (…)` to the setup line.

A claim never fills. A stale slot it meets is marked for removal, and the next fill removes it; only a slot found dirty is removed at once.

## Filling

A fill (`by env pool fill`, or a keeper in `by serve` and `by worker`) does this, one process per checkout at a time (an advisory lock on `.branchyard/pool/.fill.lock`; a second filler waits for the first, then finds the pool full):

1. Reclaims what stopped processes left (see [Crash safety](#crash-safety)).
2. Resolves the pool's base, and discards every ready slot that is stale for it.
3. Makes slots until `size` are ready or being made. Each is recorded `filling` first, then made with `git worktree add --detach`, then given its environment: the key's prepared environment restored, or the last good one of its recipe, or, when the key has neither, built in the slot (setup runs there with `BRANCHYARD_BRANCH=pool`, as `by env rebuild` would) and then restored. A key whose build failed is never retried by a pool: the fill stops and names `by env rebuild`. Copied files (`copy` globs) are removed from a slot after a build: a branch copies its own when it starts.

**Who refills.** `by serve` and `by worker` run one keeper per served repository whose scripts they may run (`allow_workspace_scripts`). A keeper fills at start, again as soon as a branch in that process takes a slot or finds none, and otherwise every 30 seconds. It reads `branchyard.toml` each time, so a changed pool needs no restart. A keeper fills only when the process carries the pool's labels.

**The CLI does not refill.** `by run`, `by fan` and the rest only take slots. A CLI process ends with its command, so a refill it started would be cut short or outlive it. Locally, run `by env pool fill` when you want the pool full, or run `by serve` (or `by worker`) on the repository.

## Crash safety

Every row names the process making or taking it, as a lease names its holder. Reclaiming (on every `Yard::open`, through recovery, and before every fill and drain) removes:

- a `filling` or `claimed` row whose process is gone from this host (or ran in an earlier boot), with its worktree if it is still in the pool;
- a `claimed` row whose worktree was already moved, when the branch was not created there: the half-made worktree goes too;
- a `ready` row whose worktree is gone;
- a directory in `.branchyard/pool/` with no row (an orphan), and git's record of it.

Directories are listed before rows, and a filler writes its row before its directory, so a slot being made is never taken for an orphan. A claimed slot is never ready again, so no restart hands one slot to two branches. Prepared environments a slot links into (`share`) are never pruned while it does.

## Commands

```sh
by env pool status    # this pool's slots: ready, being made, being claimed
by env pool fill      # discard stale slots, make new ones until full (may run setup: needs trust)
by env pool drain     # remove every ready slot on this host
```

All take `--json`, and act on a local repository (`--remote` is refused: a server's slots are on its host). `fill` needs the section to be [trusted](workspace.md#trust) and is refused from a harness on a branch. `drain` leaves slots another live process is making or taking, and says so.

## Measurement

- **`by stats`** reads the setup events: `pool 3 hits, 1 misses; 1 of 2 ready, made in median 4.2s`, then the start latency (asked for to first prompt) of hits and of misses, median and 90th percentile. `--json` has the same as `pool`.
- **`/metrics`** ([observability](observability.md)): `branchyard_pool_slots` (ready, filling and claimed slots per repository, read at scrape time), `branchyard_pool_claims_total` (hits and misses of the tasks this process ran), `branchyard_pool_slots_made_total` and `branchyard_pool_fill_seconds` (what this process's keepers made, and how long each slot took), `branchyard_pool_slots_discarded_total`, and `branchyard_start_seconds` (a task's new branches, from admission to the first prompt, by `pool` = `hit`, `miss` or `none`).

## SDK

| Call | What it does |
|---|---|
| `WorkspaceSpec::pool` (`PoolSpec`) | `size`, `labels`, `max_age_secs`, `max_behind`, `base`; stored with the branch |
| `Yard::pool_status(&spec)` | `PoolStatus`: recipe, size, base, this pool's slots (`PoolSlot`), and how many of another recipe are left |
| `Yard::pool_slots()` | Every slot on this host |
| `Yard::fill_pool(&spec)` | `PoolFill`: made (with `fill_ms`), discarded, reclaimed, ready, and why it stopped or skipped |
| `Yard::drain_pool()` | `PoolDrained`: removed and kept, each with why |
| `Yard::keep_pool(spec, every, on_fill)` | A `PoolKeeper` thread; `stop()` waits for a fill under way, dropping it does not |
| `WorkspaceReport::pool` | `PoolUse`: `slot`, `reason`, `requested_ms`, `worktree_ms`; in `schema/contract.json` |
| `pool_recipe(&spec)` | The pool's identity |

The SDK takes no trust decision: `fill_pool` runs the setup its caller passes.

## Sandboxes

Sandboxes are **not** pre-booted; this part of the plan is not built. A sandboxed branch never takes a slot: a slot holds this host's environment, while a sandboxed branch's environment is a sandbox branched from a provider snapshot ([environments](environments.md#sandboxes)). With a provider that can live-branch, that sandbox already starts from the prepared snapshot, rebound to the branch's own worktree and home, without running setup. A sandbox kept paused for a slot would have to be handed over the same way (a live branch with the branch's mounts), so it would save at most the provider's own start; that is left until a real provider (all are unqualified here) shows the gain. The pool's configuration has no key for it.

## Limits

- **One host, one checkout.** Slots are directories in the checkout's `.branchyard/`. Rows carry their host and checkout, so servers sharing a PostgreSQL database never take each other's.
- **Git's own locks.** Taking a slot writes git configuration and moves a worktree, under the engine's in-process git lock. Two processes creating branches at the same instant can still meet git's `config.lock`, as making worktrees always could.
- **`git worktree move`** refuses a worktree with submodules: such a slot is removed and the branch made as before.
- **Not tested.** A real package manager, a filesystem that clones, macOS, a server under load, and start-up time itself.

## Code and tests

| Where | What |
|---|---|
| [`pool.rs`](../crates/branchyard/src/pool.rs) | recipes, claims, staleness, fill, drain, reclaim, the keeper |
| [`run.rs`](../crates/branchyard/src/run.rs), [`workspace.rs`](../crates/branchyard/src/workspace.rs) | taking a slot in the `create` step; setup that finds the environment in place |
| [`sqlite.rs`](../crates/branchyard/src/sqlite.rs), [`pg.rs`](../crates/branchyard/src/pg.rs) | `pool_slots` and `by_pool_slots` (made, on PostgreSQL, as one catalog-checked step) |
| [`branchyard-workspace` `repo.rs`](../crates/branchyard-workspace/src/repo.rs) | `Repository::adopt_worktree` |
| [`serve.rs`](../crates/branchyard-server/src/serve.rs), [`metrics.rs`](../crates/branchyard-server/src/metrics.rs), [`observe.rs`](../crates/branchyard-server/src/observe.rs) | keepers, gauges and counters, start latency |
| [`env_cmd.rs`](../crates/branchyard-cli/src/env_cmd.rs), [`stats_cmd.rs`](../crates/branchyard-cli/src/stats_cmd.rs) | `by env pool`, `by stats` |

Tested hermetically:

- The store (`conformance.rs`, on SQLite and PostgreSQL): rows inserted once, listed per place, changed only from the expected state, six engines racing for one ready slot with exactly one winning, never ready again, and left alone when a branch is deleted.
- `branchyard-workspace`: a detached worktree adopted as a branch (moved, with its untracked files and links), brought forward, an existing branch refused before the slot is touched, a blocked checkout leaving no worktree or branch.
- The engine (`crates/branchyard/tests/pools.rs`): a fill to size with setup run once, clean detached slots at the base, a full pool making nothing, a drain; a branch taking a slot and running no setup, the next missing with its reason; shared directories staying links and their environment not pruned; six branches racing for three slots with three distinct hits; a keeper refilling after a claim; staleness when setup changes, when the base moves (brought forward by one commit), when an input changes, beyond `max_behind`, and by age; a dirty slot never handed out; a filler killed with SIGKILL in setup, and an orphan directory, reclaimed on the next open with nothing left in git; a claim whose process stopped reclaimed and not handed out again.
- PostgreSQL (`crates/branchyard/tests/postgres.rs`): a filler killed in setup reclaimed by the next open, a fill seen whole by another engine, and two engines' branches taking different slots.
- The server (`crates/branchyard-server/tests/pools.rs`): without the pool's labels nothing is filled and a task misses; with them the server fills, a task hits, the keeper refills after the claim, and `/metrics` shows the slots, the hit, the fills and the start latency. Unit tests for the metrics families and for start latency from events.
- The CLI (`crates/branchyard-cli/tests/workspace.rs`, the built `by`): `by env pool status`, `fill` refused until trusted and from a harness, `by run` taking the slot with no setup and a linked `share`, the next `by run` missing, `by stats` with hits, misses and start latency, `drain`, and `--remote` refused.
