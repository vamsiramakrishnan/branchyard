# Shared storage: artifacts and scratch areas

Two primitives for branches that collaborate rather than work in isolation: immutable **artifacts**, published once and read by authorized branches, and **scratch areas**, a named shared directory with one writer at a time. Both follow design §7's "Shared workspace: authorized participants; enforced single writer per mutable scope", and both are implemented in [`crates/branchyard/src/storage.rs`](../crates/branchyard/src/storage.rs), tested hermetically against the fake ACP agent, SQLite and PostgreSQL.

> **Status.** Built and tested in local mode: publish, list, get, share, GC, the delegation surfaces (`by`, Python, `Delegate`, MCP) inside and outside a harness, and a scratch lock reclaimed once its holder's turn ends, across two `Yard` handles on one repository. **Not yet built:** `by --remote`, the server's HTTP API, and a Substrate actor's access to a scratch area (it has no host mounts; see below). Metadata is a small trait, `StorageBackend`, kept separate from the branch-lifecycle `Backend` trait so this feature's tables do not enlarge it; both SQLite and PostgreSQL implement it on the same connection as their `Backend`, and one conformance suite (`storage` in [`conformance.rs`](../crates/branchyard/src/conformance.rs)) runs against both.

## Artifacts

An artifact is immutable bytes with provenance:

```rust
pub struct ArtifactRef {
    pub id: String,
    pub digest: String,        // blake3, hex, lower case
    pub size: u64,
    pub name: String,
    pub media_type: String,
    pub publisher_branch: String,
    pub turn: u64,
    pub created_at: u64,
    pub labels: BTreeMap<String, String>,
}
```

`digest` is the artifact's content identity: the blake3 hash of its bytes (blake3 was already a transitive dependency, pulled in by Microsandbox's image tooling; it is now a direct one of the `branchyard` crate). `id` identifies one publish, so publishing identical bytes twice, from the same or different branches, records two provenance rows over one stored blob, deduplicated by digest: bytes are written once to `.branchyard/artifacts/<digest[..2]>/<digest>` (a server's data directory, once served remotely) and kept until no artifact row references that digest.

Publishing checks the digest only on write; a read re-hashes the stored bytes and refuses if they no longer match what was recorded, rather than silently serving corrupted content.

### Read grants follow the delegation tree

A branch reads an artifact it published, its ancestors' (any branch above it, all the way to the root it was spawned under), and its descendants' (anything spawned below it, transitively). A **sibling** — another branch delegated by the same parent, but not an ancestor or descendant of the publisher — needs an explicit share:

```rust
fn readable(reader, publisher, ancestry_of_publisher_at_publish_time, explicit_shares) -> bool {
    reader == publisher
        || ancestry_of_publisher_at_publish_time.contains(reader)  // reader is an ancestor
        || is_descendant(publisher, reader)                        // reader is a descendant
        || explicit_shares.contains(reader)
}
```

The ancestor check does not need the publisher's branch record to still exist: at publish time, the publisher's ancestor chain (root..parent) is walked once and snapshotted onto the artifact row, so an ancestor's read survives the publisher's later removal. The descendant check is the reverse: it walks *up* from the reader through recorded `parent` names, which needs no record of the publisher, only of the reader and whatever sits between them — so it too survives the publisher's removal, as long as the reader (and anything between it and the publisher) is still live.

`by artifact share ID --to BRANCH`, the SDK's `Yard::share_artifact`/`Delegate::share_artifact`, and the MCP `share_artifact` tool grant this explicitly; the actor sharing must itself be able to read the artifact (typically the common parent, which is always an ancestor of both siblings).

### Garbage collection

"Artifacts survive branch removal only if referenced by a live branch" (design §7). On `Yard::remove`, [`gc_after_removal`](../crates/branchyard/src/storage.rs) checks every artifact the removed branch published: if no other live branch can still read it (an ancestor, a live descendant, or an explicit share), its metadata row is deleted and, if no other row now references its digest, its bytes are too. An artifact an ancestor can still read survives indefinitely, even after the publisher and every other descendant are gone — matching "an ancestor of the publisher" never expiring while it is itself alive. Tested: [`removing_an_unreferenced_publisher_gcs_its_artifact`](../crates/branchyard/tests/storage.rs) and [`an_ancestors_read_survives_the_publishers_removal`](../crates/branchyard/tests/storage.rs).

### Every surface

| Surface | Publish | List | Get | Share |
|---|---|---|---|---|
| `by` | `by artifact publish FILE [--name N] [--label K=V]... [--branch B]` | `by artifact list [--branch B]` | `by artifact get ID --out PATH [--branch B]` | `by artifact share ID --to BRANCH [--branch B]` |
| Python | `branchyard.publish(path, name=, labels=)` | `branchyard.list_artifacts()` | `branchyard.get_artifact(id, out)` | `branchyard.share_artifact(id, to)` |
| Rust `Delegate` | `Delegate::publish_artifact` | `Delegate::artifacts` | `Delegate::read_artifact` | `Delegate::share_artifact` |
| MCP | `publish_artifact` | `list_artifacts` | `get_artifact` | `share_artifact` |
| SDK | `Yard::publish_artifact`, `Branch::publish` | `Yard::artifacts`, `Branch::artifacts` | `Yard::read_artifact`, `Branch::read_artifact` | `Yard::share_artifact` |

`--branch` names the acting branch outside a harness (as `spawn --parent` does); inside a harness it is the harness's own and `--branch` is refused if it names anyone else. A path a delegation surface gives (`by artifact publish`, the Python module, `Delegate`, MCP) is resolved against the acting branch's own worktree when relative, so a harness can `by artifact publish output.txt` from its own working directory; the SDK's `Yard::publish_artifact` takes any path directly, since it is not run from inside a worktree.

`by --remote`, the HTTP API and `branchyard-client` do not reach artifacts yet: `by artifact` and `by scratch` against `--remote` refuse with a clear `unsupported` error rather than pretending to work. A size limit for a future remote upload/download path is defined as [`storage::DEFAULT_ARTIFACT_LIMIT`] (512 MiB), intended to be configurable by the server's operator, as the task asked; it is not enforced yet because nothing uploads over the network yet.

## Scratch areas

A scratch area is a named shared directory for a subtree: `by scratch create NAME` (or `Yard::create_scratch`) creates `.branchyard/scratch/NAME/`, owned by the creating branch. The same read-authorization rule as artifacts governs who may reach it (`Yard::scratch_areas`, `by scratch list`): the owner, its ancestors and descendants, and anyone it is explicitly shared with (`by scratch share NAME --to BRANCH`).

### One writer at a time

`by scratch lock NAME` (`Yard::lock_scratch`, `Delegate::lock_scratch`, the MCP `lock_scratch` tool) acquires a writer lock, reusing the engine's own turn-liveness signal rather than a second lease/heartbeat mechanism: the lock records its holder branch, and it is granted when

- no branch holds it,
- the calling branch already holds it (re-entrant, so a branch's own repeated calls within one turn never refuse each other), or
- the current holder's branch is **no longer running a turn** — the same `BranchStatus::Running` the engine's own lease and recovery already track — in which case the lock is silently reclaimed for the new caller.

Otherwise it is refused with `Error::Running`, naming the current holder. `by scratch unlock NAME` (`Yard::unlock_scratch`) releases it explicitly; "released on unlock or turn end" is true either way, because a turn's end always clears its branch's `Running` status, which the next `lock_scratch` call observes.

**Enforcement limits, stated honestly (design §7): "A database lease cannot fence an arbitrary live filesystem writer."** This lock is a **policy**, not a filesystem fence:

- It stops every caller that goes through `lock_scratch`/`by scratch lock` from writing without holding the lock — that is the whole surface this feature adds.
- It does **not** stop a process that already has the directory open, or one that writes to it without calling `lock_scratch` at all (a harness's own tool calls are not filtered by scratch area; nothing inspects which files a shell command touches).
- In local mode, the directory is an ordinary directory on the host filesystem: every process on the host can read and write it regardless of any lock, exactly as every branch's worktree already is (`docs/design.md` §4, "no isolation beyond the operating-system user").
- Readers get read-only access **where the OS allows it**: local mode does not remount the directory read-only for a non-holder, because doing so would also block the holder's own writes on a shared bind mount without per-process enforcement; this is the same limit local mode already states for every other shared path. A future sandboxed provider that mounts the area per-branch could enforce this by permissions per mount (see below); local mode cannot.

### Exposure per provider

| Provider | How a turn reaches its authorized scratch areas |
|---|---|
| Local | An environment variable per area: `BRANCHYARD_SCRATCH_<NAME>` (the name upper-cased, `-` to `_`), set to the area's host directory — the same directory every other authorized branch sees, since local mode has no isolation. |
| Microsandbox | A read-write mount per area at `/branchyard/scratch/<name>` (a stand-in for the same enforcement Local's mount already has, per `docs/providers.md`'s mount table), and the same `BRANCHYARD_SCRATCH_<NAME>` variable set to that guest path. The lock itself is unchanged: the mount alone would let any branch's sandbox write it regardless of who holds `lock_scratch`. |
| Substrate | None. An actor has no host mounts (`docs/providers.md`: "It does not mount"); implementing scratch-area transfer as git-bundle-style copies in and out, the way the worktree and home already cross, is future work. A Substrate-provider turn simply gets no `BRANCHYARD_SCRATCH_*` variable and no mount, whether or not it is authorized for one — this is a capability gap, not a refused turn, matching how Substrate already omits mounts silently rather than failing every turn. |

Tested in [`tests/storage.rs`](../crates/branchyard/tests/storage.rs): `a_turn_gets_its_authorized_scratch_areas_as_environment_variables` (through the fake ACP agent's `ENV` convention, which prints a named variable's live value, so the assertion is against the actual environment a real harness process would see, not a mock) and `a_scratch_lock_is_one_writer_at_a_time_across_two_processes` (two independent `Yard::open` handles on one repository, standing in for two engine processes, as the existing durability tests already do for branch leases).

### Every surface

| Surface | Create | List | Lock | Unlock | Share |
|---|---|---|---|---|---|
| `by` | `by scratch create NAME [--branch B]` | `by scratch list [--branch B]` | `by scratch lock NAME [--branch B]` | `by scratch unlock NAME [--branch B]` | `by scratch share NAME --to BRANCH [--branch B]` |
| Python | `branchyard.create_scratch(name)` | `branchyard.list_scratch()` | `branchyard.lock_scratch(name)` | `branchyard.unlock_scratch(name)` | `branchyard.share_scratch(name, to)` |
| Rust `Delegate` | `Delegate::create_scratch` | `Delegate::scratch_areas` | `Delegate::lock_scratch` | `Delegate::unlock_scratch` | `Delegate::share_scratch` |
| MCP | `create_scratch` | `list_scratch` | `lock_scratch` | `unlock_scratch` | `share_scratch` |
| SDK | `Yard::create_scratch` | `Yard::scratch_areas` | `Yard::lock_scratch` | `Yard::unlock_scratch` | `Yard::share_scratch` |

A scratch area's name is lowercase `[a-z0-9-]`, starting with a letter, at most 48 characters (it becomes an environment-variable suffix, a directory name and, for Microsandbox, a mount path segment).

## Storage backend

`StorageBackend` (a trait separate from the branch-lifecycle `Backend`, see [`state.rs`](../crates/branchyard/src/state.rs)) holds:

| Table (SQLite / PostgreSQL) | Key | Holds |
|---|---|---|
| `artifacts` / `by_artifacts` | `id` | Provenance, the publisher's ancestor chain at publish time, labels |
| `artifact_shares` / `by_artifact_shares` | `(id, branch)` | Explicit shares |
| `scratch_areas` / `by_scratch_areas` | `name` | Owner, its ancestor chain at creation time |
| `scratch_shares` / `by_scratch_shares` | `(name, branch)` | Explicit shares |
| `scratch_locks` / `by_scratch_locks` | `name` | Current holder branch, acquired time; no separate generation or heartbeat — see above |

Both backends implement `StorageBackend` on the same struct, and so the same connection, as their `Backend` implementation; `Store` holds both as separate `Arc<dyn Trait>` coercions of one underlying value, so this feature's methods do not enlarge the `Backend` trait every branch-lifecycle backend must implement.

## Code and tests

| Where | What |
|---|---|
| [`storage.rs`](../crates/branchyard/src/storage.rs) | `ArtifactRef`, `ScratchArea`, `ScratchLock`, the `StorageBackend` trait, publish/list/get/share, create/list/lock/unlock/share for scratch, the ancestor/descendant grant check, GC after removal, unit tests for grants surviving a publisher's removal |
| [`sqlite.rs`](../crates/branchyard/src/sqlite.rs), [`pg.rs`](../crates/branchyard/src/pg.rs) | `StorageBackend` for `Sqlite` and `Postgres`, their tables |
| [`conformance.rs`](../crates/branchyard/src/conformance.rs) | The `storage` conformance check: digest dedup and refcounting, ancestor/descendant/sibling grants, a scratch lock reclaimed once its holder is not running, held while it still is |
| [`tests/storage.rs`](../crates/branchyard/tests/storage.rs) | End to end against the fake ACP agent: reads following the tree, sibling refusal until shared, GC on removal (referenced and unreferenced), a scratch lock across two `Yard` handles, environment-variable exposure in a real turn |
| [`crates/branchyard-mcp/tests/delegation.rs`](../crates/branchyard-mcp/tests/delegation.rs) | `artifacts_and_scratch_areas_are_reachable_over_mcp`: publish, create, and a descendant reading and locking without an explicit share, all over the real MCP server |
| [`crates/branchyard-cli/tests/cli.rs`](../crates/branchyard-cli/tests/cli.rs) | `by artifact`/`by scratch` end to end outside a harness, and through the Python module inside one |
| [`engine.rs`](../crates/branchyard/src/engine.rs), [`placement.rs`](../crates/branchyard/src/placement.rs) | Scratch-area environment variables (local) and mounts (Microsandbox) added to a turn's environment |
| [`ops.rs`](../crates/branchyard/src/ops.rs) | `gc_after_removal` called from `remove` |

## Not guaranteed

- **Filesystem fencing of a scratch area's writer.** As stated above: the lock is a cooperative policy for callers that go through it, not an OS-level mount fence. Design §7 is explicit that a database lease cannot do this; nothing here claims otherwise.
- **`by --remote`, the server's HTTP API, and `branchyard-client`** for artifacts or scratch areas. Both refuse clearly (`unsupported`) rather than silently no-op.
- **A Substrate actor's access to a scratch area.** No transfer path exists yet; it simply gets none.
- **Cross-tenant deduplication or existence leaks.** Not applicable yet, since artifacts are scoped to one repository's store; design §7's caution applies once a server holds several tenants' artifacts in shared storage.
- **An artifact upload/download size limit enforced anywhere.** `DEFAULT_ARTIFACT_LIMIT` is defined for the remote path this document says is not built.
