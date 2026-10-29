# Install Substrate on GKE

`tools/substrate_gke.py` prepares a dedicated GKE Standard cluster, a snapshot
bucket and an Artifact Registry repository, then runs the pinned upstream
Substrate installer and starts a dedicated worker pool. Its default is an offline JSON plan. Add `--apply` to create
or adopt resources and install Substrate. Cluster provisioning is separate from
`by up`; finish the connection, bridge image and template using
[Substrate setup](substrate-setup.md).

The supported source revision is
`1d7ca8ced056192a1801d6565251adcaab3eb0c9` from
[Agent Substrate](https://github.com/agent-substrate/substrate/tree/1d7ca8ced056192a1801d6565251adcaab3eb0c9).
The helper refuses a different or dirty checkout. It uses upstream
`go run ./cmd/ate-setup` and `hack/install-microvm-deps.sh`; it does not reproduce
the Substrate manifests or use upstream's project-wide IAM grants.

## Prepare the operator machine

Use a machine with enough disk for Go builds, container images and microVM
assets, authenticated Google Cloud access, and a reachable Kubernetes API.
Install these tools from their official distributions:

- Go 1.27 or newer, Git and Bash.
- Google Cloud CLI with support for nested virtualization and
  `--enable-kubernetes-unstable-apis`, plus `gke-gcloud-auth-plugin` when using
  its generated kubeconfig.
- `kubectl`. The default Envoy dataplane also needs a running Docker daemon and Docker's buildx plugin.
- `curl`, GNU tar and `zstd` for upstream's pinned microVM assets.

The helper checks tools before touching cloud resources. It does not install
missing software or download a newer Go toolchain automatically. With the default
Envoy dataplane, Docker alone is insufficient: its image build uses `docker buildx`.
Use `--atenet-dataplane=agentgateway` for the prebuilt gateway path without a Docker
daemon. The controllers and workers still build from the pinned Go source.

Prepare an existing Google Cloud project with billing and the Compute Engine,
GKE, Cloud Storage and Artifact Registry APIs enabled. Use an existing dedicated
node service account in that project, with the node permissions required by
GKE, such as `roles/container.defaultNodeServiceAccount`. The operator needs
permission to create the named resources, use that account, change their IAM
policies, push images, and administer this Kubernetes cluster. The helper adds
only the bucket and repository permissions described below; it does not grant
project roles or create service-account keys.

Clone and pin the source:

```sh
git clone https://github.com/agent-substrate/substrate.git /srv/substrate
git -C /srv/substrate checkout --detach 1d7ca8ced056192a1801d6565251adcaab3eb0c9
```

## Review the plan, then apply it

Choose globally unique bucket names and an available full GKE release. Replace
the example project, account and names with your dedicated resources:

```sh
python3 tools/substrate_gke.py \
  --project=my-project \
  --zone=us-central1-a \
  --cluster=branchyard-substrate \
  --bucket=my-project-branchyard-snapshots \
  --registry=us-central1-docker.pkg.dev/my-project/branchyard-substrate \
  --version=1.37.1-gke.2097000 \
  --service-account=substrate-node@my-project.iam.gserviceaccount.com \
  --source=/srv/substrate \
  --state-dir="$HOME/.local/state/branchyard/substrate-gke/my-project"
```

The plan names the project, zone, cluster, bucket, registry, source revision,
Kubernetes context, ownership label and intended actions. It runs no subprocesses,
contacts no services and writes no files. Repeat the same command with `--apply`
after reviewing it.

New clusters use a dedicated `default-pool`, Ubuntu containerd, Workload Identity,
nested virtualization, no release channel, no node auto-upgrade and both required
certificate beta APIs. The default is two `c3-standard-4` nodes with 100 GB disks.
Use `--machine-type=n2-standard-8 --nodes=1`, for example, to specify another
supported footprint. `--network` and `--subnet` default to `default`.
The node disk size does not limit application volumes. The pinned upstream
bundled PostgreSQL manifest requests a separate **500 GiB** persistent volume.
The helper currently has no database-size option. Include that volume in the
cost review before applying. A smaller evaluation database needs an explicit
upstream Kustomize storage override; the upstream kind overlay also reduces
this request for testing. Reducing storage changes available database I/O
capacity, so a small evaluation volume does not establish production throughput.

`--private-nodes` requires existing outbound connectivity, such as Cloud NAT;
`--master-cidr` selects the control-plane CIDR. The helper does not configure NAT
or routes to a private Kubernetes endpoint.

New bucket and repository resources are regional and belong to the specified
project. The bucket uses uniform bucket-level access. The node service account
is explicit; the helper never selects the project's default Compute Engine
account. New resources carry `branchyard-deployment=CLUSTER`.

## Choose the gateway dataplane

The default remains `--atenet-dataplane=envoy`. Add
`--atenet-dataplane=agentgateway` to use the gateway tested in the
[29 October run](evidence/substrate-gke-2026-10-29.json). This path does not require
Docker or buildx. Its plan includes the immutable gateway image
`cr.agentgateway.dev/agentgateway:v0.0.0-alpha.9e78d1da@sha256:8177311eb0444df2829cc00410147e960a3bd7261492f2c8d52b48be6d693a45`.

The supplied checkout stays pristine. On apply, the helper exports only committed
files from the verified source revision into its private state directory, then
wraps the upstream gateway component in a Kustomize image overlay. The native
installer runs from that private copy with `VERSION` pinned, so it needs no Git
metadata there. It retains upstream authentication, policy checks, certificates
and resources. The same image reaches router and egress before their first apply;
a retry does not restore the incompatible image first. The helper verifies its
export's ownership and content digest on reuse and refuses edited or unowned
state. Inspect that state before removing only the generated `agentgateway-source`
directory to regenerate it. Ignored files, local credentials and untracked work
are excluded from the export.

For `--runtime=microvm`, the upstream asset script requires Git metadata. The
helper runs that unchanged script from the supplied Git checkout and sets its
output directory to private state. Generated assets stay outside the verified
export, so interrupted downloads do not invalidate its content digest. The
Go installer then resumes from the private export.

The upstream source's older gateway image rejects its own worker certificate
format with `403 Forbidden: actor certificate has no ActorIdentity`. The selected
image includes [upstream fix 3677](https://github.com/agentgateway/agentgateway/pull/3677)
and matches [Substrate's image update](https://github.com/agent-substrate/substrate/pull/1923).
It validates the worker's actor SPIFFE identity and checks the current Actor;
authentication is retained.

## Adopt or resume a deployment

Every apply describes the resources again. Existing resources must match the
project, location, Standard cluster mode, Kubernetes minor version, network,
node identity, machine type, node count, disk capacity, private-node setting,
Workload Identity, nested virtualization, repository type and source version.
The helper refuses a resource labeled for another deployment. To adopt compatible
unlabeled resources, add `--adopt-existing`; all resources are inspected before
any cloud resource is created or labeled. Bucket ownership must be verified from
its Cloud Storage project number; missing ownership information is refused.
Existing user labels are preserved.

A compatible pool's auto-upgrade setting and version label are reconciled in
separate GKE updates. The pool receives `ate.dev/substrate-version` with the
pinned build version `1d7ca8c`, so replacement nodes have the same version as the
installed workers. The helper sets upstream's `VERSION` explicitly because
`git describe` can vary with the tags present in a checkout. An existing different Substrate version
is refused: use the upstream upgrade procedure to move running actors safely.
The helper does not delete clusters, replace nodes, shrink disks, move snapshots
or upgrade an existing deployment to a new source revision.

To use a kubeconfig you already prepared, pass both options:

```sh
  --kubeconfig=/path/to/dedicated-kubeconfig --context=exact-context-name
```

The helper verifies that context's API address against the described cluster's
public or private endpoint. It passes the same explicit context to every
Kubernetes command and to `ate-setup`. Providing only `KUBECONFIG` would let the
upstream installer fetch GKE credentials again; the explicit `--context` avoids
that behavior. With neither option, credentials go into the helper's dedicated
state directory. Your normal kubeconfig and current context are unchanged.

Cloud Storage bucket creation and ownership labeling are separate commands.
The helper saves a private `bucket-creation.json` intent before creation and a
receipt from raw bucket metadata before applying the label. If labeling fails,
a retry can label only the same project, region, name and creation timestamp.
A replacement bucket or an arbitrary unlabeled bucket still requires explicit
ownership review. If the process stops after creation but before its receipt
is saved, the helper refuses automatic adoption; inspect the bucket and use
`--adopt-existing` only after verifying it belongs to this deployment.

Retry the same command after fixing a failed prerequisite or installer step.
Resources already created are reused; existing IAM bindings are not added
again. Upstream reapplies its installation and reuses its existing prerequisites.
Installer output is retained in the state directory's owner-readable
`install.log`; terminal output contains only progress and sanitized errors.
A successful run writes `deployment.json` containing public resource names,
source identity, worker image digest and selector, and connection-file paths. Neither file is evidence that a
harness has completed a model turn.

## Certificate APIs and worker requirements

Check the exact APIs, including on GKE 1.37:

```sh
kubectl --kubeconfig=/path/to/dedicated-kubeconfig \
  --context=exact-context-name \
  get --raw /apis/certificates.k8s.io/v1beta1
```

Both `podcertificaterequests` and `clustertrustbundles` must appear. Kubernetes
version alone does not establish that the pinned controller's APIs are served.
The helper enables both on creation and checks discovery before installing
Substrate. If an existing cluster lacks them, it reports the exact update command
and stops before the installer. GKE 1.36 also requires replacing nodes that were
created before enabling the APIs; GKE 1.37 permits the existing nodes to remain.
See Google's [Substrate installation requirements](https://docs.cloud.google.com/kubernetes-engine/ai-ml/install-overview-substrate).

Autopilot, Kubernetes versions below 1.36, spot or preemptible nodes, autoscaled
pools, and mixed or multiple worker pools are refused by this helper's dedicated
profile. Nested virtualization and GKE metadata mode are checked in the node
pool configuration. The helper defaults to a gVisor worker pool. Select `--runtime=microvm` to also
install upstream's cluster-wide `microvm` SandboxConfig and pinned assets.

The pool defaults to namespace `branchyard`, name `branchyard-gvisor` (or
`branchyard-microvm`), and one replica. Change these with `--worker-namespace`,
`--worker-name` and `--worker-replicas`. Each worker requests 500 millicores and
4 GiB memory, with limits of 2 CPUs and 4 GiB memory. The helper uses upstream's
`ate-setup publish worker-images`, requires digest-pinned references under the
selected registry, and creates the WorkerPool using the selected runtime's image.
It waits for the requested number of ready workers before reporting success.

Both the WorkerPool and its generated worker pods carry `workload=branchyard`
and `branchyard-runtime=gvisor` or `branchyard-runtime=microvm`. Select these
labels in the actor template. The emitted `deployment.json` records the exact
selector, image digest, worker limits and SandboxConfig name (`gvisor-default`
or `microvm`). Actor resources must fit inside the worker limits, including the
microVM runtime's memory overhead; 1 CPU and 2 GiB is a starting template size.
See [Substrate setup](substrate-setup.md) to create the bridge template.

Existing namespaces and WorkerPools must have the same ownership label, or be
explicitly adopted with `--adopt-existing`. Existing WorkerPool image, runtime,
resources, selector and replica count must match exactly. A mismatch fails
without changing that pool; choose another name or follow upstream's upgrade
procedure. This prevents a setup retry from rolling workers that already hold
actors.

## IAM scope

The helper reconciles these bindings without granting project-wide storage or
Artifact Registry access:

| Resource | Principal | Roles |
|---|---|---|
| Selected snapshot bucket | Workload Identity `ate-system/atelet` | `storage.objectAdmin`, `storage.bucketViewer` |
| Selected snapshot bucket | Workload Identity `ate-system/ate-api-server` | `storage.objectAdmin`, `storage.bucketViewer` |
| Selected Artifact Registry repository | Workload Identity `ate-system/atelet` | `artifactregistry.reader` |
| Selected Artifact Registry repository | Explicit node service account | `artifactregistry.reader` |

Workload Identity principals use the target project number and its
`PROJECT_ID.svc.id.goog` pool. Image publishing uses the operator's existing
Google Cloud credentials through a Docker credential helper in the dedicated
state directory. Tokens are not read, printed or written into generated plans
by this helper.

## Verification

Hermetic evidence: `python3 tests/test_substrate_gke.py` drives fake `gcloud`,
`kubectl`, Git, Go and Docker executables. It covers offline plans, repeated
apply, interrupted installation, foreign-resource refusal, explicit adoption,
context preservation, scoped IAM, missing beta APIs on 1.37, pinned worker images,
worker sizing and reuse, live log streaming, prerequisites and error-output redaction. The fast gate runs these tests. Not done: these fake
runs do not provision Google Cloud resources, verify `/dev/kvm` inside a worker,
exercise a live actor, or call a model.

Live evidence, 28 October 2026: the helper completed against GKE
`1.37.1-gke.2097000` using the pinned Substrate revision, adopted the dedicated
cloud resources, installed the control plane and created
`branchyard-qual/branchyard-gvisor-qualified`. A separate Kubernetes read
confirmed two desired and two ready workers with the declared selector and
image digest. A later apply installed the microVM assets and created
`branchyard-qual/branchyard-microvm-qualified` with two ready workers. Repeating
that apply retained the same WorkerPool UID, full spec and deployment record. The [remote validation record](remote-validation-2026-10-28.md)
tracks the deployment and actor checks; the helper record is
`/tmp/branchyard-remote-20261028/substrate-gke-helper-validation.md`, with driver
output in `logs/gke-helper-live-3.log` and `logs/gke-helper-microvm-live.log`.
Not done: these helper runs did not create a fresh GKE cluster, execute an actor
or call a model.

Live setup evidence, 29 October 2026: Google Cloud SDK `590.0.1` rejected
`gcloud storage buckets create --labels=...` with `unrecognized arguments`. A
private qualification driver created the dedicated bucket without that flag,
then applied `gcloud storage buckets update --update-labels=...`. The revised
helper's hermetic tests exercise that supported sequence, a failed label update,
a lost creation receipt, a replacement bucket and invalid or foreign receipts.
Not done: this exact revised helper has not provisioned another live deployment;
the live command record is `/home/user/branchyard-console-work-20261029/gke-state/commands.jsonl`.

Live native-bridge evidence, 29 October 2026: a fresh private GKE Standard
`1.37.1-gke.2097000` cluster ran the pinned Substrate source on one
`c3-standard-4` node with a 100 GB boot disk and an explicit 10 GiB evaluation
database. Branchyard CLI `0.3.0` candidates ran
`by substrate check --smoke --harness agy --json` from the operator machine and
an existing GCE validation host. The command created a disposable gVisor Actor,
put and retrieved the same 29 bytes through its bridge, executed
`agy --version` with result `1.4.9`, and destroyed the Actor. An independent
rerun passed, and a native API query afterward found no Actors in the
test atespace. TLS hostname verification passed over the private GCE connection.
The [dated run record](evidence/substrate-gke-2026-10-29.json) includes image
digests, ownership, scoped IAM, the 500 GiB database-default correction and the
worker-capacity recovery after resetting that fresh database. The later console
model run and resource deletion are recorded below. Not done:
this native smoke called no model. Interactive PTY, retained pause/resume,
MicroVM and Agentic Compute Engine were not tested in this deployment.

Hermetic and local rendering evidence, 29 October 2026: the helper's 26 tests
passed, including the optional agentgateway export, offline plan, Docker-free
prerequisites, retry reuse, modified-state refusal and archive traversal refusal.
Native Kustomize `v5.7.1` rendered the compatible image for both gateway components
from the actual private export, and Go resolved the installer module there without
Git metadata. Its native `ko` build also completed into a private OCI layout
without registry publication; the export's content digest was identical before
and after the build. The asset-script regression covers an interrupted download
and retry with generated outputs outside the verified export. The supplied source
checkout remained clean. On the retained live
cluster, a single Actor reproduced gateway rejection before the egress image
correction, then verified TLS 1.3 and unauthenticated HTTPS 404 responses from both
Google hosts afterward. The Actor was destroyed, and a native API query confirmed
no Actors remained. The [run record](evidence/substrate-gke-2026-10-29.json) contains
the identity mismatch and before/after results. Not done: the revised helper has
not provisioned another cluster; these network checks do not qualify a model turn.

Live console and cleanup evidence, 29 October 2026: Branchyard CLI and server
`0.3.0` completed a knowledge session without a repository on Substrate, using
native AGY in the immutable Actor image. The model produced `annual-net-summary.csv`;
its download and in-system table preview returned 15,000, 21,000 and total 36,000.
An independent check verified the CSV bytes, arithmetic, preview rows and input
INDEX hash. Native API inspection at 13:29:52 UTC found no Actors across all
atespaces and no worker allocation. After preserving outputs, owned-only cleanup
removed the temporary cluster, both database disks, node and boot disk, registry,
snapshot bucket, dedicated service account and its project IAM membership.
Private forwards and the marked connection files were also removed. The
[final run record](evidence/substrate-gke-2026-10-29.json) contains exact names,
UIDs, checks and deletion results. Not done: interactive PTY and retained Actor
pause/resume/suspend were not qualified; the default-destroy run returned an
explicit unsupported response when a terminal was requested afterward. MicroVM
and Agentic Compute Engine were not tested in this deployment.

## Remove a test deployment

Keep the redacted run records before deleting a temporary deployment. Read its
resource names from `deployment.json` and remove only resources dedicated to
that test; adopted resources may serve other work. Stop its actors and local
connection processes first. Before deleting the cluster,
record each owned database claim UID, PV UID and CSI disk handle. Delete only
the owned database controller and claim while Kubernetes is still running,
wait for its `Delete` reclaim policy to remove the PV, and verify that exact
Compute Engine disk is absent. A node boot disk's auto-delete setting does not
cover a database PVC, and cluster deletion alone can leave that disk behind.
After disk reclamation, delete the cluster, repository and snapshot bucket:

```sh
gcloud container clusters delete CLUSTER --project=PROJECT --zone=ZONE
gcloud artifacts repositories delete REPOSITORY --project=PROJECT --location=REGION
gcloud storage rm --recursive gs://BUCKET
```

Replace the uppercase names with the recorded values. Wait for cluster deletion
before removing a node service account created solely for the test. Remove that
account's project IAM binding first; with a policy containing conditional
bindings, a noninteractive removal of an unconditional binding needs
`--condition=None`. The helper uses an existing node account and therefore does
not delete it automatically.

Remove the deployment's private token files, Docker authentication directory and
dedicated kubeconfig when finished. Preserve any pre-existing credentials,
networks, NAT configuration and services. The [qualification record](remote-validation-2026-10-28.md)
records which resources were actually removed during the live run.
