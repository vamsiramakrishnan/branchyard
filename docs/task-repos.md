# Task repositories and sync

> **Status.** Design, 3 October 2026, for Wave 6. This page is the contract the task-repository and sync work build to. **Task repositories are built** (3 October 2026, branch `agent/taskrepo-16`): tasks, the record of each checkpoint, folder tasks, tasks with no files, large files in a chunk store, `by task` locally, `by --remote task ls` and `show`, and the API sync reads. Tested hermetically against the fake ACP agent; no real harness. Sync, from "Sync: principles" on, is not built here.

Every task Branchyard runs is a git repository: code or not, in a repository or in a folder you granted. Each turn is a commit, each attempt a branch, and accepting one is a merge. The conversation is committed beside the files, so rewinding a task rewinds both. What the task did outside the machine is in the [effect ledger](effects.md), which says plainly what can be undone there and what cannot.

Repositories live on your machine first and sync to durable storage you choose (Google Cloud Storage, S3 or anything S3-compatible such as R2 and MinIO, Azure Blob, a git remote, or a directory), so a task survives the machine, follows you to another, and can be run by a server.

## A task

```
task: board-update (01J9...)
  refs/heads/main                 the accepted state (a task's own repository)
  refs/heads/by/<attempt>         attempts (fan, map items, retries, forks)
  refs/branchyard/<attempt>/<incarnation>/turn-<N>     checkpoints (as before)
  refs/branchyard/<attempt>/<incarnation>/record-<N>   the record of checkpoint N
  files/...                       the work: your folder's files, or a repository's
  .task/task.toml                 what was asked, by whom, policy, grants
  .task/conversation/<turn>.jsonl prompts, answers, approvals, events of each turn
  .task/effects.jsonl             the task's ledger entries, as of each commit
```

A task is a [`Task`](../crates/branchyard/src/tasks/mod.rs): an ID (a ULID), a title (the first line of what was asked), what was asked, who asked (the gateway actor, else git's `user.name <user.email>`, else the login name), the policy and grants in one line, when, what its files are, and what started it. It owns attempts, which are branches:

| Started by | Task |
|---|---|
| `by run`, `by task new` | a new task with one attempt |
| `by fan`, a routed run | one task, an attempt per branch |
| `by map` | one task for the map (kept across resumes), an attempt per item |
| `by fork`, `by fork --at`, `by reincarnate`, `by task fork` | another attempt of the parent's task |
| a delegated child, an adopted worktree | none: part of another branch's work |

Tasks are recorded in the yard's state: `.branchyard/tasks/<id>/task.json` and one marker per attempt, with `.branchyard/task-attempts/<attempt>` naming the task of each. Removing a branch leaves it listed as removed.

**The record of a checkpoint.** After each checkpoint, the engine writes a record commit beside it, `refs/branchyard/<attempt>/<incarnation>/record-<N>`. Its tree is checkpoint N's files with `.task/` at the top; its parents are the record it continues and the checkpoint. `.task/task.toml` (`format = "branchyard-task/1"`) has the task's fields and the attempt's name and harness. `.task/conversation/<n>.jsonl` has one file per turn: a first line with the turn, the attempt, the prompt, the answer and the approvals, then every event of the turn as recorded. `.task/effects.jsonl` is the ledger's entries for the attempt: [`tasks::effects_snapshot`](../crates/branchyard/src/tasks/mod.rs) is the extension point the effect ledger fills; until then the file is empty.

The record continues the lineage, as checkpoints do. A rewind to checkpoint N puts the attempt's files at N and its record at N's: the conversation is turns 1 to N. The next turn, numbered after the highest (as checkpoints are), continues N's record, so after rewinding from 3 to 1 the next record has `1.jsonl` and `4.jsonl`. A fork starts from its parent's record at the fork point and numbers its turns after the parent's.

- **A repository you already have** keeps working as before: each attempt is a branch of it. The record is never on the attempt's branch, so its candidate, `by merge`, `by pr`, `by try`, `by diff`, compare and every diffstat are exactly what they were, and `.task/` is never merged or pushed. A repository that tracks a `.task/` of its own keeps it; only the record commits replace it.
- **A folder you grant** (`by task new --folder PATH`) gets a repository of its own in `$BRANCHYARD_HOME/tasks/<id>/` (default `~/.branchyard`): `git/` is the git directory; the folder is its work tree only when Branchyard reads it, through `GIT_DIR`, `GIT_WORK_TREE` and an index of its own in `git/` (`branchyard-folder-index`), so nothing is written into the folder. `work/` is the yard's root: a work tree of `git/` that holds Branchyard's state and checks nothing out (its HEAD is detached at `main`). The first commit on `main` is the folder as it was, and `refs/branchyard/folder` says which commit the folder is at. Ignore rules are the folder's own `.gitignore` files, then `git/info/exclude`, which Branchyard writes: its state, defaults (OS files such as `.DS_Store` and `Thumbs.db`, editor swap files, `node_modules/`, `__pycache__/`, `.cache/`, `.venv/` and other caches) and the folder's `.branchyardignore`.
- **A task with no files** (`by task new --no-files`) has the same kind of repository, starting from an empty tree: its conversation and the files its attempts write are its results.
- **Large and binary files** in a task's own repository are not stored in git history: see below.

### Attempts on a folder, and accepting one

Attempts never touch your folder. Each runs in its own worktree of the task's repository (`work/.branchyard/worktrees/<attempt>`), from `main`, like any branch. To accept one (`by task accept`), Branchyard:

1. Takes the task's lock (`accept/lock`) and the attempt's lease (refused while a turn runs).
2. Finds what `main` becomes: the attempt's record at the checkpoint it is at (its files and its conversation), as a fast-forward, or a merge of it made without a work tree (`git merge-tree`) when another attempt was accepted since. A conflict refuses it.
3. Plans the change from the folder's commit to the new `main`, leaving `.task/` and submodules out, and checks every path it touches in the folder: it must hold exactly what the folder's commit has (by blob hash, or for a large file by its BLAKE3 hash), or already what the new `main` has. A file changed, created or removed in the folder since the task began, or a file where a folder is needed, refuses the accept, naming each, and **nothing is written**. Put the file back, or move your change aside, and accept again.
4. Journals the plan (`accept/journal.json`), then writes it: removals first (and folders they emptied), then each file through a temporary name beside it, renamed over it, with its permission bits; links as links; large files from the chunk store, verified.
5. Moves `main` (compare-and-swap) and `refs/branchyard/folder` to the new commit, records the attempt `merged` into `main`, and removes the journal.

A process stopped after step 4 began leaves the journal; the next accept finishes it first (writing a file that already holds what it should is nothing, so repeating is safe). Files you changed elsewhere in the folder are never read or written. In a repository you already have, `by task accept` is `by merge` of the attempt (into `--into`, or the current branch); in a task with no files it moves `main` as above, with nothing to write.

### Large files

In a task's own repository, a file of at least the threshold (`branchyard.largeFileThreshold` in its git config, set by `by task new --large-threshold BYTES`, default 1 MiB) is cut into content-defined chunks (a gear rolling hash with normalized chunking, FastCDC-style: at least 16 KiB, at most 4 MiB, about 256 KiB on average; the gear table is fixed, so the same bytes are always cut the same way), each stored once in the chunk store at `<store>/<aa>/<blake3>` (`$BRANCHYARD_HOME/chunks`, shared by every task; `branchyard.chunks` names it) through a temporary name, and verified against its name on every read. Git stores a pointer where the file's bytes would be:

```
branchyard-chunked/1
size 3145728
blake3 <64 hex: the whole file>
chunk <64 hex> 262144
chunk <64 hex> 2883584
```

Only these lines, each ending in a newline; the chunk sizes add up to `size`, and anything else is not a pointer. Branchyard applies pointers itself, not through git filters or git-lfs: before each snapshot it stores every large file and stages its pointer in the index, marked `skip-worktree` so `git add` and `git status` leave the real file alone; after a worktree is created, and after a rewind's reset, it writes each pointer's file from the store (reassembled, checked against the pointer's hash, renamed into place) unless the file there already is it. A rewind's check for changes no checkpoint has includes large files, which `git status` cannot see. Accepting writes large files into the folder from the store. Large files in a repository you already have are left to that repository (its own git-lfs, if any).

## The API for sync

`branchyard::tasks` gives sync what it needs, without opening a branch:

| Call | Gives |
|---|---|
| `tasks::home()` | `$BRANCHYARD_HOME`, or `~/.branchyard` |
| `TaskRepo::list(home)`, `TaskRepo::open(home, key)` | every task with a repository of its own, or one by ID, ID prefix or attempt |
| `TaskRepo::of(&yard, key)` | a task in a repository you already have |
| `TaskRepo { id, files, git_dir, own, chunks, attempts }` | its git directory (a task's own, or the repository's common one, shared), whether it is the task's alone, its chunk store, its attempts |
| `TaskRepo::refs()` | the task's refs and commits: every ref of a repository of its own; in a shared repository, each attempt's `refs/heads/by/<attempt>` and `refs/branchyard/<attempt>/...` (checkpoints and records) |
| `TaskRepo::reachable_chunks(rev)`, `tasks::reachable_chunks(git_dir, rev)` | the chunk hashes named by a pointer anywhere in the history of `rev` |
| `ChunkStore::new(dir)`, `ChunkStore::at_home()`, `path`, `contains`, `put`, `get`, `store_file`, `restore_file` | the chunk store: put is idempotent, get verifies |
| `Pointer::parse`, `Pointer::render` | the pointer format |
| `tasks::home_tasks(home)`, `tasks::list(&yard)`, `tasks::view(&yard, key)` | `TaskView`s: the task, its attempts (status, checkpoint, record, conversation) and, for a task's own repository, `accepted` (`main`) and `folder_at` |

A task's own repository is a plain git directory: `git pack-objects` over `refs()` and the chunks `reachable_chunks` names are everything needed to restore it elsewhere.

### Not done yet, and limits

- **Sync itself** (everything below) is the sync track's.
- **Attempts on a folder see the folder as it was when the task began**, or as the last accept left it. Changes you make in the folder meanwhile are kept, and refuse an accept only where an attempt changed the same path.
- **`by --remote task`** lists and shows a server's tasks (`GET /v1/repos/{repo}/task-records` and `/task-records/{task}`, since `POST .../tasks` already starts a run); `new`, `rewind`, `fork`, `accept`, `open` and `rm` are refused with a message. A server's runs are tasks like any other, and accepting one is `by --remote merge`.
- **The record of a turn is written after its checkpoint**, best-effort: a failure is a warning on the branch, never the turn's failure, and a turn whose checkpoint was replayed by recovery gets no record. Checks run by `by merge` or `compare --check` in a task's own repository see pointers, not large files.
- **Large files** are chunked only in a task's own repository. Candidate line counts there count a pointer's lines. There is no garbage collection of the chunk store yet (sync's mark and sweep is designed below).
- **One copy on disk per attempt**: each attempt's worktree holds the folder's files (large ones restored from the store).

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
- `by sync [TASK]`, `by sync status`, `by sync pull TASK`, `by sync gc`.
- `by task new|ls|show|open|rewind|fork|accept|rm`, and the same over HTTP and in the UI.
- A server with `[sync]` pulls a task it is asked to run, runs it under the lease, and pushes as it goes; a phone or another machine sees it.

## What this does not do

It does not make a bucket a collaboration tool for simultaneous editing (divergence becomes branches), sync files outside tasks, or undo effects in the world (that is the [effect ledger](effects.md)'s job, within what upstreams allow).
