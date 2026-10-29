# Prepare a GCE worker

`by compute gce` prepares a dedicated Ubuntu 24.04 machine for Branchyard's
[native SSH remote](remote-ssh.md). The default command prints an offline JSON
plan. Add `--apply` after reviewing the machine, image, SSH changes and ongoing
compute charges. The helper installs released binaries; it does not compile
Branchyard or call a model.

## Prepare access

Use Python 3.10 or newer, Git, OpenSSH and an authenticated Google Cloud CLI.
The project needs billing and the Compute Engine API. The operator needs
permission to create and inspect instances, connect through OS Login, and use
passwordless sudo on the created machine. Selecting a service account also
requires permission to attach that existing account.

The helper attaches no service account by default and grants no project IAM
roles. An optional `--service-account` must name an existing account in the
selected project; the VM still receives no Google API OAuth scopes. Model
credentials can be delivered through the explicit option below. No model key is embedded in VM
metadata, the bootstrap script or an image.

The default uses an external SSH address, direct SSH and the existing `default`
network and subnet. It requires your existing SSH firewall access.

Add `--tunnel-through-iap` to connect through Google Cloud Identity-Aware Proxy
(IAP). On apply, the helper creates or verifies one deployment-owned firewall
rule. It allows TCP 22 from `35.235.240.0/20` only to the deployment's unique VM
tag. A conflicting rule is refused. Your account needs IAP tunnel access, such
as `roles/iap.tunnelResourceAccessor`, and permission to create the firewall rule.
The helper grants no IAM roles. See [Google's IAP TCP forwarding requirements](https://docs.cloud.google.com/iap/docs/using-tcp-forwarding).

IAP is optional and is not live qualified in the tested project. Its managed
firewall enforcer removes the dedicated allow rule, preventing the tunnel from
reaching SSH. An organization or project policy can therefore block IAP even
when your account can create the rule. Resolve that policy with the project
administrator or select an already permitted route. The helper does not retry
by opening broader ingress.

`--internal-ip` omits the VM's external address. Combine it with
`--tunnel-through-iap` to reach a private-only VM through IAP, or provide your own
private route for direct SSH. A private-only VM also needs existing outbound
access, such as Cloud NAT, for package and image downloads. The helper creates
no network, subnet or NAT and leaves other firewall rules unchanged.

## Review and apply

Run from a clean, committed project repository and replace the example names:

```sh
by compute gce \
  --project=my-project \
  --zone=us-central1-a \
  --name=branchyard-worker \
  --directory="$PWD" \
  --tunnel-through-iap \
  --version=0.2.1 \
  --image=ghcr.io/vamsiramakrishnan/branchyard-agy:0.2.1
```

The planner contacts no cloud service, runs no provider commands and creates no
files. Repeat with `--apply` to prepare the worker. Pin `--image` by
its published `@sha256:` digest when reproducing a qualification record. A
release-version tag is also accepted; `latest` is refused. The release binaries
and worker image must already be published and accessible to the machine.

`by compute` is an operator command. It refuses harness sessions and `--remote`.
The packaged helper needs no Branchyard source checkout. From a source checkout,
`python3 tools/compute_gce.py` runs the same helper; its source-repository option
is named `--repo` instead of the CLI's `--directory`.

The helper creates an `n2-standard-4` VM with a 100 GB balanced persistent disk.
`--machine-type`, `--disk-size`, `--network` and `--subnet` select another
reviewed footprint. Existing machines must carry the matching
`branchyard-deployment` label and exact configuration fingerprint. An unlabeled,
foreign, stopped or differently configured machine is refused. The helper
does not adopt or replace it.

If a zone has no capacity for the selected machine type, review a different
`--zone` or a compatible `--machine-type` and repeat the plan. Deployment
identity includes the zone, so another zone uses separate state and resources.

Bootstrap installs Git, curl, Docker and native runtime libraries using Ubuntu's
package manager, then installs the exact released `by`, server and bridge into
`/usr/local`. It pulls the worker image and extracts its standalone `agy`
executable. This uses Docker for artifact delivery; a local-process task on the
worker has the worker user's permissions and makes no container isolation claim.

`--directory` transfers a git bundle of committed `HEAD`; uncommitted and untracked
files cause a refusal before VM creation. On retry, the remote `HEAD` must match
the source. The helper never resets existing remote work to a newer source
commit. Without `--directory`, it initializes an empty repository with a first commit.
Each remote project lives under `/srv/branchyard/NAME`.

## Connect and resume

The helper uses `gcloud compute ssh` with the selected transport for initial
authentication and host-key registration. With IAP, the saved Host entry uses
gcloud's IAP tunnel for later native SSH connections too. It then writes a deployment-owned Host entry under
`~/.ssh/branchyard/` and prepends one Include line to `~/.ssh/config`, preserving
existing content. The alias is derived from the project, zone and machine, so
the same machine name in another project does not share its entry. An alias
already mapped by another configuration is refused. Strict host-key checking
stays enabled.

The owner-readable `deployment.json` in the state directory records `remote`,
`ssh_config`, `remote_command`, source commit, image and version. The default
state directory is
`~/.local/state/branchyard/compute-gce/PROJECT/ZONE/NAME`; `--state-dir` overrides
it. Bootstrap output stays in its private `install.log`; terminal errors omit
captured command output. Rerun the same command after fixing a reported failure.
A matching existing VM is reused and completed steps can run again.

Use the recorded URL with `by --remote URL`, or save it as `[remote] url` in the
local project configuration. The native remote starts and authenticates its own
Unix-socket server. It introduces no public Branchyard HTTP port.

Operations act on the remote copy. After reviewing and merging a candidate
there, fetch its parent branch through the recorded SSH alias if the local
checkout should receive it too:

```sh
git fetch SSH_ALIAS:/srv/branchyard/NAME main
git merge FETCH_HEAD
```

Availability of `agy --version` proves installation only. To reuse a model API
key already in your environment, add `--forward-secret=GEMINI_API_KEY` or
`--forward-secret=GOOGLE_API_KEY` to the reviewed command. Only the variable name
appears in the plan. Apply sends its value over SSH standard input to the
worker's native `by harnesses login antigravity --api-key` command. That command
stores canonical `GEMINI_API_KEY` in a 0600 file, outside the repository, and
names it in the deployment's private user configuration. Values never appear
in arguments, deployment records or bootstrap logs. A missing or malformed
selected source stops before cloud mutation; reapplying can rotate it without
replacing the machine.

The helper configures the remote project and server through `by init project`
and `by init server`, then validates both. Tasks use private homes. The server's
secret registry references the stored key file and fixes agy to the installed
executable. Its worker policy permits agy's tool execution; clients still make
the explicit `--allow-unapproved-tools` choice for a task. A deployment wrapper
keeps this server configuration and private keystore selected for later SSH
sessions. Existing explicit `by serve --config` arguments remain usable.

Without `--forward-secret`, no model key is copied. The server names its future
key file so credentials can be configured there later with the same native
login command. Model authentication remains unverified until a real task
completes. Workstation OAuth files are not transferred.

## Use Microsandbox on the worker

Add `--backend=microsandbox`. This requires an `n2-standard` or `c3-standard`
machine with nested virtualization. The helper enables nested virtualization,
installs the release's Linux runtime build with `--runtime`, adds the SSH user
to `kvm`, and installs Microsandbox 0.7.3 and its matching firmware. The runtime
archive has a pinned SHA-256. A short, private `/var/lib/by-msb` avoids Unix
socket limits under long OS Login home paths. A wrapper preserves that setting
for the remote `by` server; the worker image is imported into the local VM cache.

The installed CLI is probed and a fresh SSH session checks read/write KVM access.
The exact Microsandbox 0.7.3 CLI exposes `context`, `doctor` and `image load`;
it has no `init` command. These probes call no model and do not qualify a VM
lifecycle. The pinned runtime's hard total memory ceiling remains unqualified;
see the [real-runtime results](remote-validation-2026-10-28.md).

## Remove a temporary worker

Stop its Branchyard SSH server with `by remote ssh stop URL`, retain redacted
evidence, and delete only the dedicated VM:

```sh
gcloud compute instances delete NAME --project=PROJECT --zone=ZONE
```

If you selected IAP, read the owned firewall name from `deployment.json` and
delete that rule too:

```sh
gcloud compute firewall-rules delete IAP_FIREWALL --project=PROJECT
```

Remove that deployment's owned SSH Host file and private state directory. Keep
the shared Include line while other Branchyard Host files use it. The helper
does not delete resources automatically or change an existing service account.

## Verification

Live on 28 October 2026, finishing at 12:56:28 UTC: the patched helper completed
setup and resume on a fresh dedicated worker in `us-central1-b` over an existing
private VPC route. It installed the published `by`, server and bridge 0.1.0,
AGY 1.4.9 and Microsandbox 0.7.3, and imported the worker image. Native project
and server initialization, configuration validation, `serve --check`, KVM
access and the managed SSH alias passed. The result is recorded in
`/tmp/branchyard-install-20261028/gce-qual-published/private-b-final-apply-result.json`
with exit 0. Its `private-b-state/` directory contains the deployment record
and install log. See
the [dated validation record](remote-validation-2026-10-28.md) for the broader
qualification. Not done: no model was called or model credentials forwarded in
this setup run, no existing project was transferred, and no complete Microsandbox
task or hard memory ceiling was qualified. IAP remains unqualified because the
project's managed firewall enforcer removed its allow rule.

Hermetic on 28 October 2026: `tests/test_compute_gce.py` and
`tests/test_cloud_setup.py` use fake provider commands and a temporary Git
repository. The CLI's `setup_cmd` tests run real offline plans and a guided
terminal review, then cancel before cloud mutation. They cover VM and firewall
ownership refusals, private and IAP transport selection, named credential
forwarding, resumable SSH configuration and repository transfer. The exact
downloaded Microsandbox 0.7.3 archive was checksum-verified and its CLI help
inspected while implementing the bootstrap. See the
[dated live validation record](remote-validation-2026-10-28.md) for
real-environment evidence. Not done: these hermetic checks do not qualify a live
GCE bootstrap, model authentication or a complete Microsandbox lifecycle.


Public 0.1.1 follow-up, 28 October 2026: live provisioning through the published
standalone `by compute gce --apply` also passed from a non-Git directory, using
its embedded helper. A fresh private worker passed binary/version, native
configuration, KVM, authenticated remote connection and real MicroVM execution
checks. See the [public verification record](remote-validation-2026-10-28.md#public-011-verification-28-october-2026).
Not done: model execution, the MicroVM hard memory ceiling, and IAP under this
managed network policy.
