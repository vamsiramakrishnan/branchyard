# Durable execution

Branchyard's engine keeps its state in one durable store and runs every branch operation as journaled steps under a fenced lease. A process that dies mid-turn leaves a record that the next engine to open the repository can reconcile: it kills what the dead engine left running, says truthfully what happened to the turn, and never runs the model turn again.

> **Status.** Implemented in local mode and the server, and tested hermetically against a fake ACP agent: a real engine process killed with SIGKILL mid-turn and recovered, two yards on one repository, a superseded lease, replayed steps, cancellation through the SDK, `by`, HTTP and `by --remote`, steered input from another process, cursor reads, and the import of earlier state. The PostgreSQL store passes the same conformance suite as SQLite, and the engine and server tests that run on it, against PostgreSQL 16. Not yet run with a real harness, on macOS, across hosts, or under load.

## The model

The approach follows Temporal and DBOS, with one deliberate difference.

- **Engine code is deterministic; model turns are effects.** Creating a worktree, starting a harness, submitting a prompt, taking a snapshot and merging are steps. Each records its *intent* before the effect and its *outcome* after, keyed by `(branch, turn, step)`. A step whose outcome is recorded is not repeated: its outcome is used.
- **A model turn is never replayed.** Temporal and DBOS re-execute workflow code from its history after a crash; Branchyard does not re-execute a turn, because a prompt is a non-deterministic, costly, externally visible effect. A turn whose prompt was submitted and whose end was not recorded has an *unknown outcome*, and the branch ends `interrupted` with an event that says so. Continuing it is your decision: `send` resumes the harness session.
- **One owner at a time.** A turn runs under the branch's lease, and every write of the turn carries the lease's fencing generation, checked in the same transaction as the write.
- **Signals and timers are rows.** A cancel is a durable request bound to the turn it was asked of, and so is steered input; a `max_duration` deadline is stored with the lease.

## The store

`Store` ([`state.rs`](../crates/branchyard/src/state.rs)) is the engine's only access to durable state. It forwards to a `Backend` trait, the one abstraction: branch records, the event log, journaled steps, leases, harness process identities, cancel requests and steered input. Local mode uses [`sqlite.rs`](../crates/branchyard/src/sqlite.rs): SQLite through `rusqlite` 0.39 with the bundled library, at `.branchyard/state.db`, in write-ahead-log mode. A server started with `--database` uses [`pg.rs`](../crates/branchyard/src/pg.rs) instead (see [PostgreSQL](#postgresql)).

Writes run in `BEGIN IMMEDIATE` transactions, so a fence check and the write it guards commit together, and writers in other processes wait (up to 30 seconds) rather than fail. Records, leases, steps, processes, cancels and steered input commit with `synchronous=FULL`. Event appends commit with `synchronous=NORMAL`: they survive a process crash, but an operating-system crash or power loss can lose the last appends before it; the next `FULL` commit makes every earlier append durable too.

| Table | Key | Holds |
|---|---|---|
| `branches` | `incarnation` (autoincrement), `name` unique | The record as JSON; `NULL` is a reserved name. A removed and recreated branch is a new incarnation |
| `reservations` | `name` | For a reserved name not yet created: the reserving engine's `owner`, `host`, `pid`, `pid_start`, and `reserved_ms` |
| `leases` | `branch` | `generation`, the `turn` it was granted for, `owner` (`NULL` once released), owner `host`, `pid`, `pid_start`, `expires_ms`, and `deadline_ms` |
| `steps` | `(incarnation, turn, step)` | `intent`, `outcome` (`NULL` until done), the generation that wrote it, times |
| `processes` | `(incarnation, turn, pid)` | Process group, start time and host of each harness process a turn started |
| `cancels` | `(incarnation, turn)` | Who asked, when, and whether for a subtree; the first request for a turn is kept |
| `steers` | `id` (autoincrement), indexed by `(incarnation, turn, state)` | Input for a running turn: the turn it is bound to, who sent it, its text, when, and its `state` (`pending`, `delivered`, `accepted`, `refused`) with the `reason` for a refusal |
| `events` | `id` (autoincrement), unique `(incarnation, seq)` | Branch name, `seq` from 1 per incarnation, `at_ms`, the activity as JSON |
| `meta` | `key` | Schema version (1) and when earlier state was imported |
| `graph_edges` | `(dependent, prerequisite)`, indexed by `prerequisite` and `parent` | A dependency between two children of `parent`, and `after` (`settled` or `integrated`); see [task graphs](graph.md) |
| `graph_revisions` | `parent` | A parent's graph revision, bumped by each committed proposal and spawn |
| `messages` | `id` (autoincrement) | Harness-to-harness messages: `from`, `to`, `kind`, `text`, `in_reply_to`, `at_ms`, `delivered_ms` (`NULL` until acknowledged), `steer_id` (the steered input carrying it into a running turn), `delivered_steer` (the steered input that delivered it) and `awaiting_until_ms` (an `ask --wait` waiter's deadline); see [delegation](delegation.md#delivery) |

`id` is the feed position. SQLite serializes writers, so positions are assigned in commit order: a reader that sees position *N* already sees every position before it.

### Journaled steps

| Step | Turn | Intent | Outcome | On recovery |
|---|---|---|---|---|
| `create` | the first turn | base, worktree path | worktree, or the error | Not repeated; a branch without a worktree ends `failed` at the snapshot |
| `sandbox` | each, Substrate or Microsandbox | the actor's name and atespace, or the Microsandbox sandbox's name, before it is created | the actor's UID, or whether the sandbox was created | A live actor's work is brought back, then the actor is deleted; the sandbox is destroyed |
| `start` | each | command, sandboxed or not, and for a local harness the host and the spawn marker it is started with | pid, process group and start time, or the error | The recorded group is killed if it still matches; on Linux, processes carrying the marker are killed |
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

### Steered input

`Branch::steer(text)`, `Yard::steer_as(branch, text, by)`, `by send --steer`, `POST …/branches/{branch}/steer`, `by --remote … send --steer` and the `steer` delegation tool all queue a `steers` row for the branch's running turn: in the same transaction, the branch's lease must be held, and the row takes that lease's `(incarnation, turn)`, exactly as a cancel does, so input is never delivered to a later turn. With no lease held the request fails with `not_running`; a lease whose engine is gone is recovered first. A profile that cannot take input mid-turn is refused before anything is written ([harness support](harness-integration.md#steering-a-running-turn)).

The engine running the turn, in any process using the repository, reads the turn's `pending` rows at most every 100 ms. While the prompt has not been submitted it leaves them; while the turn runs it writes each to the harness with `Driver::steer`, records `Activity::Steered` (`id`, `by`, `text`) and sets the row `delivered`; the harness's `steer_accepted` sets it `accepted`, and its `steer_rejected` (refused, cancelled by an interrupt, or dropped because the turn ended first) sets it `refused` with the harness's reason. While the turn is being stopped, input is refused, never delivered. When the turn ends, rows still `pending` are refused and a warning names them. Every state change is fenced by the turn's lease. `Yard::steer_state` reports a row still `pending` whose turn no longer holds the lease (its engine stopped, or the request raced the turn's end) as refused: it was never delivered. Recovery neither delivers nor replays steered input: like a submitted prompt, whether the harness saw input it was written is unknown once its engine stops, and the input is not written again.

Steered input that carries an inbox message ([delegation: delivery](delegation.md#delivery)) is linked to the message's row in the transaction that queues it, refused (nothing queued) if the message is already delivered. Settling it `delivered` or `accepted` marks the message delivered in the same fenced transaction and records `Activity::MessagesDelivered` once; settling it `refused` returns a message it had delivered to pending. A turn's start skips a message whose steer is still pending for that same turn, and the engine refuses, unwritten, a steer whose message the turn's start delivered first, so a message reaches a turn once.

A turn with `max_duration` stores its deadline with the lease. The engine that owns the turn enforces it; recovery reports whether it had passed. Since a recovered turn is never continued, no engine counts it further.

A turn with `Budget::stall_after` marks itself stalled the same way `max_duration` stores its deadline: on the transition, the engine writes the branch's record under the turn's fence with `stalled: true` (and back to `false` on new activity), so a concurrent reader — `by ls`, `by inspect`, another process's `Yard::branch` — sees it live, not only after the turn ends. This is an ordinary fenced write of the same record `create` already wrote at lease acquisition, not a new journaled step: recovery never needs to reconstruct it, because whichever status the turn ends with, `conclude` writes the final record with `stalled: false`. See `docs/lifecycle.md`.

## Recovery

`Yard::open` runs `Yard::recover`; the server runs it at start and every 30 seconds; `send`, `merge` and `remove` run it for their branch first. For each stale lease, recovery takes the lease over with a new generation (only one engine wins), then:

1. **Processes.** For each harness process the turn recorded on this host and boot: if a live process has the recorded pid and start time, its process group is killed; if the pid now has another start time, the pid was reused and nothing is signalled; if the leader is gone, on Linux the remaining members of its group that started no earlier than it are killed one by one. Start time is `/proc/<pid>/stat`'s `starttime` on Linux and `ps -o lstart=` elsewhere. Then, on Linux, every process whose environment has `BRANCHYARD_SPAWN` set to the marker the `start` step journaled is killed: a local harness is started with it, and what it starts inherits it, so this finds a harness spawned in the instant before its engine stopped, whose pid was never recorded, and processes that left the harness's group. The marker names the engine instance, the branch's incarnation and the lease generation, so it matches no other turn.
   - **Sandboxes.** A Microsandbox sandbox named by the turn's `sandbox` step is destroyed through the provider (a build without the `microsandbox` feature cannot, and the `recovered` reason says so). A Substrate actor that still exists has its work brought back as the turn's end would have: recovery mints a new attempt credential with the host's bridge key, which supersedes the dead engine's, fetches the actor's working files as a bundle into the turn's staging repository, and applies them to the worktree only if the worktree still holds exactly what was sent to the actor; it brings the home back too. Then it deletes the actor and the staging directory. The `recovered` reason says whether the work came back, and if not, why (the worktree changed since, the key could not be read, the actor did not answer). The harness itself already ended when the dead engine's bridge connection closed.
2. **Status, from the journal.**
   - `turn_end` recorded: the turn is finished as the engine would have, with the recorded snapshot if there is one, else a new snapshot. The status is the turn's own.
   - `submit` recorded, no `turn_end`: the turn counts as run, the worktree is snapshotted, and the branch ends `interrupted`: *the prompt had been submitted and the turn's outcome is unknown. It was not submitted again.*
   - No `submit`: the branch ends `interrupted`: *… before the prompt was submitted* (or *before the harness was started*) *; the turn never ran.*
3. **Record.** An `Activity::Recovered { reason, killed }` event, then the final record and status event and the lease release, in one transaction. The branch keeps the harness session it last reported, so `send` can continue it.

A branch whose record says `running` and whose lease is free, which only an earlier version or an engine that failed without settling leaves behind, ends `interrupted` with a `Recovered` event that says its last turn's outcome is unknown.

**Dependents.** A child created `waiting` for its siblings ([task graphs](graph.md)) holds no lease and needs no recovery itself. When recovery settles a prerequisite, it looks at what depends on it: a dependent of a prerequisite that ended `interrupted` or `failed` is marked `blocked` in a compare-and-swap. Recovery starts no turn of its own, since it has no policy to run one under; a dependent whose prerequisites all settled while no engine was there to start it (its prerequisite's engine stopped after writing the result and before claiming the dependent) is started by the next wait for its parent's subtree in the process that applied its graph, by `Yard::resume_graph` (`by graph resume`), or by the recovery interval of any server or `by worker` serving the repository, all of which run it, several at once on a shared PostgreSQL database. Starting it is a claim: `waiting` to `running` with its first lease, in one transaction, so two engines never both start it; a test races six yards' `resume_graph` on one database and two servers and a worker on a 100 ms tick, and the dependent runs one turn.

Nothing is ever submitted again by recovery.

### Reservations

Reserving a name records the reserving engine as a lease does (owner, host and boot, pid and start time). Recovery frees a reserved name whose branch was never created when that engine is gone from this host, or when the reservation is older than 10 minutes, whichever host made it; reserving and creating happen in one call, well within that. Only the reserving engine can then create the branch: a slow engine whose reservation was freed and taken by another gets `Error::BranchExists`. A reservation from an earlier version names no engine and is never freed.

## Waiting for turns in other processes

`Branch::wait_subtree`, `Delegate::wait` (the `inspect`-until-done of `by spawn --wait` and `wait` in the SDK) and `by run`'s wait for delegated branches read each branch's status from the store, so they wait for a turn whichever process runs it. A wait for a subtree also covers what depends on a branch in it, since a turn ending there starts it, and `Delegate::wait` keeps waiting while a branch is `waiting` for its prerequisites. Children on the waiting process's threads are joined. A branch still `running` is checked once a second: if its engine stopped, the wait recovers it as `Yard::recover` would, so it ends `interrupted` instead of being waited for; a live engine in another process is waited for until its turn ends. An appended event in the same process wakes the wait at once, another process's within 100 ms.

## Operation dispatch

The server's operations follow the same model one level up. Accepting an operation is a durable enqueue: its record, its idempotency binding, its branch locks and a queue row describing its work (a serializable description, not a closure) commit in one transaction before the `202`, and a failure of any of those writes, the queue's included, rolls all of them back. A worker claims a queue row under a lease it renews and a fence (the claim's attempt number) that its `running` record and its outcome both name; the outcome, the queue row's deletion and the branch locks' release commit together, and a worker that lost its claim is refused them. A claim whose lease expired, or whose process is gone from this host, is claimed again: an operation still `queued` runs, and one recorded `running` is recorded `interrupted` and never run again, since only the engine knows whether its prompt was submitted, and the engine's own recovery settles its branch. So an operation admitted by a server that crashed before running it runs exactly once, on a server that restarts or on another sharing the database; and one whose server crashed mid-turn is interrupted, as a turn whose engine died is.

Tenant quotas live in the same transaction. An operation's record carries its tenant and the principal that admitted it, and admission counts the tenant's queue rows (its queued and running operations) before it writes, refusing with nothing written when the tenant is at `max_running`; `max_branches` counts, in the same transaction, the branches the tenant's unfinished operations will create together with those that exist. Admissions of one tenant take turns: SQLite's `BEGIN IMMEDIATE` serializes every admission, and on PostgreSQL a transaction-scoped advisory lock keyed on the tenant serializes that tenant's. There is no quota state apart from the queue, so there is none to recover: a queued operation keeps its tenant's slot across a restart, and the outcome's transaction, which deletes the queue row, is the release. A worker runs an operation as its recorded principal, not with credentials of its own ([server](server.md#quotas)).

On SQLite the data directory lock makes the server the store's only user, so at start it releases every claim its predecessor held. On PostgreSQL several servers and `by worker` processes share the queue: claims use `FOR UPDATE SKIP LOCKED`, leases are measured by the database's clock, and a claim is taken over when its lease expires, or at once when its process is gone from the claiming worker's host ([server](server.md#dispatch)).

## One server per data directory

With `--database`, the server takes no lock: the database holds its operations, and several servers may share it. Otherwise it takes an exclusive advisory lock (`flock`, through `std::fs::File::try_lock`) on `DATA-DIR/lock` before it opens anything and holds it until it has stopped. A second server on the same directory fails at start, after retrying for up to 2 seconds, with an error naming the holder's pid. The retry covers a lock released an instant earlier but still held by a copy of the file that a child starting on another thread took with it, which a server restarted in the same process could otherwise hit. The operating system releases the lock when the process exits, however it exits. `DirLock` in the engine crate is the helper.

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

### Webhook cursors

The server's webhook deliveries (`docs/server.md#webhooks`) read the same feed from a cursor, one per `<repo>:<webhook id>`, durable in the operation store (`OperationStore::load_webhook_cursor`/`save_webhook_cursor`; SQLite's `webhook_cursors` table or PostgreSQL's `by_webhook_cursors`, alongside operations). A delivery's cursor only advances after that entry has either been delivered or dead-lettered, so a restart resumes from exactly where it left off: no entry is silently skipped, and none already delivered is replayed. This is at-least-once, the same guarantee the SSE stream and `events_since` give a client resuming by cursor, extended to a target the server calls out to instead of one that calls in.

## State from earlier versions

On first open, `.branchyard/branches/*.json` (an empty file was a reservation) and `.branchyard/events/*.jsonl` are imported in one transaction: records as they were, events in line order per branch (a torn final line is dropped) and in time order across branches in the feed. The directories then move to `.branchyard/legacy/`, and the import is recorded so it never runs again. A record that says `running` is then recovered as described above. The delegation cancel marker files are no longer read. Running an earlier `by` on a repository after the import is not supported: it would write files nothing reads.

The server's operation registry moves from `DATA-DIR/operations.jsonl` to `DATA-DIR/state.db` (SQLite, the same settings, `synchronous=FULL` on every save): the file is imported on first start and renamed `operations.jsonl.imported`. `DATA-DIR/feeds/` is no longer used and can be deleted. The registry is a separate database from the repositories' because it spans every served repository and lives in the server's data directory. Operations and idempotency keys survive a restart. An unfinished operation recorded by a version without the dispatch queue has no queue row; it becomes `interrupted` at the next start, as it did then.

## Not guaranteed

- **Continuing a turn.** A recovered turn is never resumed or resubmitted, and its partial work is only what the snapshot captured.
- **Recovery across hosts.** An engine on another host is recovered only once its lease expires (30 seconds without a heartbeat), and its processes there are not killed.
- **Processes that leave their group and clear `BRANCHYARD_SPAWN`** from their environment, such as a daemon started with a scrubbed environment. On macOS and other systems without `/proc`, the marker is not searched for, so a harness started in the instant between its spawn and the `start` step's outcome, and processes that left its group, are not found there either.
- **Sandboxed harnesses.** Destroying an orphaned Microsandbox sandbox was tested only against a stand-in provider and, in a build without the feature, the report that it could not be destroyed; not against Microsandbox itself. Bringing a Substrate actor's work back was tested against the fake cluster only. What comes back is the files as they were when the bridge ended the harness; work the harness had not yet written is lost, and a result is not applied over a worktree that changed since the turn began.
- **The last event appends before an operating-system crash** (see the store).
- **A hung owner.** An engine that stops renewing for 30 seconds while still alive, such as a stopped process, is taken over; its later writes are fenced, but its harness may already have been killed by recovery.
- **Reservations of earlier versions** stay taken; another host's reservation is freed only after 10 minutes.
- **Old and new versions side by side** on one repository.
- **The data directory lock on a network file system** is only as good as that file system's `flock`.
- **A hung worker.** A server that stops renewing its operation claims for the lease (30 seconds) while still alive has its running operations recorded `interrupted` by another worker; its own later outcome is refused, and the branch is what the engine says. Operation leases are on the database's clock; the engine's turn leases are on the engines'.
- **Delivery to a server that serves other repositories.** A worker claims only the repositories it serves; an operation for a repository no running worker serves waits in the queue.

## PostgreSQL

[`pg.rs`](../crates/branchyard/src/pg.rs) implements the same `Backend` on PostgreSQL, behind the cargo feature `postgres` (the `postgres` 0.19 crate, the synchronous client over `tokio-postgres`). `Yard::open_postgres(path, url, scope)` opens a repository with it, and `by serve --database URL` opens every served repository that way, with its served name as the scope; the server's `OperationStore` is the `by_operations`, `by_operation_queue` and `by_branch_locks` tables in the same database ([`store.rs`](../crates/branchyard-server/src/store.rs)), which several servers may share. Nothing above the two traits changed.

| SQLite | PostgreSQL |
|---|---|
| `BEGIN IMMEDIATE`: writers serialize; the fence check and the write commit together | `SERIALIZABLE` transactions, retried from the start on a serialization failure or deadlock for up to 30 seconds; the fence check reads the lease in the writing transaction |
| One database per repository | One database, or schema, for many: every table carries the repository's scope. Tables are created when missing in the connection's `search_path` schema, under an advisory lock, and the schema version is checked |
| `branches.incarnation` autoincrement, `name` unique | `bigint GENERATED ALWAYS AS IDENTITY`, `(repo, name)` unique |
| `acquire`, `take_over`, `renew`, `finish` | The same statements; `take_over` updates only the observed generation, so one engine wins |
| `events.id` autoincrement is the feed position | A per-repository counter row (`by_feed_heads`) incremented in the appending transaction. Appends to one repository serialize on it, so positions are assigned in commit order: a reader that sees position *N* sees every position before it. A plain sequence would not guarantee that |
| `synchronous=FULL`; `NORMAL` for event appends | `synchronous_commit = on`; `off` for event appends, with the same caveat |
| JSON as text | JSON as text, so it reads back exactly as written |
| In-process wakeups; 100 ms polling across processes | The same: waits poll every 100 ms. Nothing listens for `NOTIFY` |
| Lease expiry from the engines' clocks | The same; engines on several hosts need synchronized clocks |
| Startup and periodic recovery | The same recovery code, over the database's leases |
| `graph_edges`, `graph_revisions`; a proposal in one `BEGIN IMMEDIATE` transaction | `by_graph_edges`, `by_graph_revisions`, keyed by scope; a proposal in one serializable transaction, retried whole on a conflict |

The client blocks; a call made on a Tokio runtime's thread, such as the server's, runs on a thread of its own. The connection is reopened when it closes. Connections have no TLS.

The dispatch queue is plain tables, not PGMQ: `by_operation_queue` has the operation's ID, its repository, its work description, and the claim (attempt, worker, host, pid, start time, `lease_until`). PGMQ could replace it; the admission transaction would send the message where it now inserts the row, and a worker would read with a visibility timeout where it now sets `lease_until`.

Not built: importing an existing `state.db`, and waking idle workers with `NOTIFY` rather than polling.

### Running its tests

The PostgreSQL tests run when `BY_TEST_POSTGRES_URL` names a database in which they may create tables and schemas; without it they print that they were skipped. PostgreSQL's `initdb` refuses to run as root, so on a machine where you are root, run the cluster as an unprivileged user:

```sh
D=/tmp/branchyard-pg; mkdir -p $D && chown nobody:nogroup $D
runuser -u nobody -- /usr/lib/postgresql/16/bin/initdb -D $D/data -U postgres --auth=trust
runuser -u nobody -- /usr/lib/postgresql/16/bin/pg_ctl -D $D/data -l $D/log -w start \
  -o "-p 54329 -k $D -c listen_addresses=127.0.0.1"
export BY_TEST_POSTGRES_URL=postgres://postgres@127.0.0.1:54329/postgres
F=branchyard/postgres,branchyard-server/postgres,branchyard-cli/postgres
cargo test -p branchyard -p branchyard-server -p branchyard-cli --locked --offline --features $F
runuser -u nobody -- /usr/lib/postgresql/16/bin/pg_ctl -D $D/data stop   # afterwards
```

As any other user, drop `runuser -u nobody --`. The CI job `postgres` in [`check.yml`](../.github/workflows/check.yml) runs the same tests against a `postgres:16` service container.

## Code and tests

| Where | What |
|---|---|
| [`state.rs`](../crates/branchyard/src/state.rs), [`sqlite.rs`](../crates/branchyard/src/sqlite.rs), [`pg.rs`](../crates/branchyard/src/pg.rs) | The store, the `Backend` trait, leases and heartbeat, the SQLite backend and the import, the PostgreSQL backend. The same two backends also implement `StorageBackend`, a separate trait for artifact and scratch-area metadata on the same connection; see [storage](storage.md), which does not otherwise touch branch lifecycle. The `messages` table (the inbox) is on the `Backend` trait: a message's `delivered_ms`, the steered input carrying it (`steer_id`, set in the transaction that queues the steer) and the one that delivered it (`delivered_steer`, set in the transaction that settles the steer, and undone with it if the harness refuses it), and the deadline of an `ask --wait` waiter (`awaiting_until_ms`), which stall detection reads. |
| [`conformance.rs`](../crates/branchyard/src/conformance.rs) | One suite, run against both backends: fencing, expiry, steps and processes, cancels, steered input (bound to its turn, settled only under the turn's fence, kept through a takeover, deleted with the branch), records and children, events and the feed, concurrent appends, races for a name, a lease and a takeover, storage (artifact digests and references, grants along the tree, scratch locks), messages (send, inbox, mark delivered, the answer to a question, the answer waiter), and graphs (a proposal committed whole or not at all against its revision, one claim of a waiting branch, a blocked branch reopened, removal) |
| [`tests/postgres.rs`](../crates/branchyard/tests/postgres.rs), the server's [`postgres.rs`](../crates/branchyard-server/tests/postgres.rs) | A task through merge, waits across yards, two yards and a cancel, a killed engine recovered, and a server's branches and operations across a restart, all on PostgreSQL |
| [`engine.rs`](../crates/branchyard/src/engine.rs), [`run.rs`](../crates/branchyard/src/run.rs), [`ops.rs`](../crates/branchyard/src/ops.rs) | Journaled steps of a turn, of branch creation, merge and removal |
| [`recover.rs`](../crates/branchyard/src/recover.rs), [`proc.rs`](../crates/branchyard/src/proc.rs) | Recovery and process identity, the spawn marker |
| [`lock.rs`](../crates/branchyard/src/lock.rs) | The data directory lock |
| [`tests/durable.rs`](../crates/branchyard/tests/durable.rs) | A killed engine recovered (orphaned harness and its child killed, prompt received once, session continued), a crash before submit, a harness whose pid was never recorded found by its marker, stale reservations freed and live ones kept, a Microsandbox sandbox that this build cannot destroy reported, two yards, a superseded lease, a replayed snapshot, a merge cut short, cursors and waits, the import; input steered from this process into a turn a child process runs, delivered and recorded with its sender, an agent without the steering extension refusing it while the turn goes on, and a profile that cannot steer refused up front |
| [`tests/substrate.rs`](../crates/branchyard/tests/substrate.rs) | A killed engine's work brought back from its actor and in the recovered candidate; not applied over a worktree changed since |
| [`tests/graph.rs`](../crates/branchyard/tests/graph.rs) | A dependent started by the process that integrates its prerequisite; a prerequisite whose engine was killed (SIGKILL) blocking its dependent on recovery; `resume_graph` starting a dependent no engine started |
| [`tests/delegation.rs`](../crates/branchyard/tests/delegation.rs) | A subtree another yard drives waited for; a child whose engine stopped, or with no lease holder, recovered by `wait_subtree` and `Delegate::wait` |
| [`server tests/lock.rs`](../crates/branchyard-server/tests/lock.rs) | A second server on one data directory refused |
| The server's [`store.rs`](../crates/branchyard-server/src/store.rs) and [`ops.rs`](../crates/branchyard-server/src/ops.rs) unit tests, [`postgres.rs`](../crates/branchyard-server/tests/postgres.rs) | Admission binds the key, takes the locks and enqueues together; a trigger-injected queue failure rolls it back (SQLite and PostgreSQL); claims fenced after expiry; a crash between admission and execution run once by another registry and by a `--worker` process; an expired claim taken over, running a queued operation and interrupting a started one without running it again; two servers on one database running each operation once, replaying one key and holding branch locks across servers; tenant quotas: a refusal writing nothing (SQLite and PostgreSQL), `max_running` holding for eight admissions raced across two servers on one database and across a restart with a queued operation (both stores), `max_branches` counting existing and planned branches once, and a worker without credentials running another tenant's operation as its principal without it becoming visible to other tenants; spawns with `depends_on` run by a worker alone, the dependent starting after the worker integrates its prerequisite; two servers and a worker resuming graphs on one database, starting a dependent once |
| `sqlite.rs`, `proc.rs`, `placement.rs`, `lock.rs` unit tests | Fencing after takeover, expiry, step replay, cancels bound to a turn; reservations freed only from a gone engine or once expired; marked processes killed in any group; a sandbox destroyed through a stand-in provider; the lock refused to a second holder |
| [`cli.rs`](../crates/branchyard-cli/tests/cli.rs), [`remote.rs`](../crates/branchyard-cli/tests/remote.rs), [`api.rs`](../crates/branchyard-server/tests/api.rs) | `by cancel` from another process, `by --remote cancel`, HTTP cancel, `by send --steer` into a turn another process runs, locally and remotely, HTTP steer, the SSE stream from the store across a restart |
