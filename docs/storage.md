# Shared storage: artifacts and scratch areas

Two primitives for branches that collaborate rather than work in isolation: immutable **artifacts**, published once and read by authorized branches, and **scratch areas**, a named shared directory with one writer at a time. Both follow design §7's "Shared workspace: authorized participants; enforced single writer per mutable scope", and both are implemented in [`crates/branchyard/src/storage.rs`](../crates/branchyard/src/storage.rs), tested hermetically against the fake ACP agent, SQLite and PostgreSQL.

> **Status.** Built and tested in local mode and remotely: publish, list, get, share, GC, the delegation surfaces (`by`, Python, `Delegate`, MCP) inside and outside a harness, a scratch lock reclaimed once its holder's turn ends, across two `Yard` handles on one repository, and now `by --remote artifact`/`by --remote scratch`, the server's HTTP API and `branchyard-client`, acting with the server's authority as a person exactly as local `by artifact … --branch B` does (see [Remote mode](#remote-mode) below). **Not yet built:** a Substrate actor's access to a scratch area (it has no host mounts; see below). Metadata is a small trait, `StorageBackend`, kept separate from the branch-lifecycle `Backend` trait so this feature's tables do not enlarge it; both SQLite and PostgreSQL implement it on the same connection as their `Backend`, and one conformance suite (`storage` in [`conformance.rs`](../crates/branchyard/src/conformance.rs)) runs against both.

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
| `by` | `by artifact publish FILE [--name N] [--media-type TYPE] [--label K=V]... [--branch B]` | `by artifact list [--branch B]` | `by artifact get ID --out PATH [--branch B]` | `by artifact share ID --to BRANCH [--branch B]` |
| `by --remote` | same, `--branch` required | same, `--branch` required | same, `--branch` required | same, `--branch` required |
| Python | `branchyard.publish(path, name=, labels=, media_type=)` | `branchyard.list_artifacts()` | `branchyard.get_artifact(id, out)` | `branchyard.share_artifact(id, to)` |
| Rust `Delegate` | `Delegate::publish_artifact` | `Delegate::artifacts` | `Delegate::read_artifact` | `Delegate::share_artifact` |
| MCP | `publish_artifact` | `list_artifacts` | `get_artifact` | `share_artifact` |
| SDK | `Yard::publish_artifact`, `Branch::publish` | `Yard::artifacts`, `Branch::artifacts` | `Yard::read_artifact`, `Branch::read_artifact` | `Yard::share_artifact` |
| HTTP | `POST …/branches/{b}/artifacts` (body: bytes) | `GET …/branches/{b}/artifacts` | `GET …/branches/{b}/artifacts/{id}` (metadata), `GET …/artifacts/{id}/content` (bytes) | `POST …/artifacts/{id}/share` |
| client | `Repo::publish_artifact` | `Repo::artifacts` | `Repo::read_artifact` | `Repo::share_artifact` |

Export and import (portable bundles, below) are local-only, so they are not in this table: `by artifact export ID... --out FILE.tar` / `by artifact import FILE`, `Yard`/`Branch::export_artifacts`/`import_artifacts`, `Delegate::export_artifacts`/`import_artifacts`.

`--branch` names the acting branch outside a harness (as `spawn --parent` does); inside a harness it is the harness's own and `--branch` is refused if it names anyone else. A path a delegation surface gives (`by artifact publish`, the Python module, `Delegate`, MCP) is resolved against the acting branch's own worktree when relative, so a harness can `by artifact publish output.txt` from its own working directory; the SDK's `Yard::publish_artifact` takes any path directly, since it is not run from inside a worktree. `by --remote artifact`/`by --remote scratch` always act as a person (there is no harness to delegate as inside `by --remote`, unlike local mode inside a harness), so `--branch` is required rather than defaulting to the harness's own.

### Portable bundles

Idea from Straitjacket's evidence capsule (`src/ctx/capsule.py`, Apache-2.0; see `docs/comparison.md` Absorption plan → Straitjacket): a self-contained, independently verifiable archive of artifacts a receiving process can check without trusting the sender, so a candidate's cited artifacts travel as one file. Nothing is ported, only the shape, built fresh in [`crates/branchyard/src/bundle.rs`](../crates/branchyard/src/bundle.rs) and [`tarball.rs`](../crates/branchyard/src/tarball.rs).

`by artifact export ID... --out FILE.tar` (`Yard::export_artifacts`/`Branch::export_artifacts`) writes every named artifact `reader` may read into one **deterministic** tar: `index.json` first, then each artifact's bytes at `artifacts/<id>`, sorted by id, with a fixed mtime, uid, gid, mode, user and group ([`tarball::write`](../crates/branchyard/src/tarball.rs), on the `tar` crate; `crates/branchyard/tests/golden/bundle.tar` pins its bytes) — exporting the same artifacts twice, in the same order, always produces byte-identical bytes. `index.json` records each artifact's provenance (`id`, `digest`, `size`, `name`, `media_type`, `publisher_branch`, `turn`, `created_at`, `labels`), the same shape as `ArtifactRef`. Every member is checked **member by member**: on export, each artifact's bytes are re-hashed as they are read and refused if they no longer match the store's recorded digest (a corrupted local blob is never exported); on import, every member's size and blake3 digest are checked against its `index.json` entry before anything is published, and the archive's membership is checked both ways — an id the index names but the archive lacks (missing) and a member the archive has but the index does not name (extra) both refuse the *whole* import, nothing partially applied.

`by artifact import FILE` (`Yard::import_artifacts`/`Branch::import_artifacts`) publishes each verified member as a **new** artifact owned by the importing branch, bound to its incarnation like any other publish (the usual grants and GC apply from there); the bundle's original provenance is kept in labels (`bundle.origin_id`, `bundle.origin_publisher`, `bundle.origin_created_at`) rather than assumed to still identify a live branch, since the branch that exported it may be long gone by the time it is imported, possibly into a different repository entirely.

Refused remotely with a clear message: the server has no bundle endpoint, so `by --remote artifact export|import` fails with `Unsupported` naming `by artifact export|import` (local mode) as the alternative, rather than silently doing nothing or half-transferring a large archive over a route not built for it.

Tests ([`crates/branchyard/tests/bundle.rs`](../crates/branchyard/tests/bundle.rs), `crates/branchyard/src/tarball.rs`'s own unit tests): a round trip preserving bytes and provenance, byte-identical re-export, a tampered member refused (content digest mismatch), a missing member refused, an extra member refused, and export denied without read access.

### Remote mode

`by --remote artifact …`, `by --remote scratch …`, the server's HTTP API and `branchyard-client` reach artifacts and scratch areas, acting with the server's authority as a person, bounded exactly like local `by artifact … --branch B`: `{branch}` in each route's path is the acting branch, checked against the same read-authorization rule as local mode (ancestor, descendant, self, or explicit share). `by --remote` prints the same JSON as local mode for every operation (tested by comparing, since artifact ids are per-repository sequence numbers, identical across two freshly created, identical repositories run through the same sequence of commands).

**A path is always the caller's own local file, never the server's.** Unlike `--substrate-key`-style flags, which name an absolute path *on the server*, a path here (`by artifact publish FILE`, `by artifact get ID --out PATH`) resolves against the machine running `by --remote`: publishing reads the file from disk and sends its bytes in the request body; getting writes the downloaded bytes to the given local path. The bytes always cross the network; there is no way to tell the server to read or write its own filesystem directly, which is the point — a remote caller has no other access to the repository's files at all.

The HTTP API:

| Method and path | Does |
|---|---|
| `POST /v1/repos/{repo}/branches/{branch}/artifacts` | Publish the request body as a new artifact of `branch`. `name`, `media_type` and repeated `label` are query parameters (the body is not JSON); `201` with the `ArtifactRef`. `413 body_too_large` over `max_artifact_bytes` |
| `GET /v1/repos/{repo}/branches/{branch}/artifacts` | Every artifact `branch` may read: `{"artifacts": [ArtifactRef]}` |
| `GET /v1/repos/{repo}/branches/{branch}/artifacts/{id}` | Artifact `id`'s provenance only, checked against `branch`'s grant: `ArtifactRef` |
| `GET /v1/repos/{repo}/branches/{branch}/artifacts/{id}/content` | Its bytes, streamed, with `Content-Type`, `Content-Length` and a digest header (`x-branchyard-artifact-digest`) the client checks against the metadata it already fetched |
| `POST /v1/repos/{repo}/branches/{branch}/artifacts/{id}/share` | `{"to": "BRANCH"}`; `{"ok": true}` |
| `POST /v1/repos/{repo}/branches/{branch}/scratch` | `{"name": "NAME"}`; create, owned by `branch`: the `ScratchArea` |
| `GET /v1/repos/{repo}/branches/{branch}/scratch` | Every scratch area `branch` may reach: `{"areas": [ScratchArea]}` |
| `POST /v1/repos/{repo}/branches/{branch}/scratch/{name}/share` | `{"to": "BRANCH"}`; `{"ok": true}` |
| `POST /v1/repos/{repo}/branches/{branch}/scratch/{name}/lock` | Acquire the writer lock for `branch`: the `ScratchLock`, or `409 running` naming the holder |
| `POST /v1/repos/{repo}/branches/{branch}/scratch/{name}/unlock` | Release it if `branch` holds it: `{"ok": true}` |
| `GET /v1/repos/{repo}/scratch/{name}/lock` | The current holder, if any (not scoped to a branch: like `Yard::scratch_lock_state`, this is not access controlled): `{"lock": ScratchLock?}` |

A publish accepts `Idempotency-Key` like the other mutating routes, but through its own in-memory cache (`storage_routes::StorageIdem`), not the operation registry's durable one: publish finishes within the request, so it needs nothing like an operation to poll, and this cache does not survive a restart (like branch locks; see `docs/server.md`'s "What is durable" table). The other storage routes (`create`, the shares, lock, unlock) do not check an idempotency key: each is naturally safe to repeat — a share or an unlock is a no-op the second time, a lock is re-entrant for its own holder, and a repeated `create` gets a clear "already exists" the caller can treat as success.

`max_artifact_bytes` (`by serve --max-artifact-bytes N`, or the configuration file's `max_artifact_bytes`) bounds a publish's upload; the default is 256 MiB. [`storage::DEFAULT_ARTIFACT_LIMIT`] (512 MiB) is a separate, local-mode-only constant with no enforcement point (nothing bounds a local `by artifact publish`'s file size); the two are unrelated on purpose, since the server's limit protects its own memory and disk, not local mode.

### Scratch areas through the server

`create`/`list`/`share`/`lock`/`unlock` all reach the server; the directory itself does not. A scratch area's directory lives on the server host (`.branchyard/scratch/NAME/` in the served repository), and a remote caller has no route that reads or writes it directly — only a harness the server runs sees it (through `BRANCHYARD_SCRATCH_<NAME>`, exactly as in local mode), or, in the future, a small `get`/`put` of one file added if that turns out to be worth the surface. Until then, a remote caller synchronizes on the lock (so its own out-of-band access to a shared resource is fenced the same cooperative way local mode's is) but exchanges the area's actual bytes some other way — a task's own file transfer tools, or an artifact published from inside a turn that runs there.

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
| [`branchyard-server/src/storage_routes.rs`](../crates/branchyard-server/src/storage_routes.rs) | The HTTP routes above, the upload size limit, the publish idempotency cache |
| [`branchyard-client/src/storage_api.rs`](../crates/branchyard-client/src/storage_api.rs), [`lib.rs`](../crates/branchyard-client/src/lib.rs) | Wire types, and `Repo`'s artifact/scratch methods, digest-verified on download |
| [`branchyard-server/tests/storage.rs`](../crates/branchyard-server/tests/storage.rs) | Over real HTTP: publish/list/get with digest verification, dedup, the size limit, grants (ancestor, sibling refused until shared), a scratch lock's contention across two clients, restart survival |
| [`branchyard-cli/tests/remote.rs`](../crates/branchyard-cli/tests/remote.rs) | `artifact_and_scratch_commands_print_what_local_ones_do`: `by --remote artifact`/`scratch` against the built binaries, JSON compared against local mode command for command |

## Not guaranteed

- **Filesystem fencing of a scratch area's writer.** As stated above: the lock is a cooperative policy for callers that go through it, not an OS-level mount fence. Design §7 is explicit that a database lease cannot do this; nothing here claims otherwise.
- **A remote caller's direct access to a scratch area's directory.** It lives on the server host; `create`/`list`/`share`/`lock`/`unlock` reach it over HTTP, but there is no `get`/`put` of the directory's files (see "Scratch areas through the server" above) — only a harness the server runs sees it.
- **A Substrate actor's access to a scratch area.** No transfer path exists yet; it simply gets none.
- **The publish idempotency cache surviving a restart.** It is in memory, like branch locks; a retried publish after a restart in between records a duplicate provenance row over the same deduplicated bytes (harmless: the bytes are stored once either way).
- **Cross-repository or cross-tenant deduplication, or existence leaks.** Each served repository's artifacts stay in its own `.branchyard/`; design §7's caution applies once a server holds several tenants' artifacts in genuinely shared storage, which this is not.
