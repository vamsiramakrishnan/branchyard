# Sync

Branchyard keeps every task on your machine first and copies it to durable storage you choose: Google Cloud Storage, S3 and S3-compatible stores (R2, MinIO, Ceph), Azure Blob, a git remote, or a directory. A task then survives its machine, follows you to another, and can be run by a server. The principles are in [task repositories](task-repos.md#sync-principles); this page is how they are built.

> **Status.** Built and tested hermetically, 3 October 2026, branch `agent/sync-16`: the crate `branchyard-sync`, `by sync`, `[sync]` in the user configuration, and `sync` in a server's configuration. The directory backend, a git remote (a local bare repository) and in-process stand-ins for S3, Cloud Storage, Azure Blob, the three KMS APIs and the cloud token services run in every test run. No test has reached a real cloud service, MinIO, fake-gcs-server or Azurite; the conformance test runs against those three when their endpoints are configured (below). Tasks today are Branchyard branches; task repositories plug in through one trait ([below](#task-repositories)).

## Quick start

In `~/.config/branchyard/config.toml` (the user file only: a repository must not choose where your tasks are sent):

```toml
[sync]
remote = "gs://my-bucket/branchyard"   # or s3://, az://, file:///, git+https://
encrypt = "passphrase"                 # or "none", or "kms://gcp/projects/.../cryptoKeys/k"
interval = "1m"
bandwidth = "10MB/s"
retention = "90d"
```

```sh
export BRANCHYARD_SYNC_PASSPHRASE=...    # or passphrase_file = "..."
by sync                    # push and pull every branch that changed
by sync status             # remote, device, each task's state and lag, counters
by sync pull 3f2a9c01be47.fix-login     # on another machine
```

| Command | What it does |
|---|---|
| `by sync [TASK]` | Sync one branch (or task ID), or every branch: pull what moved remotely, push what moved here, record a divergence as a conflict branch |
| `by sync status` | The remote, this machine's device name, whether it is encrypted, each task's state (`synced`, `pending`, `failing`, `changed`, `local`) and lag, the queue, and the counters every process added up |
| `by sync pull TASK` | Bring a task from the remote: create or fast-forward its refs |
| `by sync ls` | The tasks in the remote, with sequence, refs, bytes, last writer and holds |
| `by sync gc [--dry-run]` | Collect garbage ([below](#garbage-collection)) |
| `by sync scrub [--sample N] [--seed S]` | Read a sample of objects back and check each against its name; fails when it finds damage |
| `by sync hold TASK [--reason R] [--release]` | Set or release a legal hold |
| `by sync rm TASK` | Delete a task from the remote; refused under a hold |
| `by sync rotate-key [--to WRAPPER]` | Rotate the tenant key ([below](#keys-and-rotation)) |

Every command takes `--json`. `by sync` runs on this machine; with `--remote` it is refused, because a server syncs with its own configuration.

Keys of `[sync]`: `remote`, `encrypt`, `passphrase_file`, `algorithm` (`aes-256-gcm` or `chacha20-poly1305`), `interval`, `bandwidth`, `concurrency` (default 8), `retention`, `grace` (default `24h`, at least 15 minutes), `quota` (such as `50GB`), `device`. They are in `schema/branchyard.config.json`.

## What a task is here

A task is anything that implements `SyncSource`: a git directory, a task ID, refs in the task's own names, a chunk directory and closed conversation segments. Today that is a Branchyard branch (`BranchSource`):

| Task ref | Branch's local ref |
|---|---|
| `refs/heads/main` | `refs/heads/<git branch>` |
| `refs/branchyard/<rest>` | `refs/branchyard/<branch>/<rest>` (checkpoints) |
| `refs/heads/conflict/<device>/<n>` | `refs/heads/conflict/<git branch>/<device>/<n>` |

A branch's task ID is `<repository key>.<branch>`, with `/` as `~`. The repository key is the first 12 hex digits of the repository's root commit. Every clone shares it, so repositories syncing to one remote never mix their tasks. `by sync` and `by sync pull` take a branch name and add the key; a machine without the repository pulls by the full ID from `by sync ls`.

## The bucket

```text
keyring.json                    whether the remote is sealed; wrapped key versions
tasks/<task>/manifest           the task's one mutable object, replaced by compare-and-swap
tasks/<task>/hold               a legal hold
leases/<task>/<attempt>         who runs the attempt, until when
packs/<name>.pack               git packs
indexes/<name>                  lists of a task's chunks
chunks/<aa>/<name>              large-file chunks
segments/<name>                 conversation segments
gc/candidates                   objects seen unreferenced, and since when
gc/usage                        the tenant's stored bytes, for quotas
locks/sweep                     the collector's sweep lock
locks/writers/<device>-<nonce>  syncs uploading now
```

In a plain remote, `<task>` is the task ID and `<name>` the BLAKE3 hash of the content. In an encrypted remote both are keyed hashes, so the bucket learns neither task names nor content hashes. Packs, indexes, chunks and segments are immutable: written once with `put_if_absent`, never changed (rotation rewrites only their wrapped data key, [below](#keys-and-rotation)).

### An object

Every object but `keyring.json` is framed:

```text
"BYS1" | 0 | blake3(payload) (32) | payload                                     plain
"BYS1" | 1 | cipher (1) | key version (4) | wrapped data key (60) | nonce (12) | ciphertext and tag   sealed
```

A sealed object has a random 32-byte data key of its own. The payload is sealed with AES-256-GCM or ChaCha20-Poly1305 (through `ring`), with the object's name in the associated data, so an object moved to another name does not open. The data key is wrapped with AES-256-GCM under the tenant's key of that version, again bound to the name.

### A manifest

```json
{
  "format": 1, "task": "3f2a9c01be47.fix-login", "seq": 7,
  "writer": {"device": "laptop-1a2b", "commit": "<random>", "at_ms": 1790000000000},
  "refs": {"refs/heads/main": "<commit>", "refs/branchyard/1/turn-3": "<commit>"},
  "packs": [{"name": "<name>", "objects": 3, "bytes": 912}],
  "chunk_indexes": [{"name": "<name>", "chunks": 2, "bytes": 310}],
  "segments": {"conversation/0001.jsonl": {"object": "<name>", "bytes": 4096}},
  "ledger_watermark": 42
}
```

The packs hold every object the refs reach. Each sync adds one pack of only what the remote's refs do not reach (`git pack-objects --revs` with the remote's tips excluded), so a turn that changed one file uploads a commit, a tree and a blob. Packs are self-contained (not thin) and imported with `git index-pack --stdin --fix-thin`, which checks every object. The `.idx` is rebuilt on import rather than uploaded.

## The commit protocol

A sync of one task runs in rounds:

1. Read the manifest and its generation (GCS generation, S3 or Azure ETag, git commit, the file's inode and time).
2. Import what this machine lacks: the packs (only when a ref's commit is missing here), chunk indexes and chunks, and segments. Every object read is checked against its name after decryption.
3. Merge ref by ref from the base last agreed with the remote: equal stays; a fast-forward moves the side behind; a ref one side deleted and the other did not touch is deleted; two sides that moved apart keep the remote's value and record the local one as `refs/heads/conflict/<device>/<n>`. A divergence already recorded is not recorded again.
4. Apply the local side. A ref checked out in a worktree moves only by a fast-forward of a clean worktree; otherwise it is reported as behind. A ref reported behind keeps the base it had until the remote's change is applied here: the remote's value is not taken as agreed, so a remote deletion of a branch checked out here is not mistaken on the next sync for a ref created here and pushed back.
5. If the manifest would not change, stop. Otherwise mark this writer active (`locks/writers/...`, checked against the collector's sweep lock), and upload the pack, the new chunks with an index listing them, and changed segments.
6. Swap the manifest: `put_if_absent` for a new task, else `put_if_match` on the generation read. When another writer moved first, wait (exponential backoff, full jitter) and start again at 1.

Objects are written before the manifest that names them. A reader, or a crash between them, sees the old state or the new, never half of one. A swap whose response was lost is recognized on the next read by the random commit ID it wrote. Last-writer-wins is never used.

## Leases

One runner per task attempt. The lease is an object, `leases/<task>/<attempt>`, holding the holder, its device, an epoch, and an expiry:

- **Acquire**: create it if absent; take it over if its expiry plus the allowed clock skew (30 seconds) has passed, conditional on the generation read, with the epoch one higher; renew it if this holder already has it. Anything else is refused with the holder's name and how long it has left.
- **Renew**: conditional on the generation this holder last wrote. A renewal that finds another generation means the lease was taken after it ran out: the holder has lost it and stops. `LeaseKeeper` renews from a thread every third of the lease.
- **Release**: delete it, conditional on the generation.

A held lease is registered in the service registry as `sync_lease` (with `task`, `attempt`, `remote` and `epoch`), so `by services` shows who runs what. Expiry compares clocks across machines; the skew allowance is the bound sync assumes.

## Encryption

`encrypt = "passphrase"` or a `kms://` URL seals everything before it leaves.

- **Tenant keys.** `keyring.json` holds key versions: a random 32-byte secret each, wrapped by the tenant's wrapper, and the names key sealed under the current version. Keys are derived from the secrets with HKDF-SHA256.
- **Wrappers.** `passphrase`: PBKDF2-HMAC-SHA256, 600,000 iterations (the OWASP 2023 figure), sealing with AES-256-GCM. The design names Argon2id, which is not in `Cargo.lock`; the wrapped form records its algorithm and parameters, so Argon2id can be added beside it and keys rewrapped. `kms://gcp/projects/P/locations/L/keyRings/R/cryptoKeys/K`: Cloud KMS `encrypt` and `decrypt`. `kms://aws/<key ID, alias/NAME or ARN>?region=R`: AWS KMS `Encrypt` and `Decrypt` (JSON 1.1, SigV4). `kms://azure/<vault host>/keys/<name>[/<version>]`: Key Vault `wrapkey` and `unwrapkey` with RSA-OAEP-256. A KMS is called once per process to unwrap the keyring, never per object.
- **Names.** Object names are BLAKE3 keyed hashes of their kind and content hash under the names key; task directories and lease names are keyed hashes of the ID. Dedup is within a tenant only.
- **Refusals.** A wrong passphrase or KMS key is refused when the keyring is opened, before anything else is read. A plain configuration against a sealed remote, or the reverse, is refused.

### Keys and rotation

`by sync rotate-key` adds a key version (its secret wrapped by `--to`'s wrapper, or the current one), rewraps every object's data key under it with `put_if_match` (the data and the names stay), then retires the versions no object uses. `--to passphrase` takes the new passphrase from `BRANCHYARD_SYNC_NEW_PASSPHRASE`; `--to kms://...` moves the keyring to a KMS key. Each step is conditional, so a rotation that stopped can run again; run it with the new wrapper configured once the keyring has moved to it.

## Integrity

Every object read is checked after decryption: a content object against its name, a manifest or lease against its tag (sealed) or checksum (plain), and a manifest against the task it should hold. A mismatch is refused, counted (`corrupt`), and never imported: `git index-pack` checks a pack again object by object before any ref moves. `by sync scrub` reads a deterministic sample (a seed), checks every manifest's objects are there, and repairs a corrupt chunk from a local chunk directory that holds a verified copy. Packs, indexes and segments are reported, not repaired.

## The replicator

- **Outbox.** `.branchyard/sync.db` in a repository (`<data_dir>/sync/<repo>.db` on a server): SQLite in WAL mode, 0600. It holds the queue (a task waiting to be pushed, its tries, when next due, the last error), what this machine knows of each task in each remote, resumable uploads' progress, recent outcomes, and counters summed over every process.
- **Scan and drain.** A scan queues every task whose refs moved since its last sync (a `for-each-ref`), and every tenth scan queues every task to bring remote changes in. A drain syncs the due tasks. A failure stays queued with its error and is tried again after an exponential backoff with full jitter (up to 15 minutes). Queuing a task twice keeps one row, so a burst of turns is one push, and lag is measured from the oldest change not yet pushed.
- **Bounds.** Every request is retried on a transient failure (a network error, 408, 429, 5xx) with exponential backoff and full jitter (6 tries, 200 ms base, 30 s cap); every retried request is idempotent (a content-addressed put, a conditional write, a read). Objects of one sync go up and down on at most `concurrency` threads. `bandwidth` is a token bucket over bytes in both directions.
- **Resumable uploads.** Above the part size (8 MiB), uploads go in parts: an S3 multipart upload completed with `If-None-Match: *`, a GCS resumable session, Azure staged blocks committed with `Put Block List`, or appended parts in the directory backend. The upload ID, session or staged count is kept in the outbox; a later try continues where the last stopped.
- **Where it runs.** `by sync` scans and drains once. `by serve` and `by worker` with `sync` run it every `interval` on a thread of their own.

## Garbage collection

`by sync gc`:

1. Takes `locks/sweep` (a lease object), then looks for live writer marks. If a sync is uploading, it releases the lock and stops, deferred. A writer checks for the sweep lock after marking itself, so a sweep and a writer never overlap.
2. Deletes the manifest of a task idle longer than its retention (`retention`, or the manifest's `retention_days`), unless the task is on legal hold.
3. Marks: everything a manifest references (packs, indexes, every chunk they list, segments) is live. Every other content object is a candidate, stamped with when it was first seen unreferenced and its generation.
4. Sweeps: a candidate seen unreferenced longer ago than the grace period, still unreferenced and at the same generation, is deleted conditionally. The grace period is longer than any upload, so an object uploaded for a manifest not yet swapped is never collected from under its writer.
5. Records the remaining bytes in `gc/usage`. A sync that would take the tenant over `quota` is refused before it uploads anything.

The quota check reads `gc/usage` while it is under an hour old and adds what this process has stored since that count (allowing for clock skew); an older count, or none, means listing the bucket. Within a tenth of the quota, the bucket is listed rather than trusting the count, so the uploads of other machines since the count are seen too. The quota is soft across machines: two machines admitted at the same moment can each fit under it and together pass it, by at most one sync each. One process never passes it by adding up uploads that each fit alone.

## Backends

One interface, `ObjectStore`: `get`, `get_range`, `stat`, `put_if_absent`, `put_if_match`, `list`, `delete_if_match`, `resumable_put`. Each backend passes the conformance suite (`branchyard_sync::store::conformance`): missing objects, create-only-if-absent, ranges, replace-only-the-generation-named, empty objects, sorted paged listing, conditional deletes, resumable uploads, and six threads racing to create and to swap one key (exactly one wins each).

| Backend | URL | Conditional writes | Auth | Tested against |
|---|---|---|---|---|
| A directory | `file:///path` | under an exclusive `flock` on `.lock`: check, then rename a flushed temporary file; the directory is flushed after | the file system | the real thing, every run |
| S3 and compatible | `s3://bucket/prefix` (`?endpoint=`, `region=`, `path_style=`, `profile=`, `part_size=`) | `If-None-Match: *`, `If-Match` on puts, multipart completion and deletes | SigV4 from the variables, `~/.aws/credentials`, a container's credentials endpoint, or IMDSv2 | the stand-in (SigV4 checked on every request); MinIO when `BRANCHYARD_SYNC_TEST_S3` is set |
| Google Cloud Storage | `gs://bucket/prefix` (`?endpoint=`, `part_size=`) | `ifGenerationMatch` (`0` for absent) | `GOOGLE_OAUTH_ACCESS_TOKEN`, a service-account key (an RS256 JWT exchanged at its `token_uri`), or the metadata server (workload identity); none with `STORAGE_EMULATOR_HOST` | the stand-in (bearer token checked); fake-gcs-server when `BRANCHYARD_SYNC_TEST_GCS` is set |
| Azure Blob | `az://account/container/prefix` (`?endpoint=`, `part_size=`) | `If-None-Match: *`, `If-Match` | Shared Key (`AZURE_STORAGE_KEY`), a SAS token (`AZURE_STORAGE_SAS_TOKEN`), or a managed identity | the stand-in (Shared Key checked on every request); Azurite when `BRANCHYARD_SYNC_TEST_AZURE` is set |
| A git remote | `git+https://…`, `git+ssh://…`, `git+file://…` | `git push --force-with-lease` | git's own | a local bare repository, every run |
| Memory | `mem://name` | a mutex | none | every run (tests and examples) |

The signers are written here and checked against published vectors: SigV4 against the AWS test suite's `get-vanilla` and four S3 developer-guide examples; the service-account JWT byte for byte against `openssl dgst -sha256 -sign` (RSASSA-PKCS1-v1_5 is deterministic); Shared Key against an independent implementation of the documented string-to-sign. No AWS, Google or Azure crate is in `Cargo.lock`.

### Honest limits, per backend

- **S3**: conditional deletes (`If-Match` on `DeleteObject`) need S3 since late 2024; older S3-compatible stores may ignore it, and the collector's deletes are then unconditional (still only of objects unreferenced for the grace period, under the sweep lock). Never run against AWS itself. No virtual-host bucket names with dots over TLS.
- **Cloud Storage**: the JSON API only (no XML API, no signed URLs). Never run against Google itself; the stand-in follows the documented semantics.
- **Azure Blob**: block blobs only. A managed identity and SAS tokens were tested against stand-ins only. Never run against Azure itself.
- **Git remote**: each object is a ref, `refs/by-sync/<key>`, to a commit holding it as one blob; `list` is `git ls-remote`, which reports no sizes, so usage and quotas do not apply; uploads are one push each (nothing to resume); every operation is a git process, so it is slow for many chunks. Fine for a few tasks, not for a tenant.
- **Directory**: writers take turns on one lock; on a network file system `flock` must work.
- **All**: one connection per request (no keep-alive); objects are read whole into memory, so a pack must fit in memory.

## Servers

A server or worker with `sync` in its configuration file:

```json
"sync": { "remote": "gs://my-bucket/branchyard", "encrypt": "kms://gcp/...", "interval": "30s", "lease_seconds": 60 }
```

- Before an operation on existing branches, it pulls each branch's task and takes the task's lease (attempt `run`), renewed while the operation runs and registered in the repository's service registry. Another runner's lease fails the operation with `409 sync_lease_held`, naming the holder; a remote that cannot grant one fails it with `503 sync_unavailable`.
- A replicator per repository pushes changed refs every `interval`, so checkpoints reach the remote as turns end. When an operation ends, its branches are queued and the replicator woken; then the leases are released. At shutdown the replicators stop and drain once more.
- `/metrics` adds `branchyard_sync_*` ([observability](observability.md#metrics)).

The keys are those of `[sync]` plus `lease_seconds` (default 60), in `schema/server.config.json`.

## Task repositories

Tasks with a repository of their own (folder tasks and tasks with no files, under `$BRANCHYARD_HOME/tasks/`) sync through `branchyard_sync::tasks::TaskSource`, which implements `SyncSource` for a `TaskRepo`, and `HomeTasks`, which lists them. `by sync` uses both beside the repository's branches (`Providers`), so one `by sync` carries the branches and every such task; a task's ID is its ULID, and `by sync TASK` takes it or a unique prefix. The mapping:

| Method | A task repository returns |
|---|---|
| `task_id` | the task's ID (its ULID) |
| `git_dir` | `~/.branchyard/tasks/<id>/git` for a folder task, the repository's git directory for a code task |
| `refs` | `refs/heads/*` and `refs/branchyard/*` as they are (for a code task, the task's own branches only) |
| `local_ref` | the identity (`refs/heads/x` is `refs/heads/x`) |
| `chunk_dir` | `~/.branchyard/chunks` |
| `reachable_chunks(commit)` | its `reachable_chunks` API |
| `segments`, `segment_dir` | closed conversation segments not yet committed, and where pulled ones go |
| `ledger_watermark` | the effect ledger's last entry synced |

and a `SourceProvider` listing its tasks, so the replicator, `by sync` and servers work unchanged. The chunk directory layout sync reads and writes is `<aa>/<blake3 hex>`, each file checked against its name.

Not built yet: pulling a task's own repository onto a machine that never had it (`by sync pull` needs the task there), and a server's replicator syncs its repositories' branches, not tasks under its home.

## Observability

`by sync status` and `/metrics` report bytes and objects each way, swaps, swap conflicts (a swap that lost and was retried), divergences (conflict branches recorded), retries, objects refused as corrupt, failed syncs, queued tasks and lag. See [observability](observability.md).

## Tests

- `crates/branchyard-sync/tests/conformance.rs` (11): the suite on the directory, memory, the S3, GCS and Azure stand-ins and a git remote; MinIO, fake-gcs-server and Azurite when configured, skipped with a printed reason otherwise; resumable uploads stopped by an injected failure and continued without resending finished parts on every backend; wrong signatures and tokens refused; a service-account JWT exchanged at a token endpoint that checks its RS256 signature, the metadata server, IMDSv2 credentials signing a real request, a managed identity; the three KMS clients wrapping and unwrapping through their stand-in.
- `crates/branchyard-sync/tests/sync.rs` (17, manual clocks): a round trip between two machines through a directory and through the S3 stand-in; two writers racing on one generation (exactly one swap wins, the other merges and records a conflict branch; never last-writer-wins; nothing recorded twice) and two racing on different refs (both land); a crash between objects and manifest (the remote reads at the old state, the push stays in the outbox with a backoff, and completes after a restart reusing the uploaded pack); a corrupted pack refused before any ref moves and found by scrub; chunks and segments synced, a corrupted chunk repaired by scrub; an encrypted bucket holding no plaintext, task names, commit or blob IDs or content hashes, a wrong passphrase and a plain configuration refused, and a rotation to a new passphrase; a KMS-wrapped remote calling the KMS once each way; leases exclusive, renewed, taken over after expiry and skew with a new epoch, lost by the old holder, registered while kept; collection with the grace period (an object a later manifest references is never collected), a deferred sweep, retention, a legal hold; a quota, counting what this process wrote since the collector's count and listing the bucket near the limit; concurrency and bandwidth bounds; incremental packs (3 objects for a one-file change); a checked-out ref; a remote deletion of a checked-out ref left here and not pushed back; transient failures retried.
- Unit tests in the crate: SigV4, JWT and Shared Key vectors, dates, envelope and keyring, rotation, merge rules, retry and budget arithmetic, the outbox, configuration parsing.
- `crates/branchyard-cli/tests/sync.rs` (2, the built `by`): `by sync`, `status`, `ls`, `pull` into a clone, `hold`, `rm`, `gc`, `scrub` through `file://`; refusals with `--remote` and without `[sync]`; an encrypted remote with no branch name or text in the bucket and a wrong passphrase refused.
- `crates/branchyard-server/tests/sync.rs` (1): a task pushed after it runs; a `send` refused while another holder has the lease; another machine's commit pulled before the next turn, which runs on top of it; the result pushed; the sync series in `/metrics`.

## What is not done

- Tasks are today's branches: a branch pulled to a machine that never had it arrives as git refs, without the Branchyard record (its events, sessions and turns stay where they ran). Task repositories carry the conversation in the repository and close this.
- Packs are never consolidated; a task with thousands of syncs has thousands of packs.
- Chunks are pulled eagerly, not on demand.
- Scrub does not rebuild a corrupt pack; the operator drops it by hand.
- No real cloud service, emulator or KMS has been used; see the limits above.
- The lease is per task attempt; a server takes one per branch it runs (attempt `run`).
