# Cloud execution and console presets

Written 2 November 2026.

Choose an execution provider independently of where the Branchyard worker runs.
A GCE worker can use an owned gVisor container or a Microsandbox MicroVM. A worker
with an explicit kubeconfig context can use Kubernetes SIGs Agent Sandbox on
GKE's managed gVisor or Kata Cloud Hypervisor pools.

## Prepare the runtime

Use [GCE setup](compute-gce.md) with `--gvisor` to install the pinned gVisor
Docker runtime and add the dedicated SSH worker to Docker's group. Setup
configures the worker through a new login so that membership takes effect.
Installation preserves Docker's default runtime. The native container
provider explicitly requires `runsc` and refuses a fallback to `runc`.

Use `--backend microsandbox` for the [patched MicroVM runtime](microsandbox-runtime.md).
Its release manifest identifies the archive and provenance, and the installer
verifies them against the same release's checksums. Runtime directories are
immutable and homes are specific to the artifact. Old checkpoints require their
original runtime; incompatible references are refused without deleting them.

For GKE, review the offline plan from [Agent Sandbox setup](agent-sandbox-gke.md),
then apply it. It creates dedicated managed runtime pools, installs the pinned
SIGs controller and enforces runtime, non-root and network policies. An existing
Substrate cluster is a separate deployment.

## Run from the CLI

Native images must use an OCI digest and provide the harness plus the tools in
the [container image contract](container-sandbox.md). Replace `IMAGE@sha256:DIGEST`
with your image's complete immutable reference. Container images must seed
`/workspace` and `/branchyard/home` as writable by UID/GID 1000.

```sh
by run --provider container --image IMAGE@sha256:DIGEST --cpus 1 --memory 1024 'TASK'
by run --provider agent-sandbox --kube-context CONTEXT --kube-namespace branchyard-gvisor --image IMAGE@sha256:DIGEST 'TASK'
by run --provider agent-sandbox --kube-context CONTEXT --kube-namespace branchyard-microvm --sandbox-runtime microvm --image IMAGE@sha256:DIGEST 'TASK'
```

These providers copy the worktree and private home, return worktree changes, and
destroy the entire allocation after each turn. Follow-ups create a fresh
allocation from the returned files. They offer no retained VM, pause or runtime
snapshot. A failed worktree return fails the turn.

Networking defaults to `deny`. Model access requires an explicit networking
choice and provisioned model credentials. `--sandbox-network unrestricted`
enables Docker bridge networking for Container; on the qualified GCE host this
also reached instance metadata. GKE's choice enables IPv4 Internet access and
cluster DNS while continuing to deny metadata, ingress and IPv6 Internet access.
Neither choice implements Branchyard's required proxy-egress policy.

## Select through the console

An operator writes an [execution preset](server.md#execution-presets) and adds
its provider kind to `allow_providers`. Container uses kind `container`; Agent
Sandbox uses `agent_sandbox`. Console callers choose the preset ID. Direct API
requests cannot choose these providers, even when their kinds are allowed.

For a GCE container preset, configure this provider object, with your complete
image digest:

```json
{"kind":"container","image":"IMAGE@sha256:DIGEST","runtime":"gvisor","network":"deny","cpus":1,"memory_mib":1024}
```

For a GKE gVisor preset:

```json
{"kind":"agent_sandbox","context":"CONTEXT","namespace":"branchyard-gvisor","image":"IMAGE@sha256:DIGEST","runtime":"gvisor","network":"deny"}
```

For Kata, select `namespace: branchyard-microvm` and `runtime: microvm` in the
operator's provider object. For GCE Microsandbox, use the existing `microsandbox`
provider object with the patched runtime installed on its worker.

The [admin compute console](admin-console.md) prints these preset entries, with
the setup commands and worker label, and shows which live workers can claim each one.

Preset harness choices, tenant grants, secrets and worker labels still apply.
Label GCE presets for their GCE worker and GKE presets for workers with the
intended kubeconfig. Discovery reports native runtime and image readiness as
unknown until a session starts. It does not infer guest harness availability
from an executable installed on the API server. Session errors project the
configured image, Kubernetes context and namespace to fixed placeholders.

## Evidence

The dated [GCE container record](evidence/gce-container-2026-11-02/README.md)
contains live gVisor isolation, CPU throttling, exact memory-limit OOM and
default-deny network observations. The [GCE MicroVM record](remote-validation-2026-11-02-gce.md)
records actual runtime RAM and adapter checks. Read each record's `Not done:`
clause before interpreting a capability as tested behavior.
