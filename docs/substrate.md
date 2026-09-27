# Agent Substrate

[Agent Substrate](https://github.com/agent-substrate/substrate) runs many mostly idle sandboxes ("actors") on fewer warm Kubernetes pods ("workers"). It suspends an idle actor to object storage and resumes it on any free worker. Branchyard uses it as an optional sandbox provider. The adapter is `crates/branchyard-substrate`; it is **unqualified** until it passes the runtime gates in the [implementation plan](implementation-plan.md) against a real cluster.

Reviewed at Substrate revision `1d7ca8ced056192a1801d6565251adcaab3eb0c9`.

## Division of responsibility

The projects sit at different layers, and each leaves out what the other supplies.

| Concern | Agent Substrate | Branchyard |
|---|---|---|
| Unit of work | Actor: an instance of a template, suspended and resumed | Task, run, attempt, session, workspace and candidate, each with its own identity |
| Topology | None; actors are independent | Parent/child graph proposed at runtime, with budgets and delegation envelopes |
| Execution | gVisor or microVM sandboxes on Kubernetes worker pods | Delegated to a provider |
| State | Memory and filesystem snapshots in object storage; tags name them | Durable commands and domain state in PostgreSQL; checkpoints referenced through the provider |
| Harness awareness | None; a harness is an OCI image | Drivers with explicit fresh/resume/fork and permission handling |
| Code integration | Out of scope | Validated candidates and guarded promotion |
| Maturity | Pre-1.0 running system | Design plus foundation crates |

Substrate describes itself as "not an SDK for building agents." Branchyard is the harness-aware, merge-aware layer above such a runtime. It does not reimplement Substrate.

## How the adapter is built

- `vendor/substrate/pkg/proto/ateapipb/ateapi.proto` is an unmodified upstream copy, pinned in `vendor.lock.json`.
- `build.rs` generates the Rust client with `tonic-prost-build` and a pinned `protoc` from `protoc-bin-vendored`. Host `protoc` installations are ignored.
- The adapter maps the generated API onto `branchyard-sandbox` types. No Substrate Go code is translated. See [vendoring](vendoring.md#agent-substrate-generate-from-the-contract-do-not-translate-the-runtime).

## Operation mapping

| Branchyard operation | Substrate call | Notes |
|---|---|---|
| `capabilities(template)` | `GetActorTemplate` | Derived from `snapshot_config.on_commit` |
| `ensure(name, template)` | `CreateActor`, then `ResumeActor` | An existing actor is adopted only if it came from the same template |
| `start` | `ResumeActor` | No-op for a running actor |
| `inspect` | `GetActor` | Fails with `Replaced` if the name now refers to a different UID |
| `stop` | `SuspendActor` | Portable snapshot; releases the worker |
| `checkpoint(name)` | `CreateTag` with `TAG_SCOPE_ATESPACE` | Requires a stopped actor; never suspends implicitly. A retry adopts its own earlier tag |
| `branch(checkpoint, name)` | `CreateActor` with `source_tag` | New name, UID and Substrate-issued identity. The child starts stopped |
| `revert` | `RevertActor` | Returns to the **latest** suspend only; not restore to a chosen checkpoint |
| `destroy` | `DeleteActor` with a UID precondition | Idempotent; cannot delete a newer actor that reused the name |

## Declared capabilities

| Capability | Declared | Reason |
|---|---|---|
| Exec with process pipes | No | `ateapi` has no exec or attach call |
| Routed ingress | Yes | `atenet-router` activates suspended actors on request |
| Checkpoint | Template scope, crash consistency, portable | Suspend captures without the workload's cooperation; object storage makes it portable |
| Branch | Same as checkpoint | Tag, then create from tag |
| Restore to a chosen checkpoint | No | Only revert-to-latest exists; use `branch` instead |
| Share | No | No API for state shared between actors |

`FULL` commit scope maps to a full snapshot. `DATA` maps to a disk snapshot: resuming it cold-boots or restores the template's golden snapshot, so no process state from the source actor survives. An unset scope uses the API's documented default, `FULL`. An unrecognized scope value is an error, not a guess.

## Gaps and required decisions

1. **No exec.** M2 requires exec with independent pipes. A Substrate profile must instead run an in-guest endpoint, such as ACP over HTTP, reached through the router. That driver does not exist yet, and routing to it needs authenticated sandbox-to-route mapping (see [design §7](design.md#7-storage-and-networking-without-new-infrastructure-projects)).
2. **Secrets inherited by branches.** A full-scope branch copies the source's memory and root filesystem. Use Substrate's egress credential injection so secrets never enter the guest, or checkpoint before any credential is present. Otherwise the design's "no secret inheritance" gate fails.
3. **No quiescence handshake.** Checkpoints are crash-consistent. A harness must reach a quiet point (no in-flight tool call) before `stop`. The adapter enforces only that the actor is stopped.
4. **Fencing covers delete only.** Suspend, resume and revert take a name without a UID precondition. `inspect` detects replacement, but a check followed by an action can still race.
5. **Kubernetes.** Substrate requires a cluster. It stays optional; Kubernetes is not on Branchyard's default per-spawn path.
6. **Fork is upstream work.** Substrate lists actor forking from a state root on its roadmap. Revisit `branch` when it lands.

## Qualification steps

1. Install Substrate on kind (`hack/create-kind-cluster.sh`, `hack/install-ate-kind.sh`) at the pinned revision.
2. Run the adapter against its `ateapi` endpoint with TLS and credentials configured by the caller's channel.
3. Measure create, resume, suspend, tag and branch separately, cold and warm. Record them as the implementation plan describes.
4. Verify that tag readiness, UID preconditions and egress policy behave as the fake assumes. Correct the adapter or the declaration where they differ.
5. Update [validation](validation.md) with the exact revision and results before claiming support.
