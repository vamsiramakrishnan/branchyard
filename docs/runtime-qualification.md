# Runtime qualification gate

Use the existing Microsandbox runtime rather than building virtualization into
Branchyard. The current reference is [Microsandbox v0.7.0](https://github.com/superradcompany/microsandbox/releases/tag/v0.7.0);
reviewed source commit `a9da8ff79881deb63bc1bf85abedb63d54a1916c`.
The open runtime and its local Rust SDK are the target; hosted beta access or
hosted pricing is not an execution dependency.

## Preflight

On the intended Linux execution node, provision the matching runtime through its
upstream release procedure and verify its published checksums. Then run:

```sh
python3 tools/runtime_preflight.py --output /path/to/new-preflight.json
```

The script checks Linux, accessible `/dev/kvm` and `msb --version` matching 0.7.0.
It refuses to overwrite an existing receipt. Exit 2 means prerequisites are missing;
exit 0 means ready to attempt qualification, never qualified. The implementation
workspace reports no KVM device and no runtime. No latency claim follows from that.

The upstream [Rust SDK](https://github.com/superradcompany/microsandbox/tree/v0.7.0/sdk/rust)
ships local execution together with optional cloud, keyring and automatic binary
provisioning features. For a server node, start with `default-features = false`
and explicit `local`/`net` features, and provision matching runtime binaries
outside the request path. Do not add it to the public SDK crate: virtualization
is an execution-node dependency. Validate this feature selection on the target
node before locking it into the workspace.

## Evidence required before a provider is enabled

| Probe | Required evidence |
|---|---|
| Create, inspect, destroy | Stable allocation identity; explicit lifecycle result; no allocation remains after verified teardown |
| Lost create response | Discover/reconcile the original allocation without blindly creating a replacement |
| Guest execution | Separate stdin/stdout/stderr, exit status, cancellation and bounded output under backpressure |
| Isolation | Two sandboxes have private writes; host and sibling paths/credentials are unavailable |
| Network | Allowed traffic succeeds; denied destinations fail; policy persists across reconnect |
| Resources | CPU/memory ceilings and deadline termination are enforced, including abnormal exits |
| Snapshot/source reuse | Immutable source identity, private writable layer and no inherited guest secrets |
| Host/worker failure | Restart discovery and stale ownership behavior are observable, not assumed |
| ACP transport | Bidirectional JSON-RPC over guest pipes while permission/tool callbacks remain serviced |

The runtime CLI's `exec --stream --no-tty` path is useful for probing a separate-pipe
transport. Branchyard's eventual driver should use the maintained SDK/protocol
client; terminal prompt matching is not a substitute for ACP. A successful shell
command alone does not qualify any model harness.

Record CPU, kernel, runtime/SDK revision, image digest, storage backend, network
policy and declared concurrency alongside each result. Measure allocation,
source attachment, guest readiness, protocol handshake and first useful action
separately. Compare cold, cached and warm pools under load; do not report the
runtime's marketing latency as Branchyard latency.

## Implementation after qualification

Add a node crate with a narrow internal allocation/inspection/exec/teardown
boundary based on these receipts. Keep task, attempt, workspace and sandbox IDs
separate. Persist attempt generation before dispatch and require it on every
publication and release. PGMQ redelivery triggers reconciliation; it cannot alone
authorize repeating an uncertain allocation or model prompt.

Then run one version-pinned ACP harness with its provider credential supplied by
a server secret mechanism. Add automatic attenuated child credentials only after
that path can survive disconnect and cancellation. Prove a live child spawning a
grandchild before broadening to the sixteen-harness matrix. Candidate integration
remains a separate fenced validation and Git compare-and-swap mechanism.
