# Set up Agent Substrate

Use this workflow after installing Agent Substrate in a compatible Kubernetes
cluster. `by setup` saves the connection; `by substrate setup` prepares the
named atespaces and bridge template; `by up` checks the saved prerequisites.
Cloud cluster creation and upstream runtime installation remain separate
operations. See [Substrate](substrate.md) for the connection fields and
[remote validation](remote-validation-2026-10-28.md) for observed setup friction.

## Prepare the connection

Run `by setup` and choose Substrate, or save `[substrate]` in `branchyard.toml`.
Use a router base URL. The client sends the actor identity in a routing header.
Store credential file paths, never token or signing-key contents, in configuration.
Relative paths resolve from the configuration file's directory.

For a TLS Control port-forward, save the trusted CA and `server_name` matching
the service certificate. Keep the forwarding process running while Branchyard
uses it. A loopback connection still needs Control authentication: save the
Kubernetes service-account token path as `api_token_file`. The client rereads
that file for each request, so replacing an expired token does not require
rewriting project configuration.

Select actor egress explicitly. A denied policy cannot reach a model API.
An allowlist must cover the harness's actual service destinations; unrestricted
egress is available as an explicit operator choice. The policy is applied to
fresh actors and snapshot children before execution; an existing conflicting
policy is refused.

## Prepare the image and template

The image needs `branchyard-bridge`, `git`, a shell, the selected harness, and
writable workspace and private-home directories for its unprivileged user.
For Antigravity install `agy`; Gemini CLI is a separate harness. Pin the image
by its registry digest and use the bridge built with your Branchyard checkout.
Make the executables readable and executable by the configured UID, and test
`agy --version` as that UID while building the image. A successful root-user
image probe does not establish that the harness user can execute it.

Use [deploy/Dockerfile.substrate](../deploy/Dockerfile.substrate) and
[its startup script](../deploy/substrate-entrypoint.sh) for a bridge and a
standalone Linux harness binary. Build the bridge on Linux for the worker's
architecture and a libc compatible with the base image. Stage the binaries and
script in a clean build context:

```sh
cargo build --release --locked --offline -p branchyard-bridge
substrate_image_context=$(mktemp -d)
cp "${CARGO_TARGET_DIR:-target}/release/branchyard-bridge" "$substrate_image_context/branchyard-bridge"
cp "$(command -v agy)" "$substrate_image_context/agy"
cp deploy/substrate-entrypoint.sh "$substrate_image_context/substrate-entrypoint.sh"
docker build -f deploy/Dockerfile.substrate -t branchyard-agy:local "$substrate_image_context"
```

The recipe defaults to Ubuntu 24.04; set `BASE_IMAGE` to your approved digest
when publishing. `BRIDGE_BINARY` and `HARNESS_BINARY` select filenames in the
build context, defaulting to `branchyard-bridge` and `agy`. The image build runs
both version probes as the configured unprivileged user. It includes no model
credentials and makes no model call.

The image's `ACTOR_UID`, `ACTOR_GID`, `ACTOR_WORKDIR` and `ACTOR_HOME` build
arguments default to `1000`, `1000`, `/workspace` and `/branchyard/home`. They
become the `BRANCHYARD_ACTOR_*` environment variables. Match the UID and GID to
`by substrate setup --run-as` and the paths to saved Substrate `workdir` and
`home`. Parent directories must be traversable by that user. The startup script
refuses a mismatched `--run-as`, overlapping directories, symlinks, noncanonical
paths, and paths containing or contained by the bridge's default identity or
state directories.

At the pinned upstream revision, layer extraction can lose directory ownership
and the private overlay can give the actor root mode `0700`. Image-build
ownership alone therefore does not establish permissions inside the actor.
Before starting `serve`, the script restores `/` to root-owned mode `0755` and
prepares only the configured workspace and HOME: create the directories, take
root ownership, set mode `0700`, then hand ownership to the actor UID/GID. The
last chown avoids needing `FOWNER`; no recursive ownership or permission changes
are made. The real bridge then replaces the script as PID 1 and runs harness
commands and transfers as the unprivileged user. `--version`, `--help` and
`keygen` do not run directory preparation.

Probe the preparation using the same narrow capabilities as the template:

```sh
docker run --rm --network=none --cap-drop=ALL \
  --cap-add=CHOWN --cap-add=SETUID --cap-add=SETGID \
  branchyard-agy:local serve --run-as 1000:1000 --help
```

This runs the startup preparation before the bridge prints help. Follow it with
`by substrate check --smoke --harness agy` against the real actor: that checks the
runtime's extraction, routing, execution and transfer behavior.

```sh
by substrate setup \
  --image registry.example/branchyard-agy@sha256:<digest> \
  --storage gs://<snapshot-bucket>/branchyard \
  --worker-label workload=branchyard \
  --plan
```

The template name and signer path come from the saved connection. Repeat with
`--yes` to apply, or without `--plan` to review the interactive confirmation.
On first apply, Branchyard creates the signing key with owner-only access.
Only its public verifier enters the template. A rerun reuses the key and
matching resources; a different immutable template requires a new saved name.

The defaults are the `gvisor-default` SandboxConfig, a gVisor runtime,
`/usr/local/bin/branchyard-bridge`, execution as UID/GID `1000:1000`, two CPUs
and 4 GiB of memory. Use `--sandbox-config`, `--runtime microvm`, `--bridge`,
`--run-as`, `--cpu` and `--memory` to match your deployment. The selected worker
pool must support that runtime and capacity. The bridge listens on actor port
80, as the upstream router expects, and drops privilege for harness execution.
The template grants `CHOWN` for this directory preparation and `SETUID` and
`SETGID` so the bridge can switch identity and transfer files as the configured
user; ordinary harness processes run without those root capabilities.

`--plan` creates neither a signing key nor remote resources. With a missing key,
it uses an in-memory signer to authenticate and compare the desired template
before creating any key, directory or yard state. An existing incompatible
template is refused before any write. Without a terminal, setup previews
changes unless `--yes` is given. `--json` emits a structured report without
terminal control sequences.

## Verify and run

```sh
by up --plan
by up
by doctor
by substrate check --smoke --harness agy
by run 'Make the requested change and run the project checks' \
  --harness antigravity --secret GEMINI_API_KEY \
  --allow-unapproved-tools --yes
```

The smoke creates a disposable actor, writes and reads a file through the
bridge, executes `agy --version`, and deletes the actor. It calls no model.
Failures name the file-transfer or executable step that stopped, followed by
the actor cleanup result.
Without `--smoke`, `by substrate check` authenticates and verifies the template
listener, health probe, identity projection and signing-key match. If execution fails, cleanup is still attempted and a cleanup
failure names the actor to inspect. SIGINT and SIGTERM request cleanup after
the current bounded provider call returns; executable probes have a 30-second
deadline and bounded output capture. A force-killed CLI cannot promise cleanup;
inspect the named `by-check-` actors before retrying.

`up` and doctor check the remote credential delivery method. A local OAuth
login does not satisfy a remote Antigravity API-key requirement, and a harness
installed only in the actor image does not need a second local installation.
A configured secret source is evidence of availability, not a verified model
login. Only an actual task demonstrates that the harness authenticated and
completed work. Inspect its diff and run `by merge <branch>` after validation.

If setup stops, fix the reported prerequisite and rerun the same command.
Already matching resources are reused; a template identity or policy conflict
is reported rather than silently replaced. `by doctor` is the next diagnostic
command, and `by up` repeats the readiness checks.
