# Remote execution validation, 28 October 2026

This record tracks validation of commit `f0f7bffa` on real remote compute.
Results cover that baseline and the working-tree fixes described below.
Model calls run separately from automated tests.
Not done: the Microsandbox hard memory ceiling, bubblewrap under the host's high
OS Login UID, and Agent Compute Engine preview testing. Further boundaries are
recorded with each result.

## Source and host

- The original `execution-node` checkout is preserved in the stash named
  `pre-main-remote-validation-2026-10-28-execution-node`, including untracked files.
- `main` was fast-forwarded to `origin/main` at `f0f7bffa`.
- The old ignored Terraform state was preserved outside the clean checkout in
  `/tmp/branchyard-remote-20261028/preserved-terraform`. It is private operational
  state, not evidence to commit.
- Live inspection found `branchyard-validation-01` in `vital-octagon-19612`,
  `us-central1-a`, running Ubuntu 24.04.4 on an `n2-standard-4` with nested
  virtualization and `/dev/kvm`. SSH over its private address works.
- The host's older Branchyard services use ports 8787 and 5432. New validation
  uses separate files, runtime state, and service ports.
- Private command logs are under `/tmp/branchyard-remote-20261028/logs` on the
  operator workstation. Redacted results belong in this document.

## Coverage being exercised

The provider implementations in this checkout are local processes, Microsandbox,
Agent Substrate, and recipes. Tests also cover isolated homes, HTTP and SSH remote
access, PostgreSQL state, and local network confinement. Bubblewrap and Landlock
are host or harness facilities, not selectable Branchyard provider variants.

Agent Substrate is pinned to `1d7ca8ced056192a1801d6565251adcaab3eb0c9`.
A dedicated kind cluster runs on GCE. A separate Standard GKE cluster,
`branchyard-substrate-qual-20261028`, runs Kubernetes
`1.37.1-gke.2097000` on an `n2-standard-8` node with nested virtualization.
Its nodes are private, Workload Identity is enabled, and it uses a dedicated
node service account, registry and snapshot bucket. Existing clusters were not
changed. Google Agent Compute Engine is excluded because preview access is
unavailable; neither GCE nor Substrate is presented as ACE.

## Developer setup friction

The implementation now uses the existing Rust `clap` and `cliclack` stack for
guided setup, confirmations, progress and CLI help. `by up` and `by doctor`
share provider readiness checks; `up` rechecks actionable steps before applying
them. Saved Substrate configuration resolves credential paths relative to its
source file and flows through CLI and SDK task defaults. Remote readiness
checks credential delivery to the actor rather than treating a workstation
login or local harness installation as proof of remote readiness.

`by substrate setup --plan` reports immutable template changes without writing
a signing key. Apply reuses matching resources and refuses conflicts.
`by substrate check --smoke --harness agy` names failing phases, bounds executable
probes, and attempts actor cleanup on errors and interruption. The
[GKE helper](substrate-gke.md) supplies an offline plan and resumable provisioning;
the [image recipe](substrate-setup.md#prepare-the-image-and-template) includes
the bridge, agy and startup ownership preparation.

| Observed failure | Cause and workaround | Evidence |
|---|---|---|
| `cargo build --locked --offline` could not download `crypto-common v0.1.6` | A fresh checkout does not have every locked crate cached. Run `cargo fetch --locked` with network access before offline checks. | Dependency fetch completed. |
| Cargo could not unpack `ahash` into its cache | This agent's workspace sandbox makes `~/.cargo` read-only. Permit the build to write its existing cache. This is an execution-environment restriction, not a Rust error. | The build proceeded after permission. |
| `tools/check.py --fast` reported `No module named ruff` | The selected Python needs `ruff==0.15.8` and `mypy==2.4.0`. Use a dedicated environment and invoke the checks with that environment's Python. | Baseline fast-check log. |
| Python SDK tests failed to open loopback sockets with `Operation not permitted` | The agent sandbox blocks sockets. Run these tests with local socket access. Do not count these environmental failures as SDK regressions. | Baseline ran 96 SDK tests and reported 82 socket setup errors. |
| `gcloud` could list its account but could not query compute | Credential refresh needs write access to its config database. Permit `gcloud` access to that directory. | VM and cluster inventory succeeded after permission. |
| The Microsandbox guide refers to 14 ignored tests | The source also includes live-branch and persisted-resume tests. Discover test names from the built executable before recording totals. | `crates/branchyard-microsandbox/tests/microsandbox.rs`. |
| The backend-enabled build failed at final link with `unable to find library -lcap-ng` | Install `libcap-ng-dev` before a Microsandbox build. The compiler can spend several minutes on dependencies before revealing the missing system library. | First build log. |
| `unshare -Urn true` failed to write `uid_map`; bubblewrap failed `RTM_NEWADDR` | Ubuntu's AppArmor `unprivileged_userns` profile denied `sys_admin`, `setpcap`, and `net_admin`. KVM access alone does not qualify local network confinement or bubblewrap. | Remote kernel audit log, 03:00 UTC. |
| The live Substrate instructions omit an installer action | At the pinned revision, install with `hack/install-ate-kind.sh --deploy-ate-system`. The script also requires Go 1.27 and builds its pinned kind tool on first use. | Read pinned upstream quickstart and scripts; remote cluster creation completed. |
| A new documentation page failed the index test | Every root-level documentation page must be linked from its shelf in `docs/README.md`. | Added the link; all seven index tests and documentation link checks passed. |

## Results

Live on 28 October 2026, on the GCE host above: the local-provider conformance
executable passed 17 tests. Scripted fake-ACP write/check/diff/merge probes passed
in local and isolated-home modes; these were not model runs. A separate
Antigravity `agy` 1.4.8 run used a private home and the API-key provisioner,
wrote `result.txt` containing `remote-agy-ok`, passed `python3 verify.py`,
produced the expected diff, and merged. Its private record is
`branchyard-qual-20261028/agy-live-hesy52es` on the host. No Gemini CLI model
run was used. Not done: this local run alone does not qualify remote providers.

Live PostgreSQL 16 on GCE: all 19 server tests passed against a dedicated
container through an SSH tunnel. The server/test process ran on the workstation;
the database ran remotely. Record: `logs/postgres-tests.log`, 380.88 seconds.
Not done: distributed or multi-tenant PostgreSQL deployment qualification.

Live Antigravity `agy` 1.4.8 also passed write, configured validation, an explicit
`by check`, diff and merge through four remote workflows: a directory-backed
recipe on GCE, a recipe over actual SSH from the workstation, an HTTP server on
the host's dedicated port 8789, and the native SSH remote server. Recipe destroy
removed the owned workspace; server stop removed the owned process and socket.
Records: `logs/agy-backends/` and `agy-backends-result.md` in the private evidence
directory. Not done: container-backed recipes, recipe suspend/resume, HTTP TLS,
or multi-tenant server qualification. The recipes used private directories on
the existing host and make no VM isolation claim.

Live Microsandbox 0.7.3 on GCE KVM: the complete suite now passes 17 of 18
checks, including disk checkpoints, live process preservation, separate child
workspaces, paused-source preservation, strict rejection of changed open files,
and cleanup after a rejected restore. The adapter uses the SDK's immutable
checkpoint reference and full checkpoint restore with explicit child mounts.
Temporary capture uses its own snapshot group; saved source checkpoints remain
intact. Paused guests are forcibly removed during teardown. Redirecting a
background test process's inherited descriptors also removed a 300-second wait.
Records: `logs/msb-full-restore-suite-final.log`, 23.06 seconds, and the
independent `logs/msb-root-verification.log`, 24.01 seconds.
The remaining memory failure comes from the upstream memory layout:
Branchyard and the SDK pass 512 MiB unchanged, and `msb_krun_vmm` 0.1.39 converts
it to bytes with `<< 20`. Its x86_64 `KernelMmap` path calls
`msb_krun_arch::x86_64::arch_memory_regions`, which places the full requested
region after low memory and the separately mapped kernel when RAM fits below
the MMIO gap. The guest's e820 RAM map includes that extended range; upstream's
`regions_lt_4gb` test also asserts this layout. The SDK's `max_memory` reserves
hotplug capacity, and its runtime keeps raw kernel arguments internal; neither
provides an adapter-level total-memory cap. This source trace explains why the
limit can be exceeded, but does not derive the exact guest-kernel reservations.
Not done: the hard memory ceiling still fails. The 512 MiB request exposed
529224 KiB rather than at most 524288 KiB; the assertion was not loosened and no
empirical offset was subtracted from the request.

Live Antigravity `agy` 1.4.8 inside Microsandbox on the same GCE host passed
write/check/diff/merge with two vCPUs and 2048 MiB requested. The image included
git, Python and agy and was imported through `msb image load` from a Docker
archive. Record: the host's `branchyard-qual-20261028/agy-msb-joay8try` and
`logs/agy-msb.log`. Not done: this successful task does not resolve the separate
memory-ceiling failure.

Live host isolation: Landlock ABI 8 allowed an authorized write and refused a
write outside it, preserving the sentinel. A named AppArmor profile allowing
user namespaces made `unshare -Urn true` and local network-confinement tests
pass. Bubblewrap 0.9.0 succeeded as UID 65534 with separate namespaces and a
read-only root, but still failed UID mapping for the high numeric GCP OS Login
user. Record: `logs/bubblewrap-low-uid.log`.
Not done: a successful bubblewrap execution under that high numeric UID.

Hermetic checks for the new Substrate integration pass through authenticated
gRPC, header-routed health/WebSocket traffic, real bridge processes, immutable
template setup, token rotation, forwarded TLS naming, and per-actor egress.
These prove the local protocol fixtures. Not done: these fixtures alone do not
establish real Substrate or GKE behavior.

Live Substrate on GKE passed `by substrate check --smoke --harness agy` with
both gVisor and microVM. The smoke creates an actor, transfers a file, executes
the bridge and agy version probe, and removes the actor. The exact shipped
Dockerfile and startup script built the image with digest
`sha256:5a0b11f087e543648a4f38a26c55fd472aa0e0e5043159998c035c375d01fed7`.
Not done: smoke checks do not call a model.

Live on 28 October 2026, all four Substrate environments passed separate real
agy 1.4.8 tasks and the complete lifecycle suite below. The model tasks ran
concurrently with the same branch name in separate repositories, completed
write/check/diff/merge, and received distinct actor names. They used `by` SHA-256
`26a542177470e17809089b56c63689a8d751205aa8f5ce73331043d661965b2b` and the
image digest above. The lifecycle suite separately exercised provider
conformance, superseded credential rejection, pause/resume, checkpoint creation,
branching with independent file state and actor identity, worktree round trip,
and cleanup. Not done: a live-fork capability (Substrate does not expose one),
production ingress/TLS deployment, or live host-allowlist enforcement; model
qualification explicitly used unrestricted actor egress.

| Environment | Runtime | Real agy task | Lifecycle checks | Lifecycle duration |
|---|---|---|---|---|
| GCE, kind Kubernetes 1.37.0 | gVisor | Passed | 4/4 passed | 10.88 s |
| GCE, kind Kubernetes 1.37.0 | microVM | Passed | 4/4 passed | 19.81 s |
| GKE 1.37.1-gke.2097000 | gVisor | Passed | 4/4 passed | 13.15 s |
| GKE 1.37.1-gke.2097000 | microVM | Passed | 4/4 passed | 21.40 s |

The final records are in `remote-evidence/logs/model-*-final.log`,
`remote-evidence/logs/cluster-*-final.log` and their matrix JSON files under the
private evidence directory. Model records are `agy-kind-gvisor-ewkurl6a`,
`agy-kind-micro-yj3vvmvs`, `agy-gke-gvisor-n0sbzjj4` and
`agy-gke-micro-wvcpi39y`; their redacted command output is collected there too.
`by up --plan --json` returned ready for each saved connection, and a terminal
run displayed progress and explicit limits on what readiness proves. Record:
`logs/up-live.log`. Not done: `up` itself does not execute a model task.

Hermetic and local verification on 28 October 2026 passed the full fast gate,
affected CLI suites, embedded skill and projection checks, provider and setup
tests, privileged bridge identity regressions, formatting, schema freshness,
Clippy and rustdoc for affected crates, and the workspace missing-docs ratchet.
The new GKE helper and image wrapper have hermetic tests; the exact startup
wrapper also passed repeated real Docker execution using only `CHOWN`, `SETUID`
and `SETGID`, including exclusion of another unprivileged UID. Records are in
`logs/fast-final-2.log`, `rust-check-results.json`,
`logs/clippy-identity-final.log`, `logs/missing-docs-final.log` and
`logs/substrate-image-bootstrap-docker.log`. Not done: a fresh complete
workspace Cargo test run, or Codex/Claude OAuth model qualification.

## Additional observed friction

| Observed failure | Resolution or current boundary |
|---|---|
| Conda compiler could not find installed libcap-ng | Use the system compiler PATH for the native build. |
| Microsandbox Unix socket path exceeded its maximum | Use a short private `MSB_HOME`; long OS Login home paths trigger this before actor startup. |
| Upstream kind installer failed on `docker buildx` | Install Docker's buildx plugin before runtime installation, then rerun. |
| Existing GKE Autopilot clusters were Kubernetes 1.35 | Create a separate compatible Standard cluster; the pinned runtime requires certificate APIs from 1.36 with explicit enablement or 1.37+. |
| Control client lacked normal bearer authentication | Added per-request token-file loading, rotation and redacted failures; mTLS remains available. |
| TLS forward used the loopback name instead of the service identity | Added verified `server_name`; no insecure downgrade. |
| Fake router hid the real header contract and guest port | Client now supplies `ate-target-actor` on health and WebSocket; generated bridge listens on port 80. |
| Actors could not reach model endpoints without egress policy | Reconcile an explicit actor policy, including snapshot children; refuse mismatched existing policies. |
| Substrate could not be saved in project configuration | Added saved configuration, source-relative credential paths, setup interview and CLI/SDK defaults. |
| Up treated a local OAuth login as remote agy readiness | Check the remote provisioner's accepted secret sources and report model authentication as unverified. |
| Template creation required invoking an ignored test | Added `by substrate setup`, immutable comparison, safe key reuse and explicit `check --smoke`. |
| GKE installer ignored supplied kubeconfig and tried gcloud login | Set the exact `KUBECTL_CONTEXT` as well as `KUBECONFIG`. |
| gcloud refused combined node-management and node-label updates | Apply these as separate node-pool updates. |
| GKE 1.37 served only certificate API v1, while the pinned controller needs v1beta1 | Explicitly enable both `podcertificaterequests` and `clustertrustbundles` beta APIs and verify discovery before installing. Version alone is insufficient. |
| A Running worker pod was not selectable by the actor template | Put workload labels in `WorkerPool.spec.template.labels`; metadata labels alone do not reach worker pods. Set explicit worker capacity. |
| The bridge could not act as its unprivileged harness user | Substrate's default capabilities omit `SETUID` and `SETGID`. The template now requests them, and inspection rejects templates missing them. `CHOWN` permits startup preparation of the actor's private workspace and home. |
| File transfer still failed under the pinned gVisor after adding capabilities | Its `setfsuid` and `setfsgid` syscalls return `ENOSYS`. The bridge now falls back to per-thread effective identity changes only for that error, clears supplementary groups and effective capabilities during access, and restores the captured identity afterward. Privileged syscall-filter regressions and live smoke checks passed. |
| The provisioning helper could not verify an existing bucket's project | Normal `gcloud storage buckets describe` omits project identity. Read `--raw` API metadata and retain the project ownership check. |
| A valid image became inaccessible to the harness after extraction | The runtime produced a root directory with mode 0700 and root-owned workspace directories. The shipped startup script prepares root traversal and private workspace/home ownership before starting the bridge. Its Docker probe uses only `CHOWN`, `SETUID` and `SETGID`. |
| `install -d -o ... -m ...` failed without `CAP_FOWNER` | Prepare directories as root, set their mode while root owns them, and transfer ownership last. No `FOWNER` or discretionary-access override capability is needed. |
| A previously working port-forward stopped during testing | Both router forwards exited with `lost connection to pod` after a TCP reset. The qualification fixture used private direct TCP relays; each survived eight deliberate resets. Final model and lifecycle runs passed through these relays. Keep a managed ingress path for durable use; a stale forward can resemble a slow actor. |
| Long validation outlived credentials | Refresh the GKE client credential and replace the Control token file. A configured credential path alone does not prove the token remains valid. |
| Concurrent repositories allocated the same actor name in one millisecond | Allocate a fresh ULID per sandbox, retain it in the journal, and check template identity before adopting an actor during recovery or cleanup. A failed UID-fenced delete retains that UID for retries. |

## Cleanup

Live cleanup on 28 October 2026 removed the dedicated GKE cluster and nodes,
Artifact Registry repository, snapshot bucket, and test node service account
with its project binding. It also removed the GCE kind cluster and registry,
the test PostgreSQL container, router relays, SSH forwards, temporary host route,
test AppArmor profile, private runtime homes and qualification credentials.
The existing GCE host, its original PostgreSQL container, and listeners on
ports 8787 and 5432 remain. Existing networks, NAT, other clusters, user OAuth
credentials, the original stash and preserved Terraform state were retained.

Redacted evidence remains under `/tmp/branchyard-remote-20261028`, including
`remote-evidence/`, `host-cleanup.json`, cloud deletion logs and empty
`remaining-*.json` resource inventories. Build test executables created during
this run were removed; the current CLI build remains in `target/debug/by`.
Not done: the removed test environments are no longer available for interactive
reuse; recreate dedicated resources with the documented helper and image recipe.


## Published installation and fresh GCE setup, 28 October 2026

Live installation checks downloaded the public `v0.1.0` installer and checksums
from GitHub's latest release. In a clean home with no Rust toolchain on `PATH`,
it installed and version-probed `by`, the server and bridge, including a prefix
with spaces. Hermetic AGY readiness fixtures then exercised guided headless setup,
repeat installation and unchanged project configuration on repeat setup.
The published AGY 1.4.9 worker image also passed an anonymous registry pull check
against its release-pinned digest. Evidence is under
`/tmp/branchyard-install-20261028/public-install-3yjvz0gu`,
`public-install-result.json` and `public-image-result.json`.
Not done: those readiness fixtures do not establish a live model conversation.

A live run of the patched GCE helper finished at 12:56:28 UTC against a fresh,
owned `n2-standard-4` worker in `us-central1-b`. The worker had no service account,
used its private address, and enabled nested virtualization. It installed the
published 0.1.0 native binaries, extracted AGY 1.4.9 from the immutable release
image, verified Microsandbox 0.7.3, and imported that worker image. Native project
and server configuration, config validation, `serve --check` and the managed SSH
alias probe passed. No provider credentials were forwarded and no model ran.
The exact command, timestamps and exit status are in
`/tmp/branchyard-install-20261028/gce-qual-published/private-b-final-apply-result.json`;
`private-b-state/deployment.json` and `private-b-state/install.log` hold the setup
record. The new native CLI forwarding is also tested hermetically.
Not done: this run used the patched source helper with published 0.1.0 binaries;
it does not qualify a 0.1.1 binary, a model conversation or the MicroVM hard
memory ceiling.

Live follow-up probes also passed: `by --remote ssh://… ls --json` connected
through the managed alias, started the server and authenticated, returning an
empty branch list. Microsandbox 0.7.3 ran a real guest command that printed
`microsandbox-exec-ready`, kernel 6.12.109 and bridge 0.1.0 (protocol 2);
the unique test sandbox was removed afterwards. The dated records are
`native-remote-ls-result.json` and `private-b-raw-msb-result.json` beside the
setup evidence. Not done: model execution and the MicroVM hard memory ceiling.

Fresh setup exposed these additional friction points:

- Cloud SDK 590.0.1 reports an absent VM as `The resource ... was not found`.
  The helper now recognizes that form while refusing permission failures,
  including diagnostics that also mention a missing resource.
- Direct external SSH is blocked in this environment. The helper's private
  route worked. Guided setup now explains transport prerequisites, and
  `by compute gce` exposes planning and explicit application without a checkout.
- The managed `gceenforcer-enforcer@system.gserviceaccount.com` deleted the
  owned IAP SSH allow rule. Cloud Audit Logs show successful deletions at
  12:31:15–20 UTC. The remaining ingress policy blocks IAP. The helper exposes
  IAP explicitly and checks owned rule configuration; the guide does not claim
  it was qualified here. No managed policy was bypassed.
- `us-central1-a` returned `ZONE_RESOURCE_POOL_EXHAUSTED_WITH_DETAILS` before
  creating a VM. The helper now gives a capacity remedy. The operator explicitly
  selected `us-central1-b`; the helper did not silently change the footprint.
- A new VM briefly refused SSH public-key authentication before OS Login was
  ready. Initial connection readiness now retries within a deadline while
  authorization failures remain actionable.
- Adding the user to `kvm` does not refresh the current SSH process's groups.
  The installation and native configuration now run in separate SSH sessions.
  A fresh session accepted Microsandbox and generated its native project config;
  the wizard's capability checks remain unchanged.

Status: live installation and cloud setup evidence is recorded above; CLI,
transport selection, IAP ownership checks and failure handling also have hermetic
regressions. The owned worker was retained temporarily for the public verification below.
Not done: IAP connectivity under this managed policy, a new live AGY model run,
and the MicroVM hard memory ceiling.


## Public 0.1.1 verification, 28 October 2026

Live post-publication checks installed 0.1.1 from GitHub's public latest release,
verified its bootstrap checksum, and probed the CLI, server and bridge versions.
A clean-home installation with a prefix containing spaces passed guided
headless setup, reinstall and unchanged repeat configuration. AGY readiness in
that local fixture was hermetic. The published worker image passed an anonymous
registry check against its immutable release digest. Evidence is
`/tmp/branchyard-install-20261028/public-install-0.1.1-result.json` and
`public-image-0.1.1-result.json`. Not done: a model conversation in that fixture.

Live public-CLI provisioning then completed at 14:11:36 UTC in `us-central1-b`.
The published standalone `by` executable ran `compute gce --apply` from an empty,
non-Git directory, using its packaged helper rather than a checkout or a helper
beside the executable. It created a fresh private `n2-standard-4` worker with no
service account and nested virtualization. Installation and native configuration
passed for by/server/bridge 0.1.1, AGY 1.4.9 and Microsandbox 0.7.3. KVM access,
`serve --check`, native SSH auto-start/authentication and a real MicroVM guest
command passed; the guest printed kernel 6.12.109 and bridge 0.1.1 (protocol 2).
The MicroVM test sandbox was removed. No model ran or provider key was forwarded.
Not done: the MicroVM hard memory ceiling and live model execution.

The first public-CLI attempt met real zone capacity exhaustion and returned the
shipped actionable remedy before creating a VM. The operator released the older
owned qualification worker, then retried the same footprint in the same zone;
initial SSH readiness completed without manual intervention. The exact source-free
proof, command results, deployment metadata, version probes and MicroVM output are
under `/tmp/branchyard-install-20261028/public-gce-0.1.1`, including
`source-free-proof.json`, `retry-b-result.json`, `native-remote-ls-result.json`
and `raw-msb-result.json`. Not done: IAP under the managed firewall policy.


Live cleanup completed at 14:17:28 UTC. Both owned installation workers and
their auto-delete boot disks were removed; no owned IAP firewall rule remains.
The temporary MicroVM sandboxes and native servers were stopped, owned SSH
aliases and local connection state were removed, and the known-host files
matched their original baseline hashes. The original validation host remained
running. The socket-only PostgreSQL test instance was stopped and generated test
executables were removed while retaining the standalone binaries and evidence.
Records are `public-proof.json` and `cleanup-proof.json` beside the public GCE
evidence. Not done: model execution, the MicroVM hard memory ceiling, and live
IAP under this managed policy; temporary workers are no longer available for
interactive reuse.
