# Branchyard: Rust SDK for meta-harnesses

Design revision 3 · 16 September 2026 · Architecture specification

Repository: [vamsiramakrishnan/branchyard](https://github.com/vamsiramakrishnan/branchyard). The initial commit contains a controls crate and tested upstream assets. Server, SDK, node, and integration operations below are implementation specifications, not shipped services.

## 1. Decision

Branchyard is an open-source Rust SDK for building a meta-harness that controls other harnesses. A meta-harness supplies the decision loop: how to decompose work, which harness to use, when to delegate, when to revise the topology, and which results to pursue. Branchyard supplies durable control operations backed by server-side execution. Existing coding harnesses can act as meta-harnesses through a skill with CLI or MCP access.

The SDK is the primary developer contract. The reference server and node services implement its execution backend. All managed harness execution and sandbox state live on servers. CLI, web and SDK callers observe and control that remote state; they do not host desktop workers.

Build the graph, delegation and integration semantics. Reuse existing implementations for virtualization, guest execution, snapshotting, networking, persistent queues, storage access and policy evaluation.

The server owns execution. The SDK is a remote client. No desktop worker, new hypervisor, or custom guest protocol is part of the design. Local mode (§4) runs the same engine in-process for development; it is a deployment of the server, not a separate worker.

The proposed first deployment uses:

- Microsandbox through its Rust SDK on dedicated Linux execution hosts.
- Tokio, Axum and Tower for server concurrency and request handling.
- Tonic/Prost with rustls for authenticated node RPC.
- PostgreSQL through SQLx for authoritative domain state.
- PGMQ for durable commands in the same database transaction.
- Apache OpenDAL for artifact and snapshot object storage access.
- Cedar for authorization policy evaluation.
- Existing Git and harness implementations for code integration and model interaction.

PostgreSQL, Git, Linux and the existing harnesses remain external infrastructure. Rewriting them in Rust would add maintenance without improving this product.

## 2. Scope and invariants

The topology is created and revised during execution. Configuration defines environments, permitted harnesses, resource limits and acceptance contracts. It contains no mandatory worker graph, specialist committee or fixed pipeline.

The implementation must enforce these invariants:

1. Disconnecting any client has no effect on execution.
2. Accepted topology changes, budget reservations and their execution commands commit atomically.
3. Children cannot acquire authority outside their delegation envelope.
4. Retries do not create additional accepted operations for the same idempotency key.
5. Runtime uncertainty is explicit; missing telemetry is never proof of completion.
6. Only an exact validated candidate can be promoted.
7. Shared mutable state has enforced ownership; isolated changes have an explicit integration strategy.
8. Provider capability differences are observable. Unsupported operations never silently weaken isolation or recovery.

Task topology and infrastructure topology are independent. A child can run beside its parent or on another eligible host. Dependencies do not determine physical placement.

## 3. SDK contract and reuse decisions

### The developer-facing product

Keep three boundaries explicit:

| Boundary | Owner |
| --- | --- |
| Decision loop and topology proposals | The meta-harness built or selected by the developer |
| Durable commands, permissions, budgets and acceptance | Branchyard SDK contract and server implementation |
| Virtualization, guest execution and snapshots | Existing sandbox runtime |

Branchyard does not require a new model loop or a prebuilt planner agent. A custom Rust service can drive its SDK, or an existing harness can invoke the same operations through tools. A managed root meta-harness may itself run in a server-side sandbox. Any child with delegated rights can act as a meta-harness for its subtree.

The public Rust crate should be named branchyard, subject to package-name availability. It is a typed asynchronous client of the reference backend. Keep the domain engine in an internal crate; do not require SDK consumers to link a VMM, SQL client, Wasmtime or policy engine.

Proposed SDK operations, not an implemented API:

~~~text
Client.connect(endpoint, credential_provider)
Client.submit(TaskSpec) -> TaskHandle
TaskHandle.propose(GraphProposal) -> OperationHandle
TaskHandle.events(cursor) -> EventStream
RunHandle.send(Message) -> OperationHandle
RunHandle.checkpoint(CheckpointSpec) -> OperationHandle
RunHandle.publish(ArtifactManifest) -> ArtifactHandle
TaskHandle.integrate(IntegrationSpec) -> CandidateHandle
TaskHandle.cancel() -> OperationHandle
~~~

Expose typed TaskSpec, RunSpec, SandboxRequirements, ResourceBinding, DelegationEnvelope, ArtifactRef and IntegrationSpec. Convenience spawn calls lower to the same graph proposal contract, including idempotency and expected revision. Builder ergonomics must not hide authority or error semantics.

A returned handle identifies remote durable state. Dropping a handle or cancelling a local Future stops observation; it does not implicitly cancel accepted remote work. Explicit cancellation is a command. A network timeout exposes operation identity so the caller can reconcile whether submission committed.

The SDK preserves commands and effects, not arbitrary Rust stack frames. The meta-harness resumes by reading durable graph state and events, or through its harness-native session mechanism. No promise of transparent replay of arbitrary orchestration code.

Provide a fake backend implementing the public contract for deterministic tests. Keep one semantic implementation on the server; CLI, MCP and language bindings adapt to it.

### Infrastructure to reuse

| Responsibility | Reuse | What Branchyard adds |
|---|---|---|
| MicroVM lifecycle, image preparation, guest execution | Microsandbox Rust SDK and runtime | Attempt identity, placement, capacity accounting and policy binding |
| Suspend/resume multiplexing on Kubernetes (alternative provider) | Agent Substrate gRPC API | Capability mapping, tenant-to-atespace binding, UID-fenced lifecycle |
| Async I/O and coordination | Tokio / tokio-util | Explicit concurrency limits and task ownership |
| Public HTTP, streaming, middleware | Axum / Tower | Task API and tenant context |
| Internal RPC | Tonic / Prost / rustls | Versioned node and extension contracts |
| Transactions and connection pooling | SQLx + PostgreSQL | Domain tables, invariants and migrations |
| Durable command delivery | PGMQ | Command payloads, handlers and idempotent effects |
| Storage backends and transfers | Apache OpenDAL | Artifact manifests, authorization and retention |
| Authorization evaluator | cedar-policy | Resource schema and delegation envelopes |
| Content hashing | blake3 | Branchyard artifact identity; retain native OCI/Git digests |
| Code merges and ref operations | Git executable | Candidate/check/promotion protocol |
| Harness sessions | Native protocols and official ACP SDK | Normalized capabilities, outcomes and provenance |
| Optional MCP access | Official rmcp SDK | Translation to the same control API |
| Portable extension execution | Wasmtime Component Model, later | Versioned bounded hook contracts |

Microsandbox documents Rust APIs, OCI support and snapshot/branch operations; its repository labels the software beta. Its advertised sub-100 ms boot figure is an M1 guest-boot measurement, not evidence of our Linux server latency. Select it as the implementation to qualify, pin a tested SDK/runtime pair, and use conformance results to decide which capabilities ship. [Microsandbox repository](https://github.com/superradcompany/microsandbox)

[Agent Substrate](substrate.md) is a second provider candidate for operators who already run Kubernetes. It suspends idle sandboxes to object storage and resumes them onto warm workers, which suits trees of mostly waiting harnesses. It has no exec API; Branchyard's bridge, run as the actor's entry point, provides exec with independent pipes over routed ingress, and code crosses as git bundles because actors have no host mounts. That path is unqualified. It stays optional: Kubernetes remains outside the default per-spawn path (§4).

The supporting primitives already exist: [Tokio synchronization](https://docs.rs/tokio/latest/tokio/sync/index.html), [Tower limits](https://docs.rs/tower/latest/tower/limit/index.html), [SQLx](https://docs.rs/sqlx/latest/sqlx/), [PGMQ](https://github.com/pgmq/pgmq), [OpenDAL](https://github.com/apache/opendal), [Cedar](https://github.com/cedar-policy/cedar), [Tonic](https://docs.rs/tonic/latest/tonic/), and [BLAKE3](https://github.com/BLAKE3-team/BLAKE3).

### Self-hosted runtime versus vendor cloud

The selected dependency is the public Apache-2.0 runtime running on Branchyard execution servers. Here, local means local to each server process; it does not imply execution on the developer's desktop. The repository documents Linux/KVM execution and an embeddable Rust SDK. Its source license is separate from the vendor's hosted service terms. [Runtime source and requirements](https://github.com/superradcompany/microsandbox), [runtime license](https://github.com/superradcompany/microsandbox/blob/main/LICENSE).

The vendor advertises cloud access as private beta and its supported BYOC offering as an evaluation. Those are optional commercial deployment paths. Their access process and hosted pricing are not dependencies of this reference architecture. The website's caveat about owner confirmation of rates is not a runtime permission gate. Do not use provisional cloud prices as the cost model for self-hosted execution. [Deployment options](https://microsandbox.dev/), [hosted pricing](https://microsandbox.dev/pricing).

Branchyard's own server/node services supply fleet placement, tenant policy and durable task coordination. The public runtime supplies node-local execution. Self-hosting still incurs host capacity, storage, traffic and operational costs; the open-source license does not provide those services.

Add an explicit upstream-independence gate to runtime qualification:

1. Build or install an exact public SDK/runtime release with documented source and bundled-dependency provenance.
2. Preload the required images and artifacts from operator-controlled storage.
3. Run with no vendor cloud credentials; deny the vendor's control endpoints while allowing only the workload services required for the test.
4. Exercise create, exec, snapshot/restore, cancellation, inspection and restart recovery through public APIs.
5. Record supported features for that exact release. A feature that exists only in private beta or unreleased source cannot satisfy the release gate.

Keep SandboxProvider independent of vendor SDK types. If this test exposes a private dependency or missing guarantee, resolve it upstream or qualify another existing provider; do not turn a hidden vendor service into an implicit requirement.

### Explicit exclusions

Do not build a VMM, OCI image loader, guest execution daemon, TCP stack, general workflow engine, event broker, storage SDK, authorization language or terminal emulator.

Do not add QUIC, io_uring, an actor framework or a new database solely because they are Rust implementations. Adopt a second execution or transport mechanism only when a benchmark or compatibility requirement warrants its operational cost.

Restate is technically relevant to durable execution and offers a Rust SDK. Its current server license is BSL 1.1 and explicitly distinguishes itself from an open-source license, so it is not a required dependency of this fully OSS reference deployment. This is a packaging decision, not a claim that all commercial use is prohibited. [Restate license](https://github.com/restatedev/restate/blob/main/LICENSE)

Nydus provides Rust-based on-demand image loading and chunk deduplication. It is an optional cold-cache optimization if compatible with the selected runtime. Do not assume a drop-in integration or stack a second image path over Microsandbox without measuring a need. [Nydus](https://github.com/dragonflyoss/nydus)

## 4. Deployment and ownership

Ship three binaries:

| Binary | Role |
|---|---|
| branchyard | Thin authenticated client; no local execution |
| branchyard-server | API, domain transitions, placement and command workers |
| branchyard-node | Host service using the upstream sandbox SDK |

Reuse the sandbox runtime's guest agent. Do not ship a new branchyard-guest.

### Local mode

For development, the `by` CLI and the `branchyard` SDK embed the engine in-process and run harnesses through a local process provider: its own process group, a scrubbed environment and a private home per harness. Branches are git worktrees of the developer's repository. Task, branch, event, budget and merge semantics are identical to the server's; only the provider and the state store differ (a `.branchyard/` directory in the repository instead of PostgreSQL). Local mode provides no isolation beyond the operating-system user and must say so wherever it runs. Moving work to a server changes the provider and endpoint, not the program.

### Developer vocabulary

The public SDK and CLI name work in git terms: a **task** is what was asked; a **branch** is one agent working in its own worktree and harness session; a **fork** is a new branch from another branch's candidate, forking the conversation where the harness supports it; a **candidate** is the exact commit a branch proposes; a **merge** promotes a candidate only after checks pass against the exact target revision (§12). Domain identities in §5 (task, run, attempt, session, workspace, candidate) remain separate internally.

The server is one modular service initially. Run multiple replicas against the same PostgreSQL primary when needed; transactions and unique constraints coordinate them. Execution hosts maintain outbound authenticated connections to the server, which avoids requiring publicly reachable host control ports.

~~~mermaid
flowchart TD
    C["CLI / SDK / Web"] --> API["Branchyard server"]
    API --> DB["PostgreSQL + PGMQ"]
    API --> N["Branchyard node pool"]
    N --> R["Microsandbox runtime"]
    R --> H["Harness sandboxes"]
    H --> API
    N --> O["Object storage"]
    API --> O
~~~

Deploy control services on ordinary servers or an existing container platform. Execution hosts need the selected runtime's virtualization support; qualify a Linux KVM configuration first. Kubernetes is not a per-spawn dependency. If operators already use it, it can manage node services and control replicas; each harness does not require a new Kubernetes Pod.

Authoritative state belongs in PostgreSQL. Runtime-local state belongs to the upstream runtime and is accessed through supported APIs. Large logs, source bundles and snapshots belong in object storage, with a host-local NVMe cache. Branchyard must not mutate Microsandbox's private database or internal image formats. Its published compatibility map demonstrates how much lifecycle machinery already exists upstream. [Microsandbox compatibility boundaries](https://github.com/superradcompany/microsandbox/blob/main/COMPATIBILITY.md)

## 5. The application model

Use distinct identifiers and records:

| Record | Meaning |
|---|---|
| Task | Objective, root budget, policy and acceptance contract |
| Run | Authorized unit of work with a fixed owner |
| Attempt | Concrete launch/restart with a fencing generation |
| Session | Provider-native conversation and continuation reference |
| Workspace | Source base and writable-state ownership |
| Sandbox | Runtime allocation and execution boundary |
| Artifact | Immutable bytes, provenance and access scope |
| Candidate | Exact proposed integration result |
| Attestation | Checks on an exact candidate and environment |

Maintain an ownership tree, a dependency DAG, a communication graph and resource bindings separately. Communication cycles are valid. Dependency cycles are rejected. Integration operations explicitly combine complementary results or select among alternatives.

The core API accepts a GraphProposal with expected_revision, idempotency_key, bounded typed operations and evidence references. Operations include Spawn, AddDependency, RemoveDependency, Connect, BindResource, Retire and RequestIntegration.

The server derives the caller identity from authentication. It validates the entire proposal and commits one graph revision. It never trusts a caller-supplied run identifier as authority.

A proposal can create several runs and references between them atomically. The resulting topology may be provision-requested while infrastructure is being realized. The proposer receives durable handles, not a promise that all sandboxes already exist.

## 6. Fast sandbox creation

The warm path is admission, placement, reuse of prepared state, private writable allocation, identity binding and task delivery. Image builds, dependency downloads, cloud instance creation and full repository transfer should occur before or outside it.

### Prepared state

Separate:

- Environment snapshot: toolchain and harness software; no task credentials.
- Source checkpoint: exact authorized repository state.
- Session handoff: brief, artifact references and optional native continuation.
- Attempt identity: created at allocation time, never inherited from a snapshot.

Microsandbox distinguishes disk snapshots from full execution snapshots. Its documented full restore can request copy-on-write memory sharing with forked(); direct branching also shares memory while keeping writes private. CPU/memory geometry and runtime compatibility constrain full restore, and application connections require reconnection. Disk restore starts a fresh guest. [Snapshot and branching contract](https://docs.microsandbox.dev/sandboxes/snapshots)

Enable full CoW restore only after a pinned released SDK/runtime pair passes qualification on the target Linux hosts. Documentation on main includes development contracts; presence in current documentation does not by itself establish release support. A supported disk-prepared path remains a distinct selectable capability, not a silent fallback for a requested live fork.

The upstream warm-worker example already separates toolchain installation from worker creation and uses private writable layers. Reuse that preparation lifecycle. [Prepared workers](https://docs.microsandbox.dev/examples/sandboxing/warm-workers)

### Scheduling and launch

1. Authenticate and authorize the requested delegation.
2. Reserve task-wide and tenant-wide allowances in a transaction.
3. Persist graph changes and a PGMQ command.
4. Choose an eligible node with available resources and relevant cached state.
5. Atomically reserve node capacity and assign an attempt generation.
6. Send an idempotent desired allocation over the persistent node channel.
7. The node discovers any existing allocation before asking the SDK to create one.
8. Bind fresh credentials/policy and source state; verify readiness.
9. Record runtime identity, effective configuration and milestones.

The placement policy filters by trust boundary, hardware, runtime compatibility and required capabilities before scoring cache locality and load. A cache hit cannot override tenant isolation or resource capacity.

Maintain headroom on execution hosts. Cloud autoscaling replenishes it asynchronously. When capacity is exhausted, expose queued state and estimated constraints rather than weakening limits.

Warm pools are optional. Start with cached prepared snapshots. Add pre-restored idle sandboxes only when request rates justify memory reservations and cleaning costs.

### Fork semantics

A logical child does not require a live process fork. Default to a prepared environment plus a consistent source checkpoint and fresh harness session.

Live cloning of an active harness may duplicate tool calls, external connections or timers. Make live fork an explicit capability requiring a quiescent point and a post-restore barrier before side effects. Branching the VM does not merge the harness's external session or tool state.

For a group of children, prefer one consistent checkpoint followed by independent restores over repeatedly capturing a changing parent.

### As built: provider snapshots under git checkpoints

Implemented as [sandbox snapshots](sandbox-snapshots.md), tested against fake providers only, and **unqualified** on every provider:

- The provider contract has optional `pause`, `resume`, `branch_live` (children rebound to their own mounts) and `release_checkpoint`, and capabilities name `LIVE_BRANCH`, `PAUSE` and `FULL_SNAPSHOT`. The engine chooses by capability; a live fork is never a silent fallback, nor the reverse: each path is recorded with its reason.
- Environment snapshot and source checkpoint stay separate, as above. A branch that keeps its sandbox (`keep = "pause"`) pauses it between turns, and each git checkpoint takes a provider snapshot of it (Microsandbox: a paused live-branched child; Substrate: suspend and a tag). The code in a snapshot's descendants always comes from git; the snapshot contributes the environment (root disk, running services, and with a live branch, memory).
- Session handoff is unchanged: a branched sandbox never continues the harness's session, and the harness is not running when a snapshot is taken, so no live harness is cloned. Attempt identity is created per turn (Substrate mints a new attempt credential on every resume and branch).
- A group of children takes one consistent capture, as recommended above: `by fan` runs setup once in a prepared sandbox, pauses it, and live-branches it once for every branch.
- Microsandbox's pause, live branch and full snapshots are declared only behind the `live_branch` opt-in until the pinned SDK and runtime pass the ignored KVM tests; with it off, every path falls back to a fresh sandbox, git and setup. Warm pools stay out: a kept sandbox is per branch, bounded per repository by `max_paused` with least-recently-used eviction.

## 7. Storage and networking without new infrastructure projects

Use the sandbox runtime's image cache, snapshot format and writable-layer machinery. OpenDAL transports opaque, versioned artifacts; it is not a POSIX filesystem or a VM memory manager. Store manifests with producer/runtime version, architecture, geometry, scope, digest and compatibility requirements.

Repository content should be materialized once per immutable source checkpoint and cached within its authorization domain. Integrate with supported snapshot/volume APIs. Measure source attachment separately: if the selected backend only supports copying a tree, report that cost rather than claiming instant cloning.

Keep cross-tenant content deduplication off by default. Even a content hash is not an authorization token. Avoid existence leaks and account for retained snapshots and logs as well as active disks.

| Resource | Default |
|---|---|
| Source and build outputs | Private writable state |
| Toolchain/dependency assets | Immutable shared state where supported |
| Artifacts | Immutable, scoped reads |
| Mutable services | Private instance or explicit broker-owned access |
| Shared workspace | Authorized participants; enforced single writer per mutable scope |

A database lease cannot fence an arbitrary live filesystem writer. Revoke its access or confirm termination before assigning another writer. Cross-host live sharing requires a backend that explicitly supports it; otherwise use artifacts.

**Status.** Artifacts and scratch areas are built in local mode: content-addressed, immutable artifacts with reads following the delegation tree (an explicit share for a sibling), and scratch areas with a writer lock reclaimed once its holder's turn ends rather than fenced against a live filesystem writer, exactly as this section says a lease cannot do. See [storage](storage.md).

For networking, reuse the runtime's enforcement and credential facilities. No per-sandbox cloud NIC, public IP, load balancer or DNS provisioning is required in the intended deployment. Publish previews through existing ingress infrastructure and an authenticated sandbox-to-route mapping.

Microsandbox's host-held credential mechanism can substitute secrets for approved destinations. Its documented boundary includes TLS inspection requirements and limitations for guest-side request signing. Test each harness authentication path; do not repeat the repository's strongest marketing claims as guarantees. [Credential mechanism](https://docs.microsandbox.dev/sandboxes/secrets)

Bind fresh run identity after restore. Block cloud metadata and internal infrastructure unless specifically allowed. Require tests for DNS/IP bypass, redirects, destination checks and network revocation. Keep control traffic available under the intended isolated network profile.

Do not add a second packet stack or custom TLS interceptor. If the existing ingress is insufficient, Pingora is a reusable Rust proxy framework, but it is not a mandatory new service. [Pingora](https://github.com/cloudflare/pingora)

## 8. Durable execution using existing queues

PGMQ supplies persistent queue operations, visibility windows and acknowledgments on PostgreSQL. Branchyard supplies command semantics. Use its SQL functions on the same SQLx transaction as domain updates; do not introduce a separate outbox database or custom queue tables.

**As built.** The server's queue is a plain table in the store's own database (`operation_queue` on SQLite, `by_operation_queue` on PostgreSQL), written in the same transaction as the operation record, its idempotency binding and its branch locks, and claimed with `FOR UPDATE SKIP LOCKED` under a fenced lease ([durability](durability.md), [server](server.md)). PGMQ is not assumed to be installed, and SQLite needs the same guarantee, so no extension is required; the table keeps the property this section asks for, one transaction for the command and its domain effects. PGMQ can replace the table on PostgreSQL without changing that transaction.

Conceptual transaction:

~~~sql
BEGIN;
-- Lock this task's mutation record; check revision and idempotency.
-- Apply graph delta and reserve the task/tenant budget.
-- Append durable control events and the accepted command record.
SELECT pgmq.send('branchyard_commands', :command_json);
COMMIT;
~~~

Execute this through prepared queries with bound parameters. The example is a transaction sketch, not runnable schema code.

Queue delivery can repeat after a visibility timeout. That is not exactly-once external execution. Command handlers identify effects by operation ID, expected attempt generation and recorded outcome. Acknowledge only after recording the durable disposition. Launch or promotion ambiguity requires reconciliation before repetition.

Process short control steps through the queue. Do not hold a message invisibly for an hours-long model session; persist the active run and handle later observations as events. Use delayed commands for durable deadlines and retries. Periodic reconciliation covers missed or delayed signals.

For low dispatch latency, send PostgreSQL notification hints after enqueue and retain bounded polling as the recovery path. Notifications are not the durable source. Use a bounded number of queue consumers and database connections, never one connection per sandbox or subscriber.

The server owns domain state transitions; this is not a general workflow engine. There is no replay of arbitrary user code or invented exactly-once process execution.

Local mode implements this model today on an embedded SQLite store: leases with fencing generations, journaled steps, durable cancel requests and deadlines, and startup reconciliation that never replays a model turn. [Durability](durability.md) describes it and maps it onto PostgreSQL.

## 9. Concurrency and backpressure

Use one Tokio runtime per service process. Thousands of idle sessions must not imply thousands of operating-system threads or open database transactions.

| Mechanism | Decision |
|---|---|
| Request admission | Tower concurrency limits, deadlines and load shedding |
| Internal work channels | Bounded Tokio mpsc; separate control from logs |
| Resource gates | Separate semaphores for launch, transfer and validation |
| Task lifetime | JoinSet/TaskTracker and cancellation propagation |
| Blocking work | Bounded blocking/CPU workers for hashing, Git and image work |
| Shared read state | Immutable cached views; authoritative checks in transactions |
| Database access | Bounded SQLx pools with reserved capacity for cancellation |
| Streaming | Bounded buffers; cursors and explicit gap behavior |

This is a proposed application of the cited libraries, not a claim that importing them automatically provides these guarantees.

Never hold an async lock across a network request, process wait or database round trip. Serialize mutations within one task as needed, not across the fleet. Keep log writes and heartbeat metrics away from the task's graph-revision lock.

Use short transactions. Acquire task, tenant and node budget locks in a documented consistent order. Retry serialization/deadlock failures within a bounded request deadline. Avoid a fleet-wide counter.

Root budgets include every descendant. Record reservations and reconcile reported usage; unknown provider billing stays unknown. Waiting orchestration logic releases active-execution capacity. A parent that is blocked solely on child completion must not prevent its children from obtaining slots.

Control events are durable and replayable. stdout/stderr are chunked artifacts with bounded local spool and explicit retention policy. A slow browser cannot backpressure cancellation or graph commands. Lost best-effort telemetry is marked; terminal outcomes are never dropped.

Tenant fairness applies before node selection. Limit launch bursts, CPU, memory, disk growth and outbound traffic separately. Sharing clean memory pages does not remove the need to reserve for private dirty pages.

## 10. Protocol and extensibility

Public interface: HTTPS JSON commands and SSE event streams through Axum. The public Rust SDK uses this endpoint and owns typed handles, credential refresh integration, bounded observation and idempotency-aware retry behavior. Use request IDs, idempotency keys, structured errors, bounded payloads and resumable event cursors.

Internal interface: Tonic/Prost over authenticated TLS connections. Separate control streams from log/bulk channels. Retain reconnect acknowledgments and generations at the application layer; transport reconnection alone does not provide durability.

Upload/download artifacts through existing object-storage transfer mechanisms. Avoid routing gigabyte snapshots through the command API.

Define narrow provider contracts:

~~~text
SandboxProvider:
  capabilities, ensure, inspect, exec, stop, destroy
  optional checkpoint, restore, branch, share

HarnessDriver:
  capabilities, start, send, observe, interrupt, resume

IntegrationPolicy:
  propose_candidate, required_checks, acceptance_decision
~~~

Capabilities include precise scope: disk or full snapshot, crash or application consistency, same-host or portable restore, supported sharing, and native versus reconstructed session continuation. Record effective guarantees on each attempt.

First-party adapters use Rust traits and typed enums. Out-of-process extensions use a versioned RPC contract. Avoid an unstable Rust dynamic-library ABI.

For bounded user extensions, adopt Wasmtime and WIT interfaces when that requirement is implemented. Suitable hooks rank eligible nodes, score completed candidates or transform bounded artifact metadata. Give each hook explicit inputs, fuel/time/memory limits and minimal host functions. It cannot override hard authorization or acceptance invariants. Keep compilation off the request path; treat precompiled artifacts as trusted only under the runtime's compatibility and provenance rules. [Wasmtime](https://github.com/bytecodealliance/wasmtime)

Harnesses requiring arbitrary OS behavior remain in microVMs. WASI components are an extension mechanism, not a universal replacement for coding sandboxes.

## 11. Authorization and recovery ownership

Authenticate users through the deployment's existing identity provider and nodes through workload identity/mTLS. Use existing Rust protocol libraries rather than implementing cryptography.

Cedar evaluates access to typed Task, Run, Workspace and Artifact resources. Branchyard issues a scoped delegation envelope, stores its parent, and enforces attenuation and root budgets. Cedar alone does not create delegation or revoke filesystem access.

Version policy and acceptance contracts. Changes cannot silently expand a running child's privileges. Cache validated policy representations with explicit invalidation; authoritative revocation and fencing checks remain on privileged operations.

Each allocation has an attempt ID, owning node, lease and generation. On reconnect, the node reports observed allocations. The server reconciles them against desired state before rescheduling.

Node partition handling:

- The node enforces a local expiry deadline when control renewals stop.
- Server-side services reject stale generations.
- Storage and tool brokers fence stale writes where they can.
- Replacement execution uses new private state until prior shared-writer termination is proven.

A lease does not physically stop a partitioned host. External side effects require idempotent APIs or service-level fencing. Stale results can be rejected even when a stale process could not yet be killed.

Use the upstream runtime's lifecycle and ownership APIs for stop/reaping. Do not duplicate its PID, lock-file or guest-protocol implementation. If public APIs cannot prove a required guarantee, document the blocker and contribute upstream.

## 12. Validated integration

Branchyard owns this protocol because VM branching and queue delivery cannot decide whether code should be accepted.

Record exact source base B, child result C, current target T, environment digest and acceptance-contract digest.

1. Validate provenance and ancestry.
2. Combine complementary results in a private integration workspace, or explicitly select one competing result.
3. Run trusted checks on the exact candidate.
4. Persist an attestation bound to candidate, environment and contract.
5. Persist a promotion intent identifying expected old and new ref values.
6. Compare-and-swap a supervisor-owned Git ref.
7. Record the outcome and acceptance event.

If T changes, construct and validate a new candidate. A clean textual merge is insufficient. Multiple independently passing branches must be tested in combination.

Git and PostgreSQL do not share a transaction. Recovery checks the recorded intent against the actual ref: intended new value completes recording; expected old value permits a guarded retry; another value requires reconciliation. Never force a promotion.

Workers cannot weaken the acceptance contract. Repair runs can change code, and reviewer runs can contribute evidence, but neither can self-certify acceptance.

Shared live edits are already shared state and do not undergo a fictional branch merge. Generated files are regenerated from merged sources. Binary artifacts require explicit selection or a format-aware strategy. External database changes need their own idempotency/migration protocol.

Acceptance and publication are separate operations. Remote repository policies still govern PR merging.

## 13. Developer experience and package structure

README promise:

> Build a meta-harness in Rust. Control existing coding harnesses on your servers. Let the topology evolve, and integrate validated results.

Proposed client commands:

~~~bash
branchyard login --server https://branchyard.example
branchyard submit --repo registered-repo --revision COMMIT "Implement account export"
branchyard watch task_123
branchyard graph task_123
branchyard diff task_123
branchyard cancel task_123
branchyard accept task_123
~~~

These are proposed product interfaces, not installed commands. The client needs no container runtime or local harness. Optional source upload creates an immutable bundle.

The skill explains delegation and artifact use through the same API. It receives scoped authentication and no administrative node access. CLI, SDK and optional MCP share server semantics.

Repository:

~~~text
crates/core/        domain types and pure validation
crates/protocol/    public schemas and internal RPC
crates/server/      transactions, policy, placement, command handlers
crates/node/        upstream sandbox and harness adapters
crates/sdk/         public branchyard Rust SDK
crates/cli/         thin CLI built on that SDK
migrations/
policies/
environments/
skills/
tests/conformance/
tests/faults/
benchmarks/
deploy/
docs/
~~~

Ship the SDK crate and examples as the primary developer package, with a reference server image, node package with a qualified runtime bundle, thin CLI binaries and schema documentation. Include server-side installation recipes for PostgreSQL/PGMQ and object storage. Avoid a required desktop bootstrap.

A lock manifest records Rust toolchain, Cargo.lock, runtime/SDK versions, environment image digests and tested compatibility. Selected control sources are already vendored with exact revisions and licenses; see [vendoring](vendoring.md). Infrastructure crates remain normal dependencies. Adapt a copied control only for a demonstrated gap, retain a patch, add a regression test, and record the condition for dropping the adaptation.

First-party code is under Apache-2.0. The release inventory must include runtime native libraries and bundled binaries as well as Rust dependencies.

## 14. Performance contract

Measure before asserting latency. Proposed initial targets on declared Linux hardware, within one region, with spare capacity and warm compatible snapshots:

| Measurement | Initial target |
|---|---|
| API receipt to durable submission acknowledgment | p95 below 50 ms |
| Durable launch admission to first successful guest command | p95 below 1 second |
| Control-plane subscription test | 10,000 idle simulated subscribers without one thread/DB connection each |
| Burst allocation test | Sweep 1, 8, 32 and 128 requests; state the available host capacity |
| Harness startup and first useful action | Measure separately; no inferred guarantee from guest boot |

These are engineering targets, not benchmark results or production SLOs. They exclude client WAN time and model inference. Record rejection/queue rates alongside latency so load shedding cannot hide overload.

Report cold image, cached image, disk-prepared, full CoW restore and ready-pool paths separately. Include workspace attachment and first-file access, not only VM readiness.

Capture API/DB/queue/placement/restore/source/handshake/harness timings, page faults, dirty memory, CPU steal, disk growth, bytes transferred and cache hit rate. Benchmark realistic repositories and builds. A first command that merely prints a constant is a readiness probe, not proof that a large codebase is ready.

If the critical path is source transfer, optimize data placement. If it is SDK serialization, improve that integration upstream. If it is control-plane contention, reduce shared mutation scope. Do not substitute a faster transport without identifying the bottleneck.

## 15. Implementation plan

The ordered milestones, owners, dependencies, and acceptance gates are in [the implementation plan](implementation-plan.md). The first commit establishes researched contracts and a reusable controls crate. It does not claim to implement a fake server or any of the proposed SDK task methods.

Qualify the public sandbox SDK before implementing a scheduler around its assumed capabilities. In parallel engineering work, the control/state, execution/harness, and workspace/integration lanes can proceed once their shared contracts are agreed. Pair on ownership fencing and Git/database recovery.

## 16. Verification and first-release acceptance

Use proptest for graph and budget invariants. Use a deterministic fake sandbox driver for state-machine exploration. Turmoil can exercise compatible Rust networking components under delay and partition; it does not simulate PostgreSQL, microVM execution or filesystem durability. Use real services and process kills for those boundaries. [Turmoil](https://github.com/tokio-rs/turmoil)

Required failure cases:

- Duplicate spawn and stale graph revision.
- Worker starts but acknowledgment is lost.
- Queue visibility expires while an effect is being reconciled.
- Node reconnects with stale generation.
- Cancellation during restore or a shared write.
- Snapshot lacks required capability or runtime compatibility.
- Credential/network policy attempts to broaden after fork.
- Object upload succeeds before its manifest transaction fails.
- Target branch moves after validation.
- Git promotion succeeds before database completion.
- Slow subscribers, log floods and exhausted disk/capacity.

Artifact publication records a committed manifest only after verified upload. Orphan uploads are collected after a grace period. Cancellation and cleanup remain idempotent.

The release demonstration begins with one remote root and no predefined worker graph. It discovers work, creates children across two harness implementations, and creates a grandchild when a prerequisite emerges. The client disconnects. An execution connection fails. The system reconciles ownership, integrates conflicting edits and rejects stale validation after target movement. A second client observes the same durable task and accepts one validated result.

## 17. Relationship to the upstream controls

Scion's provisioning bundles, Herdr's session recipes and observation data, and OpenRig's launch contract are retained as pinned upstream sources. Warp's selected AGPL helpers are retained separately as unlinked references. Replicas supplies a product reference; no licensed runtime implementation was identified to copy. Exact revisions and adaptations are in [third-party notices](../THIRD_PARTY.md) and [vendoring decisions](vendoring.md).

The runtime protocol choices and 16-harness research matrix are in [harness integration](harness-integration.md). Build one ACP client over the existing Rust SDK, plus a small number of native driver families. Do not implement sixteen unrelated orchestration loops.

Branchyard owns dynamic collaboration state and the path from isolated work to accepted code. Upstream integrations supply the execution primitives beneath that boundary.
