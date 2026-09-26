# Implementation plan

The first commit captures the architecture and reusable controls. Build the next slices in dependency order. Ship a complete remote task before expanding the harness roster.

## Current foundation

- Rust workspace with a dependency-free resume control crate and preserved upstream tests.
- One harness identity registry, checked against every vendored source and the integration matrix.
- The `HarnessDriver` half of M1's contracts, with drivers for Claude Code stream-json, Codex App Server and ACP v1. Twelve of sixteen targets have an unqualified default profile; see [implemented drivers](harness-integration.md#implemented-drivers).
- Vendor-independent sandbox capability types with an admission check (the capability half of M1's provider contract).
- An unqualified Agent Substrate provider adapter, generated from its vendored proto; see [Agent Substrate](substrate.md).
- 84 unchanged upstream files with commit pins, Git blob IDs, SHA-256, and licenses.
- Scion provisioning tests and an explicit compatibility exclusion for its Claude model-alias mismatch.
- Architecture, harness interface design, vendoring decisions, and validation records.

The task SDK, database schema, server, node, driver registry, and integration coordinator do not yet exist. Crate names below describe intended modules, not empty crates created to imply progress.

## Milestones and acceptance gates

| Milestone | Deliverable | Depends on | Gate |
|---|---|---|---|
| M1: contracts | Domain identities, capability types, command/event schema, fake provider and fake harness | Foundation | Type-level separation of task/run/attempt/session/workspace; duplicate command and stale revision tests |
| M2: runtime qualification | Existing Microsandbox SDK/runtime on a Linux KVM host | M1 provider contract | Create, exec with independent pipes, inspect, stop, destroy, private writes, resource/network enforcement; no vendor-cloud credentials |
| M3: durable commands | PostgreSQL schema, SQLx transactions, PGMQ delivery, operation lookup | M1 | Commit-and-timeout reconciliation; duplicate delivery; crash before acknowledgment; no repeated unknown external effect |
| M3.5: local mode | `branchyard` SDK engine and `by` CLI over a local process provider, git worktree branches and validated merge | M1 | `by run`, `by fork`, `by merge` work end to end on a local repository; merge refuses a moved target and failing checks; budget and policy stop and answer out-of-policy actions |
| M4: one remote task | Thin SDK, server, node, one ACP harness, artifacts | M2, M3 | Submit remotely, disconnect client, reconnect from another client, observe same task, cancel, inspect result |
| M5: dynamic delegation | Atomic graph proposals, root budgets, scoped capabilities, dependencies | M4 | Parent creates child which creates grandchild; no predefined graph; invalid delta leaves state and reservations unchanged |
| M6: prepared workspaces | Environment/source checkpoints, explicit sharing modes, cache-aware placement | M2, M5 | Private writes after fork; revocation before writer reassignment; warm/cold timing breakdown; no secret inheritance |
| M7: validated integration | Candidate construction, trusted checks, attestation, promotion intent and ref CAS | M5, M6 | Conflicts return for repair; moved target invalidates candidate; recover after Git update before DB completion |
| M8: native drivers | Codex App Server and selected native profiles from the matrix | M4 driver contract | Two harness implementations complete the same task API; negotiated differences remain visible |
| M9: operational limits | Backpressure, tenant fairness, partition handling, retention, observability | M4–M8 | Log floods and slow clients do not block cancel; stale nodes cannot publish accepted results; orphan resources reconciled |
| M10: distribution | Versioned SDK, server image, node package, deployment guide, compatibility lock | M9 | Fresh server installation reproduces the release demonstration |

Do not gate dynamic delegation on implementing all sixteen harnesses. One ACP profile plus one native driver provides a stronger contract test than many unqualified launch commands.

## Engineering lanes

| Lane | Owns | Interfaces to agree first |
|---|---|---|
| Control and state | Domain transitions, SQL schema, policy, budgets, queue handlers | Command IDs, generations, graph revision, event envelope |
| Execution and harnesses | Runtime qualification, node lifecycle, ACP, native adapters | SandboxProvider, process streams, HarnessDriver, callback routing |
| Workspaces and developer experience | Source checkpoints, integration, public SDK, CLI, docs | ResourceBinding, ArtifactManifest, Candidate, Attestation |

Each change should complete a behavior and its failure path. Cross-review ownership fencing and Git/database reconciliation across lanes. A small team can rotate these responsibilities; they are boundaries, not prescribed headcount or a static agent topology.

## First executable vertical slice

1. Define a `TaskSpec` referencing a registered repository revision, a registered harness profile, a resource limit, and a result contract.
2. Accept it with an idempotency key; persist the task and queue command together. Return an operation ID before allocation completes.
3. Lease a worker allocation with an attempt generation. Reconcile a previous allocation before retrying creation.
4. Materialize exact source state in a private sandbox. Project scoped configuration through the selected upstream provisioner or adapter.
5. Start one structured protocol session. Handle callbacks within the sandbox. Persist effective capabilities.
6. Publish a verified artifact manifest and a typed outcome. Retain the distinction between model completion and accepted result.
7. Demonstrate client disconnect, explicit cancellation, and recovery from a worker acknowledgment loss.

This slice requires a real sandbox host. A unit-test fake is useful for determinism but cannot establish kernel isolation, network policy, or filesystem durability.

## Runtime qualification record

Record host CPU/architecture, kernel, virtualization access, runtime release and commit, SDK version, image digest, source size, storage backend, network mode, resource limits, and runtime logs. Test lifecycle without vendor-cloud credentials or control endpoints. Mark snapshot capabilities separately: disk/full, consistent/crash-consistent, local/portable, fresh identity, and sharing behavior.

If live clone is unavailable, expose an explicitly named prepared-disk path. If basic isolation or lifecycle fails, qualify an existing alternative behind the same provider contract. Do not start a new VMM or depend on private runtime internals.

## Budget and side-effect model

Reserve before spawning; account across the entire ownership subtree. Keep concurrent compute limits separate from spend reservations and cumulative usage. Limit allocation growth and lease duration even when a provider cannot report exact cost.

Hard model spend bounds require a metered broker or equivalent provider enforcement. A harness receiving an unrestricted provider credential can create unobserved requests. Such a profile must report its weaker accounting guarantee and cannot satisfy a task requiring strict per-request budget admission.

Each side-effecting command has an operation ID and expected generation. Queue retries repeat reconciliation, not uncertain side effects. Deployment, remote pushes, and external database changes require explicit capabilities beyond producing a local candidate.

## Performance work

Instrument admission, queue wait, placement, image readiness, source attachment, guest command readiness, harness handshake, and first useful action. Preserve cold and warm results separately. Include capacity, failures, and queue rates in every benchmark.

The design's sub-second warm readiness goal is an engineering target. It is not an upstream-derived guarantee. Optimize measured costs: prefetch source, reuse clean prepared state, bound CPU work, and reduce task-lock contention before introducing another transport or storage layer.

## Release demonstration

Begin with one remote meta-harness and no worker graph. It creates two children using different drivers. A child discovers a prerequisite and spawns a grandchild. Disconnect the client. Interrupt a node connection. Reconnect, reconcile ownership, and retain every task identity.

Have children produce conflicting code. Resolve the conflict in an isolated integration attempt. Move the target after an initial validation and demonstrate that stale evidence cannot promote the candidate. Revalidate, promote through a guarded intent, and recover correctly if database recording is interrupted after the Git operation.

The release is ready when this demonstration and the documented failure tests pass on a fresh server installation. A successful model response or a large number of registered harnesses is not the release criterion.
