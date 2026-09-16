# Control API v1alpha1

Status: Rust protocol, HTTP client and PostgreSQL-backed admission server are
implemented. [Generated schemas](../schema/contract.json) describe wire shape;
[server deployment](server.md) describes the implemented boundary. Execution and
worker-side guarantees below remain acceptance criteria until a worker is shipped.

## Requests and identity

All endpoints require a bearer credential. Tenant and principal identity come
from verified authentication, never a caller-supplied `tenant_id`. All JSON
messages include `schema: "branchyard/v1alpha1"`.

| Method | Path beneath the endpoint prefix | SDK method |
|---|---|---|
| GET | `/v1alpha1/info` | `info` |
| POST | `/v1alpha1/commands` | `submit` |
| GET | `/v1alpha1/operations/{operation_id}` | `operation`, `reconcile` |
| GET | `/v1alpha1/tasks/{task_id}` | `task` |
| GET | `/v1alpha1/tasks/{task_id}/events?after=N&limit=N` | `events` |

A command includes a caller-generated UUID operation ID. Persist the request
before submission. The same ID appears in `Idempotency-Key`; the SHA-256 of its
normalized bytes appears in `X-Branchyard-Request-Sha256`. The server must verify
both headers against the body. Request headers are not signatures or authority.

Normalization parses the typed command, serializes all object keys in sorted
order, uses compact UTF-8 JSON and integer numbers, and normalizes typed UUIDs
and capability sets. The Rust `Command::canonical_bytes` is the current reference
algorithm, independent of Serde JSON's `preserve_order` feature. This is not an
arbitrary-JSON canonicalization standard. Persisting and replaying the same wire
bytes avoids cross-language serializer differences; new language SDKs need golden
fixtures for Unicode, UUID normalization and unordered capability inputs.

The server binds `(authenticated tenant, operation_id)` to the fingerprint and
original result. Same ID + same input returns the original receipt with `replay`;
same ID + different input returns 409 without dispatch. Task IDs cannot be reused
to create another task. A receipt binds its operation ID and fingerprint and means
**durable admission**, before remote allocation or model completion.

Operation records and tombstones must outlive the documented retry horizon. An
expired identity must be rejected, not silently treated as new. The current server
retains operation, task and event identities indefinitely; no deletion/TTL endpoint
exists. Replay is restricted to the original principal subject and current task
scope. Automatic SDK retries remain disabled.

## Commands

`create_task` supplies a task ID and a spec containing registered harness,
environment and policy profiles, immutable workspace source, component bindings,
required capabilities and resource ceilings. Root budgets belong to policy.
Per-child limits are requests within the root's remaining envelope, not new money
or permission. `cpu_millis` is millicores, not accumulated CPU time.

`apply_graph` supplies the ownership root ID, expected graph revision and 1–64
edits: spawn a child, add a dependency, or remove a dependency. Ownership and
execution dependencies remain distinct. The server validates the proposed final
graph, including references to children created in the same delta, independent
of edit order. It rejects unknown parents, cycles, cross-root references,
authority escalation, excessive fan-out/depth and unsatisfied capabilities.

Under one transaction, it checks the revision, reserves the root's resources,
updates graph state and writes the operation and durable dispatch command. Any
failure leaves all of these unchanged. This contract expresses dynamic proposals;
it does not store a predetermined workflow template. Runtime placement is separate.

`cancel_task` checks the selected task's revision and records a cancellation
request; `cascade` is explicit. Admission does not prove that a running process
has stopped or shared resources have been revoked. Execution must reconcile and
fence before marking the task cancelled and releasing exclusive bindings.

Local validation rejects nil identities, malformed profiles, floating source
refs, zero execution ceilings, duplicate components/children/edge edits, oversized
goals, excessive requirements and self dependencies. It does not inspect server
state, validate the full DAG or enforce principal authority. Servers must repeat
all validation; the SDK is not a security boundary.

## Uncertainty and errors

The client never retries or follows a redirect automatically. Only a receipt with
a matching identity and fingerprint is accepted. A timeout, disconnect, server
5xx, unexpected status, malformed/oversized receipt, or wrong receipt identity
returns `SubmissionUnknown`, including the saved operation ID and fingerprint.

This protocol reserves HTTP 400/401/403/404/409/413/422/429 on submission for
**definitive non-admission** of that request. Ingress and server implementations
must preserve this distinction and must not emit these statuses after committing
admission. HTTP 409 covers stale revision and operation/task identity conflicts;
inspect state before creating a new proposal. Server response prose is not
surfaced by the client, avoiding accidental leakage of tokens or provider output.

`reconcile(command)` looks up the operation and checks the input fingerprint.
A GET 404 means not visible now; a delayed submission may still commit. It is
not permission to issue the same side effect with a new ID. Resubmission, when
appropriate under the server retention contract, uses the identical saved command.
An operation in `unknown` is distinct from an unknown HTTP submission: the former
is a persisted uncertain execution effect. Its worker must reconcile before a
new effect can be issued. A succeeded graph operation does not mean its tasks
finished; a completed task does not mean code was accepted or promoted.

## Reads and bounds

Commands are at most 256 KiB; successful response bodies at most 1 MiB. A client
checks body chunks as they arrive, including when Content-Length is absent.
Deadlines cover connection and body reading. Credentials are never placed in a URL.

`info` advertises execution readiness, admitted operation names and registered
harness profiles with qualification/capability data. Profile configuration may
exist while `execution_ready` is false. Environment/policy/repository IDs currently
come from operator configuration, not a new discovery API.

Events have monotonically increasing per-task sequence IDs and bounded summaries
with artifact IDs. Pages contain 1–100 requested entries at most; an empty page is
valid. `after` is exclusive. `next_after` equals the last delivered sequence or the
input cursor for an empty page. Persist it only after consumption. Retention gaps
must cause an explicit non-success response, never a fabricated advanced cursor.
Consumers may repeat a page and deduplicate by sequence after a crash.

Polling is the current client surface. SSE/streaming subscriptions and bounded
artifact reads are later additive surfaces with their own reconnection and access
contracts; neither full logs nor arbitrary opaque provider events are embedded in
this first API.
