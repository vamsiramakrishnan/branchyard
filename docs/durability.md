# Durable execution

Branchyard's engine keeps its state in one durable store and runs every branch operation as journaled steps under a fenced lease. A process that dies mid-turn leaves a record that the next engine to open the repository can reconcile: it kills what the dead engine left running, says truthfully what happened to the turn, and never runs the model turn again.

> **Status.** Implemented in local mode and the server, and tested hermetically against a fake ACP agent: a real engine process killed with SIGKILL mid-turn and recovered, two yards on one repository, a superseded lease, replayed steps, cancellation through the SDK, `by`, HTTP and `by --remote`, cursor reads, and the import of earlier state. Not yet run with a real harness, on macOS, across hosts, or under load. The PostgreSQL mapping below is a design, not code.

## The model

The approach follows Temporal and DBOS, with one deliberate difference.

- **Engine code is deterministic; model turns are effects.** Creating a worktree, starting a harness, submitting a prompt, taking a snapshot and merging are steps. Each records its *intent* before the effect and its *outcome* after, keyed by `(branch, turn, step)`. A step whose outcome is recorded is not repeated: its outcome is used.
- **A model turn is never replayed.** Temporal and DBOS re-execute workflow code from its history after a crash; Branchyard does not re-execute a turn, because a prompt is a non-deterministic, costly, externally visible effect. A turn whose prompt was submitted and whose end was not recorded has an *unknown outcome*, and the branch ends `interrupted` with an event that says so. Continuing it is your decision: `send` resumes the harness session.
- **One owner at a time.** A turn runs under the branch's lease, and every write of the turn carries the lease's fencing generation, checked in the same transaction as the write.
- **Signals and timers are rows.** A cancel is a durable request bound to the turn it was asked of; a `max_duration` deadline is stored with the lease.

## The store

`Store` ([`state.rs`](../crates/branchyard/src/state.rs)) is the engine's only access to durable state. It forwards to a `Backend` trait, the one abstraction: branch records, the event log, journaled steps, leases, harness process identities and cancel requests. Local mode uses [`sqlite.rs`](../crates/branchyard/src/sqlite.rs): SQLite through `rusqlite` 0.39 with the bundled library, at `.branchyard/state.db`, in write-ahead-log mode.

Writes run in `BEGIN IMMEDIATE` transactions, so a fence check and the write it guards commit together, and writers in other processes wait (up to 30 seconds) rather than fail. Records, leases, steps, processes and cancels commit with `synchronous=FULL`. Event appends commit with `synchronous=NORMAL`: they survive a process crash, but an operating-system crash or power loss can lose the last appends before it; the next `FULL` commit makes every earlier append durable too.

| Table | Key | Holds |
|---|---|---|
| `branches` | `incarnation` (autoincrement), `name` unique | The record as JSON; `NULL` is a reserved name. A removed and recreated branch is a new incarnation |
| `leases` | `branch` | `generation`, the `turn` it was granted for, `owner` (`NULL` once released), owner `host`, `pid`, `pid_start`, `expires_ms`, and `deadline_ms` |
| `steps` | `(incarnation, turn, step)` | `intent`, `outcome` (`NULL` until done), the generation that wrote it, times |
| `processes` | `(incarnation, turn, pid)` | Process group, start time and host of each harness process a turn started |
| `cancels` | `(incarnation, turn)` | Who asked, when, and whether for a subtree; the first request for a turn is kept |
| `events` | `id` (autoincrement), unique `(incarnation, seq)` | Branch name, `seq` from 1 per incarnation, `at_ms`, the activity as JSON |
| `meta` | `key` | Schema version (1) and when earlier state was imported |

`id` is the feed position. SQLite serializes writers, so positions are assigned in commit order: a reader that sees position *N* already sees every position before it.

### Journaled steps

| Step | Turn | Intent | Outcome | On recovery |
|---|---|---|---|---|
| `create` | the first turn | base, worktree path | worktree, or the error | Not repeated; a branch without a worktree ends `failed` at the snapshot |
| `sandbox` | each, Substrate only | the actor's name and atespace, before it is created | its UID | The actor is deleted if it still exists |
| `start` | each | command, sandboxed or not | pid, process group and start time, or the error | The recorded group is killed if it still matches |
| `submit` | each | the prompt | the harness's turn number | Recorded intent means the prompt may have reached the harness: never submitted again |
| `turn_end` | each | whether a prompt was submitted | how the turn ended | Finished as the engine would have |
| `snapshot` | each | the commit message | the candidate, or the error | A recorded snapshot is used, not taken again |
| `merge <target> <candidate>` | 0 | target, candidate, expected target revision | the merge | A pending merge whose candidate is already in the target is recorded as done instead of repeated |
| `remove` | 0 | none | none (the branch is deleted) | A removal cut short can be repeated |

The check a merge runs is part of the `merge` step. The final record, the final status event and the lease release commit in one transaction; that transaction is the turn's conclusion.

A merge the caller repeats after it succeeded is still refused with `already_merged`: the journal replays a merge only when the record does not yet say it was merged.

## Leases and fencing

`create`, `send`, `fork`, `merge` and `remove` take the branch's lease in the same transaction that marks the branch running (or, for a merge or removal, keeps its record), with a new generation. The engine opened by each `Yard::open` is one owner, identified by its process, the process's start time, and the host and boot it runs on. While the lease is held, from admission until the turn, merge or removal concludes, a heartbeat thread renews it every 5 seconds for 30.

- While a live owner holds the lease, another `Yard`, in this process or another, gets `Error::Running` for `send`, `merge` and `remove`.
- Every record write, event append, step and process row of a turn checks that the lease still has the turn's incarnation and generation. Once another engine takes the lease over, the old owner's writes fail with `Error::Fenced` and it writes nothing more; its heartbeat notices within 5 seconds, and the turn stops its harness.
- A lease is *stale* when its owner is on this host and boot and its process (pid and start time) is gone, or when it expired. Stale leases are what recovery takes over.

A lease cannot stop a process on another host that is still running but partitioned (design §11): fencing makes its writes to the store fail, and nothing more.

## Cancellation and deadlines

`Yard::cancel(branch)`, `Branch::cancel()`, `by cancel`, `POST /v1/repos/{repo}/branches/{branch}/cancel` and `by --remote … cancel` all record a cancel request for the branch's running turn and for every running turn delegated below it (the delegation subtree, as `cancel_tree` computes it). The request names who asked (`Yard::cancel_as`). The engine running the turn, in any process using the repository, reads it every 100 ms, interrupts the harness like a budget stop, and ends the branch `interrupted` with a `cancelled by …` warning. A request is bound to the turn it was asked of, so it never stops a later turn. It returns the branches that were running; a branch with no running turn is not an error.

A turn with `max_duration` stores its deadline with the lease. The engine that owns the turn enforces it; recovery reports whether it had passed. Since a recovered turn is never continued, no engine counts it further.

## Recovery

`Yard::open` runs `Yard::recover`; the server runs it at start and every 30 seconds; `send`, `merge` and `remove` run it for their branch first. For each stale lease, recovery takes the lease over with a new generation (only one engine wins), then:

1. **Processes.** A Substrate turn's actor, named by its `sandbox` step, is deleted through the `Control` API, and the `recovered` reason says so; its harness already ended when the dead engine's bridge connection closed. For each harness process the turn recorded on this host and boot: if a live process has the recorded pid and start time, its process group is killed; if the pid now has another start time, the pid was reused and nothing is signalled; if the leader is gone, on Linux the remaining members of its group that started no earlier than it are killed one by one. Start time is `/proc/<pid>/stat`'s `starttime` on Linux and `ps -o lstart=` elsewhere.
2. **Status, from the journal.**
   - `turn_end` recorded: the turn is finished as the engine would have, with the recorded snapshot if there is one, else a new snapshot. The status is the turn's own.
   - `submit` recorded, no `turn_end`: the turn counts as run, the worktree is snapshotted, and the branch ends `interrupted`: *the prompt had been submitted and the turn's outcome is unknown. It was not submitted again.*
   - No `submit`: the branch ends `interrupted`: *… before the prompt was submitted* (or *before the harness was started*) *; the turn never ran.*
3. **Record.** An `Activity::Recovered { reason, killed }` event, then the final record and status event and the lease release, in one transaction. The branch keeps the harness session it last reported, so `send` can continue it.

A branch whose record says `running` and whose lease is free, which only an earlier version or an engine that failed without settling leaves behind, ends `interrupted` with a `Recovered` event that says its last turn's outcome is unknown.

Nothing is ever submitted again by recovery.

## Reading events from a cursor

Each branch's events are numbered from 1; the repository's feed numbers every event once, in commit order.

| API | Returns |
|---|---|
| `Branch::events_since(cursor, limit)` | `Page { events, next_cursor }`: events numbered after `cursor` |
| `Branch::wait_for_events(cursor, limit, timeout)` | The same, waiting up to `timeout` for the first |
| `Yard::events_since(cursor, limit)` | `FeedPage { events: [FeedEvent { position, branch, event }], next_cursor }` |
| `Yard::wait_for_events(cursor, limit, timeout)`, `Yard::events_head()` | The same, waiting; the last position |

A wait wakes at once for events recorded in the same process and within 100 ms for another process's. `Branch::events` still returns every event. The server's SSE stream, its `GET …/events?cursor=N`, `by watch`, and the delegation `events` tool all read through these; nothing tails files any more.

The SSE cursor is the feed position. Its documented semantics hold: `open` then one `activity` per event after the cursor, resume by `?cursor=` or `Last-Event-ID` with no gap or repeat, `cursor_out_of_range` past the head, across restarts. Positions only grow; do not assume they are contiguous. The per-branch `?cursor=N` of `GET …/events` means the same as before, events after the first *N*. **Changed:** stream positions now come from the repository's store rather than a server-side copy, so a cursor saved from a server of an earlier version names a different event; reconnect from 0 or from now.

Events of a removed branch stay in the feed; a new branch with the same name numbers its own events from 1.

## State from earlier versions

On first open, `.branchyard/branches/*.json` (an empty file was a reservation) and `.branchyard/events/*.jsonl` are imported in one transaction: records as they were, events in line order per branch (a torn final line is dropped) and in time order across branches in the feed. The directories then move to `.branchyard/legacy/`, and the import is recorded so it never runs again. A record that says `running` is then recovered as described above. The delegation cancel marker files are no longer read. Running an earlier `by` on a repository after the import is not supported: it would write files nothing reads.

The server's operation registry moves from `DATA-DIR/operations.jsonl` to `DATA-DIR/state.db` (SQLite, the same settings, `synchronous=FULL` on every save): the file is imported on first start and renamed `operations.jsonl.imported`. `DATA-DIR/feeds/` is no longer used and can be deleted. The registry is a separate database from the repositories' because it spans every served repository and lives in the server's data directory. Its restart behavior is unchanged: operations and idempotency keys survive, and unfinished operations become `interrupted`.

## Not guaranteed

- **Continuing a turn.** A recovered turn is never resumed or resubmitted, and its partial work is only what the snapshot captured.
- **Recovery across hosts.** An engine on another host is recovered only once its lease expires (30 seconds without a heartbeat), and its processes there are not killed.
- **Processes that leave their group** (a daemon calling `setsid`), and a harness started in the instant between its spawn and the `start` step's outcome, which recovery does not know about.
- **Sandboxed harnesses.** For a Microsandbox branch the provider's process ID is recorded but not reconciled; the sandbox is not destroyed by recovery. A Substrate turn's actor is deleted, but what its harness changed there is not brought back, and the host's transfer staging directory is left in the temp directory.
- **The last event appends before an operating-system crash** (see the store).
- **A hung owner.** An engine that stops renewing for 30 seconds while still alive, such as a stopped process, is taken over; its later writes are fenced, but its harness may already have been killed by recovery.
- **Stale reservations.** A name reserved by an engine that died before creating the branch stays taken.
- **Old and new versions side by side** on one repository.
- **One server per data directory** is still not enforced.

## PostgreSQL

The server's store is meant to be PostgreSQL (design §7–§8, §11). The `Backend` trait and the server's `OperationStore` are the seams; nothing above them changes. The mapping:

| Local SQLite | PostgreSQL |
|---|---|
| `BEGIN IMMEDIATE` with the fence check inside | `BEGIN`; `SELECT … FROM leases WHERE branch = $1 FOR UPDATE`; compare incarnation and generation; write; `COMMIT` |
| `branches.incarnation` autoincrement | `bigint GENERATED ALWAYS AS IDENTITY`; `name` unique among live rows, scoped by repository and tenant |
| `acquire` (insert or bump the lease and write the record) | `INSERT … ON CONFLICT (branch) DO UPDATE … WHERE leases.owner IS NULL RETURNING generation`, in the same transaction as the record |
| `take_over` | `UPDATE leases SET generation = generation + 1, owner = $2 … WHERE branch = $1 AND generation = $observed AND owner IS NOT NULL`; one row updated wins |
| Lease expiry, heartbeat | `expires_at timestamptz` compared with the database's `now()`, not the engine's clock |
| `steps`, `processes`, `cancels` | Same tables; `jsonb` for intents and outcomes; `INSERT … ON CONFLICT DO NOTHING` for the first cancel |
| `events.id` feed positions in commit order | A sequence is *not* commit-ordered across concurrent transactions. Assign positions from a per-repository counter row locked in the appending transaction (`UPDATE feed_heads SET head = head + 1 … RETURNING head`), or read the feed only up to the oldest in-flight transaction's position |
| In-process wakeups, 100 ms polling across processes | `NOTIFY` after commit as a hint, with bounded polling as the recovery path (design §8) |
| `synchronous=FULL` / `NORMAL` | `synchronous_commit = on` for records, leases, steps; `off` is acceptable only for events, with the same caveat |
| Startup and periodic recovery | A reconciler that selects stale leases (`expires_at < now()`) with `FOR UPDATE SKIP LOCKED` |
| Operation registry | An `operations` table in the same database, written with the accepted command in one transaction and delivered through PGMQ, as design §8 describes |

Process identity then comes from the node that runs the harness, which reports its observed allocations on reconnect (design §11); the server rejects stale generations from it.

## Code and tests

| Where | What |
|---|---|
| [`state.rs`](../crates/branchyard/src/state.rs), [`sqlite.rs`](../crates/branchyard/src/sqlite.rs) | The store, the `Backend` trait, leases and heartbeat, the SQLite backend and the import |
| [`engine.rs`](../crates/branchyard/src/engine.rs), [`run.rs`](../crates/branchyard/src/run.rs), [`ops.rs`](../crates/branchyard/src/ops.rs) | Journaled steps of a turn, of branch creation, merge and removal |
| [`recover.rs`](../crates/branchyard/src/recover.rs), [`proc.rs`](../crates/branchyard/src/proc.rs) | Recovery and process identity |
| [`tests/durable.rs`](../crates/branchyard/tests/durable.rs) | A killed engine recovered (orphaned harness and its child killed, prompt received once, session continued), a crash before submit, two yards, a superseded lease, a replayed snapshot, a merge cut short, cursors and waits, the import |
| `sqlite.rs` unit tests | Fencing after takeover, expiry, step replay, cancels bound to a turn |
| [`cli.rs`](../crates/branchyard-cli/tests/cli.rs), [`remote.rs`](../crates/branchyard-cli/tests/remote.rs), [`api.rs`](../crates/branchyard-server/tests/api.rs) | `by cancel` from another process, `by --remote cancel`, HTTP cancel, the SSE stream from the store across a restart |
