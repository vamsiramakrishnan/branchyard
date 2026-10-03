# Task repositories and sync

> **Status.** Design, 3 October 2026, for Wave 6. This page is the contract the task-repository and sync work build to. **Sync is built** (3 October 2026, branch `agent/sync-16`) for today's branches and any `SyncSource`, tested hermetically: see [sync](sync.md) for the layout in the bucket, the protocol, and what each backend was and was not tested against.

Every task Branchyard runs is a git repository: code or not, in a repository or in a folder you granted. Each turn is a commit, each attempt a branch, and accepting one is a merge. The conversation is committed beside the files, so rewinding a task rewinds both. What the task did outside the machine is in the [effect ledger](effects.md), which says plainly what can be undone there and what cannot.

Repositories live on your machine first and sync to durable storage you choose (Google Cloud Storage, S3 or anything S3-compatible such as R2 and MinIO, Azure Blob, a git remote, or a directory), so a task survives the machine, follows you to another, and can be run by a server.

## A task

```
task: board-update (01J9...)
  refs/heads/main                 the accepted state
  refs/heads/attempt/<n>          attempts (fan, map items, retries)
  refs/branchyard/<attempt>/turn-<N>   checkpoints (as today)
  files/...                       the work: your folder's files, or a repository's
  .task/task.toml                 what was asked, by whom, policy, grants
  .task/conversation/<turn>.jsonl prompts, answers, approvals, events of each turn
  .task/effects.jsonl             the task's ledger entries, as of each commit
```

- **A repository you already have** keeps working as now: the task is a branch of it, `.task/` lives on the task's branch only, and merging a candidate leaves `.task/` out.
- **A folder you grant** gets a git directory outside the folder (`~/.branchyard/tasks/<id>/git`, `GIT_DIR` and `GIT_WORK_TREE` split), so nothing is written into it but the files the task changes. Ignore rules come from the folder's `.gitignore`, a `.branchyardignore`, and defaults (caches, `node_modules`, OS files).
- **A task with no files** (research, a message to draft) still has a repository: its conversation and results are the files.
- **Large and binary files** (decks, spreadsheets, images, datasets) are not stored in git history. They are cut into content-defined chunks (FastCDC-style, 16 KiB to 4 MiB, blake3-addressed); git stores a small pointer. Identical chunks are stored once, across every task of the tenant.

Undo of the task's own state is `by rewind` (exact). Undo outside is the effect ledger's (honest).

## Sync: principles

Built the way large storage systems are, and small enough to run on a laptop:

1. **Local first.** The local repository is the working copy; every command works offline. Sync is a background replicator with a durable outbox, never on the hot path of a turn.
2. **Immutable, content-addressed objects; one mutable pointer.** Everything uploaded is named by its hash and never changes: git packfiles (`packs/<blake3>.pack` and `.idx`, produced by `git pack-objects`), chunks (`chunks/<aa>/<blake3>`), and conversation segments. The only mutable object per task is its manifest (`tasks/<id>/manifest.json`: refs → commits, packs listed, chunk roots, ledger watermark).
3. **Atomic commits by compare-and-swap.** Objects are written first, then the manifest is replaced with a conditional write: GCS `ifGenerationMatch`, S3 `If-Match` / `If-None-Match`, Azure `If-Match` ETags, a git remote's `--force-with-lease`, a directory's rename with a lock. A reader sees the old state or the new one, never half of one. A failed swap means another writer moved first: fetch, merge, try again.
4. **Divergence becomes branches, never a lost write.** Two machines that changed the same task produce two heads; the second sync records `refs/heads/conflict/<device>/<n>` instead of overwriting, and the person (or a policy for fast-forwards) chooses. Last-writer-wins is never used.
5. **One runner at a time.** A turn takes a lease on its task in the remote (a lease object written with a precondition and an expiry, renewed while it runs), so two machines never run the same attempt at once. Leases appear in the [service registry](registry.md) as well.
6. **Encrypted before it leaves.** Client-side envelope encryption: each object is sealed with AES-256-GCM or ChaCha20-Poly1305 (`ring`) under a data key; data keys are wrapped by a tenant key from a KMS (GCP KMS, AWS KMS, Azure Key Vault) or a passphrase-derived key (Argon2id) locally. Object names that would reveal content are keyed hashes (HMAC-blake3 with a tenant key), so the bucket learns nothing it does not need. Dedup is within a tenant only.
7. **Verified on every read.** Each object is checked against its name's hash after decryption; a mismatch is refused and repaired from another copy if one exists. A periodic scrub reads samples.
8. **Idempotent, resumable, bounded.** Uploads are resumable (GCS resumable sessions, S3 multipart); every request is retried with exponential backoff and full jitter, under a concurrency limit and a bandwidth budget; a retried put of a content-addressed object is harmless by construction.
9. **Garbage collected safely.** Unreferenced objects are collected by mark and sweep from all manifests, with a grace period longer than any upload can take, in two phases (mark as candidate, delete after the grace period if still unreferenced), so a slow writer is never collected from under itself.
10. **Retention and holds.** Per-task retention, legal holds that block deletion, and a tenant quota; deleting a task deletes its manifest and lets GC reclaim what nothing else shares.
11. **Observable.** Bytes up and down, objects, swap conflicts, retries, lag behind remote per task, in `/metrics` and `by sync status`.

## Sync: what is built

Every principle above is built in the crate `branchyard-sync` and described in [sync](sync.md):

- 1, local first: a SQLite outbox per repository (`.branchyard/sync.db`), a replicator in `by serve` and `by worker`, and `by sync`.
- 2 and 3: packs of only what the remote lacks, chunks, chunk indexes and segments under content-derived names; one manifest per task swapped with `put_if_absent` or `put_if_match` on each backend's generation.
- 4: a ref-by-ref merge; divergence recorded as `refs/heads/conflict/<device>/<n>`, once.
- 5: lease objects with an epoch, renewed by a keeper thread, registered as `sync_lease` services; a server runs a branch only under its lease.
- 6: a data key per object (AES-256-GCM or ChaCha20-Poly1305), wrapped by a tenant key from a passphrase (PBKDF2-HMAC-SHA256: Argon2id is not in `Cargo.lock`) or Cloud KMS, AWS KMS or Key Vault; keyed BLAKE3 names; rotation by rewrapping.
- 7: every read checked against its name; `by sync scrub` samples and repairs chunks from a local copy.
- 8: resumable uploads (S3 multipart, GCS sessions, Azure blocks, directory parts), retries with full jitter, a concurrency bound and a bandwidth budget.
- 9 and 10: two-phase mark and sweep with a grace period, a sweep lock that waits for active writers, retention, legal holds and a quota.
- 11: `by sync status` and `branchyard_sync_*` in `/metrics`.

Limits, honestly: no test has reached a real cloud service or emulator (in-process stand-ins implement each API's conditional semantics and check its signatures; the conformance test runs against MinIO, fake-gcs-server and Azurite when configured). The git backend stores each object as a ref and has no sizes, so quotas do not apply to it. Tasks are today's branches until the task-repository work implements `SyncSource` ([how](sync.md#task-repositories)).

## Backends

One `ObjectStore` interface: `get`, `get_range`, `put_if_absent`, `put_if_match(generation)`, `list(prefix)`, `delete_if_match`, `resumable_put`. Backends:

| Backend | URL | Conditional write | Auth |
|---|---|---|---|
| Google Cloud Storage | `gs://bucket/prefix` | `ifGenerationMatch` | the metadata server, a service-account key (RS256 JWT), or workload identity |
| S3 and compatible (R2, MinIO, Ceph) | `s3://bucket/prefix` (`?endpoint=`) | `If-None-Match: *`, `If-Match` | SigV4 from the usual variables, profiles or instance roles |
| Azure Blob | `az://account/container/prefix` | `If-Match`, `If-None-Match` | shared key or a managed identity |
| A directory | `file:///path` | rename under a lock file | the file system |
| A git remote | `git+https://…`, `git+ssh://…` | `--force-with-lease` | git's own |

The directory backend is the reference implementation and runs the conformance suite in every test run; cloud backends run it against local emulators where they exist (fake-gcs-server, MinIO) and against the real service only in an opt-in qualification job.

## Surfaces

- `[sync] remote = "gs://my-bucket/branchyard"`, `encrypt = "kms://…" | "passphrase"`, `interval`, `bandwidth`.
- `by sync [TASK]`, `by sync status`, `by sync pull TASK`, `by sync gc`; built, with `ls`, `scrub`, `hold`, `rm` and `rotate-key` ([sync](sync.md)).
- `[sync]` also takes `concurrency`, `retention`, `grace`, `quota`, `algorithm`, `passphrase_file` and `device`, in the user file only; a server's configuration takes the same as `sync`, with `lease_seconds`.
- `by task new|ls|show|open|rewind|fork|accept|rm`, and the same over HTTP and in the UI.
- A server with `[sync]` pulls a task it is asked to run, runs it under the lease, and pushes as it goes; a phone or another machine sees it.

## What this does not do

It does not make a bucket a collaboration tool for simultaneous editing (divergence becomes branches), sync files outside tasks, or undo effects in the world (that is the [effect ledger](effects.md)'s job, within what upstreams allow).
