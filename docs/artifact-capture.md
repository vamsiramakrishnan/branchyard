# Capture result artifacts after a turn

Written 29 October 2026.

A required deliverable result can name an output manifest under its producer's
worktree. Branchyard collects the declared files after the harness finishes and
its provider returns the work, before accepting native final JSON. Collection
publishes ordinary immutable artifacts as that producer. The final JSON stays
in the caller's schema.

For example, a result contract may request a report alongside an answer:

```json
{
  "schema": {
    "type": "object",
    "required": ["answer"],
    "additionalProperties": false,
    "properties": {"answer": {"type": "string"}}
  },
  "required": true,
  "purpose": "deliverable",
  "capture": {
    "manifest": "outputs/artifacts.json",
    "max_files": 4,
    "max_bytes": 10485760
  }
}
```

`manifest` is relative to the producer's worktree. The optional limits bound
file count and total contents; omitting them selects `ResultCapture::new`'s
defaults. Invalid paths, zero limits and a capture on an optional or report-only
contract are refused before starting the task. The manifest itself has a
separate fixed size bound.

Use the common result flags on a task command:

```sh
by run 'Create the report and return the answer as final JSON' \
  --result-schema-file answer-schema.json --result-deliverable \
  --result-artifacts outputs/artifacts.json \
  --result-artifacts-max-files 4 --result-artifacts-max-bytes 10485760
```

`--result-artifacts` requires `--result-deliverable` and a schema. Its limit
flags require the manifest flag. A deliverable requires a result and can
complete without a Git diff. Capture does not add a planning session, child,
model call or automatic retry.

A managed lead may finish its own intermediate turn while its children run.
When the existing wake policy parks it in `WaitingOnChildren`, neither native
result acceptance nor artifact collection runs. The lead writes its final JSON
and outbox on a later wake after its children settle. This uses the rig's
existing delegation grant and wake limits.

The Python in-harness SDK accepts the same `capture` object inside `result`.
The HTTP SDK and rig contract carry that same object. In Rust, set
`ResultContract.capture` to `Some(ResultCapture::new("outputs/artifacts.json"))`
on a required deliverable, and call `check()` after setting explicit limits.
No upstream credential is needed to collect local files.

## Write the producer's outbox

The file named by `capture.manifest` contains a versioned inventory:

```json
{
  "schema_version": 1,
  "files": [
    {
      "path": "brief.pdf",
      "name": "Evidence brief",
      "media_type": "application/pdf",
      "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    }
  ]
}
```

Replace the example hash with the file's actual SHA-256. Each `path` is relative
to the manifest's own directory. `name` is optional. The producer can use the
[artifact toolkit](artifact-toolkit.md) to hash explicitly selected outputs:

```sh
python3 -m branchyard_artifacts manifest outputs/brief.pdf --out outputs/artifacts.json
```

The manifest cannot choose a publisher, branch incarnation, turn, grant or
executable. Unknown fields are errors. Branchyard rejects absolute paths,
parent traversal, symlinks at any path component, directories, FIFOs and other
special files, duplicate paths, self-capture and invalid or mismatched hashes.
It opens directories and files through pinned descriptors with `NOFOLLOW`,
then copies and hashes bounded contents from those same opened files. A path
replacement cannot redirect a later reopen outside the selected directory.

An outbox may be ignored by Git in a local or shared-mount task, so useful
files can complete a no-change deliverable. Copied-provider work currently
returns through the provider's ordinary worktree transfer. Ignored copied
outboxes need the scoped transfer integration and are not qualified by this
unit. A copied worktree transfer failure prevents capture rather than accepting
an older host outbox.

## Publication and retries

All publication rows and the completed `capture_artifacts` journal receipt
commit in one existing storage transaction. The transaction checks the live
producer's lease, branch incarnation, engine turn and cancellation state. It
then inserts ordinary artifact IDs and stores their references as the receipt
outcome. SQLite and PostgreSQL follow the same contract; no capture table is
added.

A retry under the same producer incarnation and engine turn with the same
manifest digest returns the original artifact references. It does not create
duplicate publications or require reopening the captured output files. A
different manifest digest is refused. A stale, canceled, removed or replaced
producer cannot publish, including when it presents an earlier receipt.
Publication rows and receipt roll back together on any insertion error.
Content-addressed blob staging can leave unreferenced immutable blobs after a
failed metadata transaction, as an ordinary failed publication can; it leaves
no partially accepted inventory.

Read captured files through `by artifact list/get`, the SDK's ordinary artifact
methods, or existing handoffs. References carry producer provenance and labels
for `capture.manifest`, `capture.manifest_sha256`, `capture.path` and
`capture.engine_turn`. The artifact's `turn` keeps the producer's submitted-turn
count; the journal and engine-turn label identify the engine invocation. Artifact
digests use blake3. The manifest's SHA-256 is a separate input-integrity check.
Sibling reads still need explicit shares or a handoff. A manifest grants no
additional access.

Collection failure prevents native-result acceptance and fails the task. A
previous explicit result remains an event-log claim, but a failed task does not
complete its required deliverable. Local recovery can finish collection after
a recorded successful turn. Sandbox recovery without a completed capture
receipt fails closed until provider-transfer evidence is available.

Status: hermetic fake-harness tests and storage conformance verify collection,
bounded descriptor reads, receipt idempotence and rejection of invalid producer
authority. Shared-mount fake sandbox tests exercise the post-harness boundary.
Not done: real provider qualification, ignored copied-provider outbox transfer,
cloud services or model runs.
