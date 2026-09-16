# Implementation plan

Revision 4: put the SDK and portable harness surface first, then connect it to
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

No production API server, database schema, scheduler, node, sandbox provider,
ACP/native driver or integration coordinator exists yet. Test HTTP fixtures are
not development executors and cannot qualify durability or isolation.

## Ordered slices

| Gate | Deliverable | Acceptance evidence |
|---|---|---|
| S0: caller surface — implemented | Shared protocol, Rust SDK, CLI, skill/plugin, scripts | Input/response validation; lost response reconciliation; no automatic retry; concurrent calls; extracted distribution works |
| S1: durable admission | Axum/Tower API, authenticated principal, SQLx/Postgres operation/state schema, root reservations, transactional PGMQ command | Same request replays; changed input conflicts; stale revision leaves all state unchanged; crash after commit before response reconciles; retention/tombstones prevent identity reuse |
| S2: sandbox qualification | Existing Microsandbox SDK/runtime on Linux KVM, provider contract derived from measured behavior | Create, inspect, exec with separate pipes, stop/destroy, private writes, resource/network enforcement without vendor-cloud credentials |
| S3: one remote task | Node ownership, attempt fencing, source materialization, one ACP driver, artifacts | Submit remotely; client disconnect; same task observed from another client; explicit cancel; recover lost worker acknowledgment |
| S4: dynamic delegation | Atomic graph deltas, scoped child credentials, root accounting, dependencies | Parent creates child which creates grandchild; no predefined graph; cycles/escalation/budget exhaustion reject atomically |
| S5: workspaces and sharing | Prepared checkpoints, registered component bindings, placement, cache reuse | Private writes after fork; verified revoke before new exclusive writer; no secret inheritance; measured cold/warm critical paths |
| S6: accepted code | Candidate creation, trusted checks, attestation, promotion intent, Git CAS | Conflict repair; target movement invalidates evidence; recover after ref update before database completion |
| S7: second driver | Codex App Server or another native profile | Same task API works across ACP and native paths; session/permission differences stay visible |
| S8: operational release | Tenant fairness, bounded event streaming, artifact access, package publishing, deployment guide | Slow clients/log floods do not block cancel; stale nodes cannot publish; fresh installation reproduces release scenario |

S1 and S2 can be developed independently once the allocation identity/fencing
contract is agreed. S3 needs both. Keep S4 independent of a sixteen-harness roster.
Do not publish SDK/runtime compatibility claims based on the client fixture.

## First next implementation: admission and reconciliation

1. Treat `docs/control-api.md` and generated schemas as the initial wire contract.
   Define a tenant-scoped authentication interface; delegated tokens carry root,
   subtree, action, resource and expiry restrictions. A task file cannot grant
   itself authority by naming a permissive profile.
2. Introduce migrations for operations, task identities/revisions, graph state,
   reservations and dispatch records. Resolve command identity before effects.
   Define operation retention, tombstones and read-after-write consistency.
3. Under a transaction, validate the resulting graph and policy, reserve the root
   envelope, insert the operation and PGMQ command, and then return its receipt.
   Treat all client fields as untrusted. Reject unsupported required capabilities.
4. Persist execution uncertainty separately from HTTP uncertainty. Queue redelivery
   first reconciles the operation/attempt; it does not reissue an unknown model turn.
5. Exercise real Postgres crashes and concurrent submissions. Repeat the same
   SDK/CLI requests against this implementation. Prove transaction behavior rather
   than making a more realistic in-memory demo.

Next, integrate one provider and harness with an explicit attempt generation.
A worker that reconnects with stale authority cannot claim completion or release
someone else's resource. Callback filesystem/terminal operations execute inside
the sandbox, never on the host. Continuous ACP request servicing must remain live
while a prompt is pending.

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
