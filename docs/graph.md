# Task graphs

A delegating branch can do more than spawn children one at a time: its children may depend on one another, and it can change its graph of children in one atomic step. A dependent child is created at once and starts only when the siblings it depends on have settled. Nothing is declared up front: each branch grows its own graph at runtime, and a child may grow its own below it (parent, child, grandchild), within the [delegation](delegation.md) envelope.

This works in local mode and through a server that allows delegation, including one of several servers and `by worker` processes sharing a PostgreSQL database, tested against the fake ACP agent on SQLite and PostgreSQL; no real harness has used it yet.

## Dependencies

A child created with `depends_on` names other children of the same parent, its siblings, including ones created in the same proposal. It is created `waiting`: its record, budget reservation and name exist, but it has no worktree and runs no turn. It starts its first turn once every prerequisite has **settled**:

| `after` | A prerequisite counts as done when it is |
|---|---|
| `settled` (default) | `ready`, `no_changes` or `merged` |
| `integrated` | `merged` into the parent's own branch (`by integrate`), or `no_changes`, which leaves nothing to integrate |

Either way, a prerequisite whose candidate the parent's git branch contains counts as integrated, whatever its recorded status says: one that was interrupted and then integrated, or that the parent merged by hand.

A prerequisite that ends `failed`, `interrupted` (cancelled, or recovered after its engine stopped with nothing written), `budget_exceeded` or `blocked`, or that is removed, marks the dependent **`blocked`**, with the reason: `{"state": "blocked", "reason": "its prerequisite lint failed: ..."}`. What depends on a blocked branch is blocked in turn. A blocked branch never ran.

The verdict is taken from current facts, not from the event that blocked it: whenever a prerequisite's state changes (its turn ends, it is integrated, it is recovered), each dependent blocked by it is judged again, and one whose prerequisites no longer block it is reopened to `waiting`, in a compare-and-swap, with a warning on its log, and starts once they have settled. So a dependent blocked because its prerequisite was interrupted starts once that prerequisite is continued, or integrated. A proposal that adds or removes one of a blocked branch's dependencies reopens it too. A prerequisite that is `ready` but not yet integrated keeps an `after: integrated` dependent waiting for as long as it takes.

`waiting` and `blocked` are two new `BranchStatus` states. Existing records and JSON read as before; a client written against the earlier set of states sees two it does not know.

**Where it starts from.** A dependent starts from its parent's git branch (`by/<parent>`) as it is when it starts: the parent's latest candidate, and every child integrated into it by then, which is how an `after: integrated` dependent builds on its prerequisite's work. Unlike a child that starts at once, its parent's uncommitted changes are not snapshotted first, since the parent's harness may be in the middle of editing. With an explicit `base`, it starts from that revision, resolved when the proposal was made. `Spawned.base` is empty until it starts; `inspect` shows the base once it has.

**While it waits**, a `send` to it is refused (`denied`), `inspect` and `graph` show it `waiting`, `wait` keeps waiting (in every surface: `Delegate::wait`, `by spawn --wait`, `branchyard.wait`, the fake agent's `MCP wait`), and `cancel` ends it `interrupted` at once, without a turn, which blocks what depends on it. It is removed like any branch.

## Atomic graph proposals

A proposal is a list of edits and the revision of the parent's graph it was made against:

```json
{"expected_revision": 3, "edits": [
  {"kind": "spawn", "name": "schema", "prompt": "Add the migration", "budget": {"max_usd": 0.5}},
  {"kind": "spawn", "name": "api", "prompt": "Use the new column", "depends_on": ["schema"], "after": "integrated"},
  {"kind": "add_dependency", "dependent": "docs", "prerequisite": "api"},
  {"kind": "remove_dependency", "dependent": "docs", "prerequisite": "lint"}
]}
```

| Edit | Fields |
|---|---|
| `spawn` | Every field of the `spawn` tool (`prompt`, `name`, `harness`, `base`, `budget`, `check`, `max_depth`, `max_children`, `harnesses`, `deny`, `seat`, `connectors`, `plan`, `model`), and `depends_on`, `after`, `bindings`. `check` is a literal argv array (`["cargo", "test"]`), not a string to shell-split like `by spawn --check "cargo test"` |
| `add_dependency` | `dependent`, `prerequisite`, optional `after`. The dependent must be `waiting` or `blocked`, or spawned in the same proposal |
| `remove_dependency` | `dependent`, `prerequisite`. The same condition |

The whole proposal is validated before anything is written:

- **Authority.** The caller edits its own graph: every spawn becomes its child, and every dependency joins two of its children (existing ones or spawns in the proposal). A grandchild, a sibling's child or any branch outside the subtree is refused (`denied`). A child edits its own graph the same way, one level down.
- **Envelope and budget, for all spawns together.** Each spawn is checked as `spawn` checks it, counting the spawns before it in the proposal: `max_children`, seat instances, the parent's remaining budget minus every earlier spawn's `max_usd`, harnesses, depth, denials, provisioning, [bindings](#bindings).
- **The graph.** Names are unique and free; no child depends on itself; the dependencies after the proposal have no cycle (`a waits for b waits for a`); an edge added twice or removed when absent is refused.
- **The revision.** `expected_revision` must be the parent's current graph revision, or the proposal is refused with `stale_revision` (HTTP 409, with `{"expected", "actual"}` in the error's `detail`). Read the graph again and propose against what it is now.
- At most 64 edits.

Then it commits in **one store transaction**: every child's record, the parent's list of children, the dependency rows, a `blocked` dependent reopened, and the revision, bumped by one. The transaction checks the revision, the names and each dependent's state again, so a proposal that raced another commits whole or not at all. **Any invalid edit leaves state, reservations and the revision unchanged**: no record, no name, no budget held, no dependency, no revision; the refusal is recorded on the caller's log as a refused `apply_graph`, like every refused delegation operation.

After the commit, each child whose prerequisites have already settled, and every child with none, starts at once from the parent's current work, as a spawn does, and the call returns `GraphApplied`: `{branch, revision, spawned: [Spawned], dependencies: [Dependency]}`, each spawned child `running`, `waiting` or `blocked` as it was when the call returned.

A plain `spawn` is a one-edit proposal made against whatever the revision is, so it bumps the revision too; `spawn` with `depends_on` creates a waiting child the same way. A proposal and a spawn in the same process are serialized; across processes the revision check decides.

## Surfaces

| Surface | Apply a proposal | Show a graph | A spawn that waits |
|---|---|---|---|
| Rust | `Delegate::apply_graph(edits, expected_revision)` | `Delegate::graph(branch)`, `Yard::graph(branch)` | `Spawn { depends_on, after, bindings, .. }` |
| `by` | `by graph apply FILE` (`-` for stdin), or `--edits JSON --expected-revision N`; outside a harness `--parent BRANCH` and `--yes`/`--ask` | `by graph show [BRANCH]` | `by spawn --depends-on a,b [--after integrated] [--bind NAME:ACCESS]` |
| Python | `branchyard.apply_graph(edits, expected_revision)` | `branchyard.graph(branch=None)` | `branchyard.spawn(..., depends_on=[...], after=..., bindings={...})` |
| MCP | `apply_graph` `{expected_revision, edits}` | `graph` `{branch?}` | `spawn` with `depends_on`, `after`, `bindings` |
| HTTP | `POST /v1/repos/{repo}/branches/{branch}/graph` `{expected_revision, edits, policy?, unapproved_tools?}` | `GET …/graph` | `depends_on`, `after`, `bindings` in `POST …/spawn` |
| client | `Repo::apply_graph` | `Repo::graph` | `SpawnRequest` fields |
| `by --remote` | the same `by graph apply --parent` | `by graph show` | the same `by spawn` flags |

`Graph` is `{branch, revision, children: [{name, status, depends_on, bindings, seat}], dependencies: [{dependent, prerequisite, after}]}`; `after` is omitted when `settled`. `Inspection` gains `graph_revision` (the branch's own graph, omitted while 0), `depends_on` (what it waits for) and `bindings`; `Spawned` gains `depends_on`. All of them are omitted when empty, so existing JSON is unchanged. `by --remote` prints the same JSON as `by` for each, and the same errors; a test compares them command for command.

Every surface calls the same operation, so the answers and refusals are the same. `POST …/graph` is not an operation: the proposal commits before the answer, the children it starts run on the server's threads, and their siblings' turns start the rest. A retried request after a lost connection is refused `stale_revision` if the first attempt committed, so a proposal never applies twice. `by graph apply` outside a harness runs the children on its own threads and waits for them, and for the dependents they start, before it prints; inside a harness it returns at once.

`by --allow-delegation` (`Policy::allow_delegation_commands`) allows `by graph` too.

## Durable scheduling

Dependencies are rows in the store, beside the branch records, in SQLite and PostgreSQL alike ([durability](durability.md#the-store)): `graph_edges` (`parent`, `dependent`, `prerequisite`, `after`) and `graph_revisions` (`parent`, `revision`), `by_graph_edges` and `by_graph_revisions` on PostgreSQL, keyed by the repository's scope. A dependent is started by a compare-and-swap, `claim`: in one transaction, only if its stored status is still `waiting` and no lease is held, it becomes `running` and its lease is taken for its first turn. Whichever engine gets there first starts it; every other finds it claimed and does nothing. Marking it `blocked` is the same kind of swap.

Who starts it:

- **The engine that settles a prerequisite, in any process.** When a turn ends, its engine looks at what depends on that branch; when a child is integrated, so does the integrating engine. A dependent whose prerequisites have all settled is claimed and started on a thread of that process; one with a failed prerequisite is blocked. So a prerequisite continued by `by send` in one process, or integrated by `by integrate` in another, starts its dependent there, and that command waits for it before it exits.
- **Recovery.** A prerequisite recovered after its engine stopped ends `interrupted` (or with its journaled result, or `ready` when its lost turn had written its work; [durability](durability.md#recovery)), and recovery blocks its dependents (or, if it had settled, leaves them to the next point below).
- **Integration.** Integrating a prerequisite, or the parent's branch coming to contain its candidate however it got there, settles its `after: integrated` dependents and reopens those it had blocked.
- **Waits.** `wait_subtree` (and so `by run`, `by spawn`, `by graph apply` and every server operation that waits for a subtree) and `Delegate::wait` start a waiting dependent whose prerequisites have settled, when this process applied its proposal. A crash between a prerequisite settling and its dependent starting is picked up there.
- **`Yard::resume_graph(options)`** (`by graph resume [--yes]` locally) starts every such dependent in the repository under the given options, reopens a blocked one whose prerequisites no longer block it, and wakes a parent [waiting on its children](delegation.md#waiting-on-children) whose children have all settled. Every server process, and every `by worker`, does this for each repository it serves on its recovery interval (`recover_interval`, every 30 seconds), right after recovering branches whose engine stopped; see [below](#with-the-servers-queue).

A wait for a subtree covers what the subtree's turns start: a branch that depends on one in the subtree, and its descendants, so `by send child` waits for the sibling that `child`'s turn started.

**The policy a dependent runs under.** A policy cannot be stored (it may ask a person), so a dependent's first turn runs under the options of the proposal that created it when the process that starts it applied that proposal; otherwise under the options of the turn that settled its prerequisite (its sibling's, which are its parent's too); otherwise under what `resume_graph` was given, which on a server is the default policy, deny. Its own limits and denials come from its record either way.

## With the server's queue

A server admits `POST …/spawn` as it admits every operation: a durable queue row whose description is the whole `SpawnRequest`, `depends_on`, `after` and `bindings` included, run by whichever worker claims it, in any server or `by worker` process sharing the database ([server](server.md#dispatch)). The worker creates the child as `spawn` does locally: `waiting` when a prerequisite has not settled, in which case the operation finishes `succeeded` with the child's inspection showing `waiting`, and the child starts later in whichever process settles or integrates its prerequisite (an `integrate` operation's worker, say), under that process's options as [above](#durable-scheduling). The operation locks the parent and the new child while it runs, as every spawn does; it does not hold them while the child waits.

`POST …/graph` is not an operation and does not go through the queue: the proposal commits in one store transaction before the response, as locally, and the children with nothing to wait for start on the answering server's threads. A retry after a lost response is refused `stale_revision`, which is what an idempotency key would give it.

**Who resumes graphs.** Every server and every `by worker` runs `resume_graph` for each served repository on its recovery tick (`Config::recover_interval`, 30 seconds by default; the same tick recovers branches whose engine stopped). Nothing elects one of them: with several processes on one PostgreSQL database they all look, and the `claim` compare-and-swap (`waiting` to `running` with the first lease, only if the stored status is still `waiting` and no lease is held) lets exactly one start each dependent; the others find it claimed and skip it. A dependent started this way runs on a thread of the process that claimed it, outside any operation, under the default policy (deny) unless that process applied the proposal that created it. So a `by worker` resumes graphs even though it serves no HTTP, and a deployment with servers only, workers only, or both needs nothing more. Tests race six yards' `resume_graph` on one database behind a barrier, and two servers and a `by worker` with a 100 ms tick, and check that the dependent ran one turn.

## Bindings

A spawn, or a rig seat, may bind the child to [scratch areas](storage.md#scratch-areas): `bindings: [{scratch: NAME, access: read_only | exclusive_write}]`, `by spawn --bind NAME:read_only` (or `NAME:exclusive_write`, repeatable; `ro`, `rw` for short), and in a rig spec `bindings = ["notes:exclusive_write"]` on a seat.

- **At plan time**, each binding must name a scratch area that exists, once, that the child will be able to read. A new child has no shares of its own yet, so that means an area its parent or one of the parent's ancestors owns; binding an area a sibling owns is refused. The whole proposal is refused with it.
- **`exclusive_write`** takes the area's writer lock (`lock_scratch`) for each of the child's turns, when the turn starts, and releases it when the turn ends. A turn that cannot take it, because another branch's running turn holds it, ends `failed` with the reason and runs nothing; a later `send` tries again.
- **`read_only`** checks, when each turn starts, that the child can still reach the area.
- A seat's bindings apply to every child in it; a spawn by seat may add bindings, not change the access the seat gives an area.

Enforcement is what [storage](storage.md#one-writer-at-a-time) says of scratch areas: the lock is a cooperative policy for callers that go through it, not a filesystem fence. A `read_only` child can still write the directory: its path is in its environment like any authorized area's, and nothing filters what its tools touch; `read_only` promises only that the child does not hold the lock. An `exclusive_write` binding keeps other bound and locking branches out while its turn runs; it does not stop a process that writes without asking.

## What PR #1's design had that this does not

Branchyard's first graph design (an unmerged server protocol) carried task and graph revisions per node, ownership separate from dependencies, and component bindings. This keeps the semantics that fit branches: proposals validated whole against the final graph independent of edit order, one transaction for records, reservations and revision, `409` on a stale revision, dependencies between siblings, and typed bindings. It does not have a per-task revision on every node, spawning under an arbitrary descendant in one proposal (a child grows its own graph instead), or dependencies across different parents.

## Not guaranteed

- **A dependent's policy after a restart.** One started by `resume_graph`, or by an engine that neither applied its proposal nor ran a sibling's turn, runs under that caller's options: on a server after it restarts, the default policy, deny.
- **A dependent that waits forever.** `after: integrated` waits for as long as the prerequisite is not integrated; nothing times it out. A wait for the subtree returns once nothing runs, leaving it `waiting`.
- **Filesystem enforcement of bindings**, as above.
- **Real harnesses.** Tested against the fake ACP agent only.

## Code and tests

| Where | What |
|---|---|
| [`graph.rs`](../crates/branchyard/src/graph.rs) | Edits, proposals, `Graph`, `Binding`, the `GraphBackend` trait, the verdict on a waiting branch, `advance`, `start`, `resume_graph`, bindings, cycle detection |
| [`delegation.rs`](../crates/branchyard/src/delegation.rs) | `Delegate::apply_graph`, `graph`; validating a whole proposal (`plan_child` for each spawn, counting the ones before it) and committing it; spawn as a one-edit proposal; waits that cover dependents; cancelling a waiting child |
| [`sqlite.rs`](../crates/branchyard/src/sqlite.rs), [`pg.rs`](../crates/branchyard/src/pg.rs) | `GraphBackend` for both stores: `commit_graph` in one transaction, `claim` and `settle_waiting` as compare-and-swaps |
| [`conformance.rs`](../crates/branchyard/src/conformance.rs) | The `graph` check on both stores: a stale revision, a taken name, a duplicate or missing edge and an edit on a started branch each change nothing; a claim wins once; a blocked branch reopened by an edit; removal |
| [`tests/graph.rs`](../crates/branchyard/tests/graph.rs) | A dependent starting only after its prerequisite settles, from the parent's head; blocked dependents (failed, cancelled, transitively) and reopening, by a graph edit or when a blocking prerequisite settles; a dependent blocked by an interrupted prerequisite starting once it is continued and integrated; a prerequisite whose engine died after it wrote its work recovering `ready`, its dependent waiting and then started by `resume_graph`; fifteen invalid proposals each leaving state, names, budget and revision unchanged; a three-level graph built at runtime; `after: integrated` starting from the merge; a dependent started by the process that integrates its prerequisite; a prerequisite whose engine was killed blocking its dependent on recovery; `resume_graph` starting a dependent no engine started; bindings; the tool and the typed call agreeing |
| [`branchyard-mcp` `tests/delegation.rs`](../crates/branchyard-mcp/tests/delegation.rs) | The M5 gate over the real MCP server: a root harness applies a graph whose child's harness applies its own, grandchildren waiting on each other; a stale proposal refused |
| [`branchyard-cli` `tests/cli.rs`](../crates/branchyard-cli/tests/cli.rs), [`remote.rs`](../crates/branchyard-cli/tests/remote.rs) | `by graph` and the Python module inside a harness; `by graph apply` from a file and `by spawn --depends-on` outside one; `by --remote graph` printing the same JSON as local mode, refusals included |
| [`branchyard-server` `tests/parity.rs`](../crates/branchyard-server/tests/parity.rs) | `POST …/graph` and `GET …/graph`: `409 stale_revision`, `403 delegation_not_allowed`, the graph the SDK reads; `POST …/spawn` with `depends_on` through the queue: the operation finishes with the child `waiting`, and integrating its prerequisite (another queued operation) starts it from the merge |
| [`branchyard-server` `tests/postgres.rs`](../crates/branchyard-server/tests/postgres.rs), [`branchyard` `tests/postgres.rs`](../crates/branchyard/tests/postgres.rs) | Spawns that wait, admitted into the queue and run by a `by worker` alone, starting after the worker integrates the prerequisite; two servers and a `by worker` on one database, all resuming graphs every 100 ms, starting a satisfied dependent once; six yards racing `resume_graph` on one database, one winner |
| [`branchyard-server` `work.rs`](../crates/branchyard-server/src/work.rs) | A `Work::Spawn` description round-trips `depends_on`, `after` and `bindings` |
