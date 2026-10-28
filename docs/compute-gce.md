# Prepare a GCE worker

`tools/compute_gce.py` prepares a dedicated Ubuntu 24.04 machine for Branchyard's
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

The default uses an external SSH address and the existing `default` network and
subnet. `--internal-ip` creates a machine without an external address and uses
its private address for SSH. That option requires an existing operator route
and outbound access, such as Cloud NAT, for package and image downloads. The
helper changes no firewall, network, subnet or NAT configuration.

## Review and apply

Run from a clean, committed project repository and replace the example names:

```sh
python3 tools/compute_gce.py \
  --project=my-project \
  --zone=us-central1-a \
  --name=branchyard-worker \
  --repo="$PWD" \
  --version=0.1.0-rc.1 \
  --image=ghcr.io/vamsiramakrishnan/branchyard-agy:0.1.0-rc.1
```

This prints the plan without running subprocesses, contacting Google Cloud or
creating files. Repeat with `--apply` to prepare the worker. Pin `--image` by
its published `@sha256:` digest when reproducing a qualification record. A
release-version tag is also accepted; `latest` is refused. The release binaries
and worker image must already be published and accessible to the machine.

The helper creates an `n2-standard-4` VM with a 100 GB balanced persistent disk.
`--machine-type`, `--disk-size`, `--network` and `--subnet` select another
reviewed footprint. Existing machines must carry the matching
`branchyard-deployment` label and exact configuration fingerprint. An unlabeled,
foreign, stopped or differently configured machine is refused. The helper
does not adopt or replace it.

Bootstrap installs Git, curl, Docker and native runtime libraries using Ubuntu's
package manager, then installs the exact released `by`, server and bridge into
`/usr/local`. It pulls the worker image and extracts its standalone `agy`
executable. This uses Docker for artifact delivery; a local-process task on the
worker has the worker user's permissions and makes no container isolation claim.

`--repo` transfers a git bundle of committed `HEAD`; uncommitted and untracked
files cause a refusal before VM creation. On retry, the remote `HEAD` must match
the source. The helper never resets existing remote work to a newer source
commit. Without `--repo`, it initializes an empty repository with a first commit.
Each remote project lives under `/srv/branchyard/NAME`.

## Connect and resume

The helper uses `gcloud compute ssh` for initial authentication and host-key
registration. It then writes a deployment-owned Host entry under
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

Remove that deployment's owned SSH Host file and private state directory. Keep
the shared Include line while other Branchyard Host files use it. The helper
does not delete an existing service account or any network resources.

## Verification

Hermetic on 28 October 2026: `tests/test_compute_gce.py` runs fake gcloud and SSH
executables, a real temporary Git repository, and the offline CLI. It exercises
VM ownership and configuration refusals, missing login, permission failures,
private errors, retry, idempotent SSH configuration, repository transfer and
Microsandbox bootstrap generation. The exact downloaded Microsandbox 0.7.3
archive was checksum-verified and its CLI help inspected while implementing
the bootstrap. Not done: this helper has not yet created a fresh live GCE
machine or completed a model task; the earlier SSH and Microsandbox runs used
separately prepared environments.
