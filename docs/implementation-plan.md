# Implementation plan

Revision 5: put the SDK and portable harness surface first, then connect it to
real server execution. [Straitjacket's mechanisms](straitjacket.md) inform the
boundaries; its local executor is not the Branchyard backend.

## Implemented foundation

- `branchyard-protocol`: identities, versioned command/read contracts, local
  validation, input fingerprints and generated schemas.
- `branchyard-sdk`: pooled async remote client, deadlines, bounded bodies,
  receipt checks and uncertain-submission reconciliation.
- CLI, canonical skill, Codex/Claude manifests, explicit installer, launcher,
  reproducible archives and extraction tests outside the checkout.
- Preserved resume controls, 82 pinned upstream assets, integrity checks and the
  explicit Scion Claude compatibility baseline.

- `branchyard-server`: authenticated admission, transactional graph/reservation/
  operation/event writes and PGMQ dispatch. Bootstrap credentials carry tenant,
  subject, expiry, action and optional subtree restrictions.
- PostgreSQL/PGMQ integration tests and a dedicated CI database service.

No queue consumer, execution node, sandbox provider, ACP/native driver or
integration coordinator exists yet. Admission does not qualify runtime isolation.
Scoped credentials are operator-issued; runtime credential delegation is pending.

## Ordered slices

| Gate | Deliverable | Acceptance evidence |
|---|---|---|
| S0: caller surface — implemented | Shared protocol, Rust SDK, CLI, skill/plugin, scripts | Input/response validation; lost response reconciliation; no automatic retry; concurrent calls; extracted distribution works |
| S1: durable admission — implemented | Axum/Tower API, authenticated principal, SQLx/Postgres operation/state schema, root reservations, transactional PGMQ command | Same request replays; changed input conflicts; stale revision leaves all state unchanged; crash after commit before response reconciles; retention/tombstones prevent identity reuse |
| S2: sandbox qualification | Existing Microsandbox SDK/runtime on Linux KVM, provider contract derived from measured behavior | Create, inspect, exec with separate pipes, stop/destroy, private writes, resource/network enforcement without vendor-cloud credentials |
| S3: one remote task | Node ownership, attempt fencing, source materialization, one ACP driver, artifacts | Submit remotely; client disconnect; same task observed from another client; explicit cancel; recover lost worker acknowledgment |
| S4: dynamic delegation — graph admission implemented | Atomic graph deltas, scoped child credentials, root accounting, dependencies | Parent creates child which creates grandchild; no predefined graph; cycles/escalation/budget exhaustion reject atomically |
| S5: workspaces and sharing | Prepared checkpoints, registered component bindings, placement, cache reuse | Private writes after fork; verified revoke before new exclusive writer; no secret inheritance; measured cold/warm critical paths |
| S6: accepted code | Candidate creation, trusted checks, attestation, promotion intent, Git CAS | Conflict repair; target movement invalidates evidence; recover after ref update before database completion |
| S7: second driver | Codex App Server or another native profile | Same task API works across ACP and native paths; session/permission differences stay visible |
| S8: operational release | Tenant fairness, bounded event streaming, artifact access, package publishing, deployment guide | Slow clients/log floods do not block cancel; stale nodes cannot publish; fresh installation reproduces release scenario |

S1 and S2 can be developed independently once the allocation identity/fencing
contract is agreed. S3 needs both. Keep S4 independent of a sixteen-harness roster.
Do not publish SDK/runtime compatibility claims based on the client fixture.

## Next implementation: qualify a runtime, then fence execution

Admission uses one PostgreSQL transaction under a tenant row lock. It commits the
resulting graph, aggregate reservations, task events, operation receipt and PGMQ
message together. Replays do not enqueue again. Identities are retained indefinitely.
The database suite tests concurrent duplicates, final queue-write rollback, stale
revisions, scopes, quotas and a lost HTTP response after the real handler commits.
This is not yet a database-process crash or failover qualification.

1. Run the [runtime preflight and qualification matrix](runtime-qualification.md)
   on a Linux KVM node. Derive the provider interface from observed operations and
   failure behavior before committing a public trait. Keep vendor cloud optional.
2. Add durable worker claims, attempt generations and leases. PGMQ visibility is
   delivery coordination, not authority to execute. Reconcile uncertain allocation
   and model effects before retrying; stale generations cannot publish or release.
3. Integrate one ACP profile using the maintained protocol client and the runtime's
   separate stdin/stdout pipes. Service permission and filesystem callbacks while
   prompts run; all guest filesystem and terminal actions stay in the sandbox.
4. Mint child credentials through the authenticated server, attenuating parent
   actions, expiry, subtree and root budget. Do not expose bootstrap tenant tokens
   to guests. Prove live parent → child → grandchild execution with no fixed graph.
5. Add candidate artifacts, trusted validation and Git compare-and-swap promotion.
   A successful admission operation is not an execution or integration result.

The local implementation environment has no KVM device or container runtime.
Preflight fails explicitly; it does not substitute process isolation or report a
sandbox qualification. A provisioned KVM node and one harness credential are the
external prerequisites for the execution milestones.

## Packaging and extensions

The CLI and plugin are delivery layers already built on the SDK. Add an MCP
facade when a real caller needs it, using the maintained Rust MCP SDK and existing
Branchyard operations. Do not implement another JSON-RPC stack or hidden daemon.
Qualify a live plugin load with version-pinned hosts before claiming broad support.

Add artifact reads and SSE only with bounded output, access control, replay and
retention-gap semantics. Keep the canonical skill short; references and generated
schemas disclose detail on demand. Generate standalone copies and verify them in
CI rather than editing another skill for every host.

Only build native hooks when a required behavior cannot be expressed through the
portable surface. A host's ability to call Branchyard is separate from Branchyard's
ability to supervise that host. Preserve that distinction in support matrices.

## Performance and runtime receipts

Record CPU, kernel, virtualization access, runtime/SDK revisions, image digest,
source size, storage/network mode and limits. Test disk preparation separately
from full VM fork and native conversation resume. If a capability is unavailable,
expose a named weaker option only when the request permits it; never infer it.

Measure admission, queue wait, placement, guest readiness, source attachment,
protocol handshake and first useful action. Compare cold, cached and warm paths
under declared capacity with rejection/queue rates. The existing sub-second warm
readiness goal remains a target. No fixture test establishes runtime latency.

## Release scenario

Start with one remote root and no worker graph. It creates children using two
qualified drivers; a child creates a grandchild after discovering a prerequisite.
Disconnect the client and interrupt an execution-node connection. Reconnect and
retain all task identities while ownership is reconciled.

Have children produce conflicting code. Repair in an isolated integration attempt.
Move the target after validation and reject the stale evidence. Revalidate, promote
through a guarded intent, and recover if database recording is interrupted after
Git changes. This complete scenario, with its failure tests, is the release gate.
