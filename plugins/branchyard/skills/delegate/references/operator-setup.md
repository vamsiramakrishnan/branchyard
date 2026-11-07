# Delegate reference: Substrate setup, readiness and native sandbox providers

Read when: an operator sets up Substrate, a compute worker, a native container or Agent Sandbox provider, or readiness checks.

## Substrate setup and readiness

`by substrate setup --image IMAGE --storage URI --plan` previews the saved
Substrate template setup; `--yes` applies it. `by substrate check --smoke
--harness agy` verifies bridge execution, file transfer and cleanup without a
model call. These are operator commands, run outside a harness session.
`by up` and `by doctor` check the saved connection and remote credential sources.
They report seven readiness checks; only a verified one was established by that run.
`by up --plan --smoke` also lists Substrate workers, creating no actor and calling no model.
An operator runs `by setup --guided` to choose a repository and compute backend.
`by setup --up --json` includes readiness; `--directory PATH` selects a repository.
An operator can plan a dedicated worker with `by compute gce --project PROJECT
--zone ZONE --name NAME --tunnel-through-iap`; `--apply` provisions the reviewed
VM and its dedicated IAP TCP 22 firewall rule. `--internal-ip` also omits the
external address. `--service-account EMAIL` attaches that account with the
`cloud-platform` scope, so its IAM roles decide API access; `--scopes` narrows it.
This command refuses harness sessions and `--remote`.
`--gvisor` also installs the pinned gVisor Docker runtime. Container execution
requires permission to use the worker's local Docker socket.
`by run` accepts `--substrate-api-token-file PATH` for Control authentication and verified
forwarded TLS accepts `--substrate-server-name NAME`. Actor egress is explicit:
`--substrate-egress deny`, `--substrate-egress unrestricted`, or repeatable
`--substrate-allow-host HOST`. Saved `[substrate]` configuration avoids repeating
connection flags. See the Substrate setup documentation for image and pool prerequisites.
Operators also run `by harnesses update ID --qualify` (checks a release before
pinning it), `by images build` and `by images pin REF@DIGEST` (digest-pinned
harness images) and `by startup report` (launch time by provider), never from
inside a harness session.
For GKE, an operator can use `tools/substrate_gke.py --atenet-dataplane=agentgateway`
with the required target options. This pins a compatible gateway before installation
from a private source export; the default Envoy path is unchanged.

## Native sandbox providers

An operator starts a native allocation with `by run "TASK" --provider container
--image IMAGE@sha256:DIGEST`, or uses `--provider agent-sandbox` with
`--kube-context CONTEXT --kube-namespace NAMESPACE`. The image supplies the
harness. Both accept `--cpus`, `--memory` in MiB and `--pass-env` variable names.
`--sandbox-runtime gvisor` is the default; `--sandbox-runtime runc` applies only to
Container and `--sandbox-runtime microvm` applies only to Agent Sandbox.
`--sandbox-network deny` disables external networking;
explicit `--sandbox-network unrestricted` permits ordinary networking. Container permits cloud
metadata; Agent Sandbox retains its GKE metadata blocks.
Native allocations are destroyed after each turn and refuse `--keep-sandbox`,
snapshots, host mounts and required hostname allowlists. On a server, use its
approved execution preset; a preset does not permit client provider overrides.
