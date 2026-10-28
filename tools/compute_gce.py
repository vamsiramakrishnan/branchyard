#!/usr/bin/env python3
"""Plan or prepare an owned GCE machine for Branchyard's native SSH remote."""

import argparse
import hashlib
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

OWNER_LABEL = "branchyard-deployment"
# Checked and rewritten by tools/release_version.py before release packaging.
RELEASE_VERSION = "0.1.0"
IMAGE_REPOSITORY = "ghcr.io/vamsiramakrishnan/branchyard-agy"
MSB_VERSION = "0.7.3"
MSB_ARCHIVE_SHA256 = "2d5da7de187246c804dce2a1a23f06626b6f7cb892d9a0c7c5c620784ba412ea"


class SetupError(Exception):
    """An actionable failure that never includes captured command output."""


@dataclass(frozen=True)
class Config:
    project: str
    zone: str
    name: str
    state_dir: Path
    version: str = RELEASE_VERSION
    machine_type: str = "n2-standard-4"
    service_account: str | None = None
    image: str = f"{IMAGE_REPOSITORY}:{RELEASE_VERSION}"
    backend: str = "local"
    repo: Path | None = None
    network: str = "default"
    subnet: str = "default"
    disk_size: int = 100
    internal_ip: bool = False
    forward_secret: str | None = None

    @property
    def remote_path(self) -> str:
        return f"/srv/branchyard/{self.name}"

    @property
    def alias(self) -> str:
        identity = f"{self.project}/{self.zone}/{self.name}".encode()
        return f"branchyard-{self.name[:32]}-{hashlib.sha256(identity).hexdigest()[:8]}"

    @property
    def fingerprint(self) -> str:
        target = asdict(self)
        for key in ("state_dir", "repo", "forward_secret"):
            target.pop(key)
        return hashlib.sha256(json.dumps(target, sort_keys=True).encode()).hexdigest()

    def validate(self) -> None:
        for key in ("project", "name", "network", "subnet"):
            if not re.fullmatch(r"[a-z][a-z0-9-]{0,61}[a-z0-9]", getattr(self, key)):
                raise SetupError(f"--{key.replace('_', '-')} must be a lowercase Google Cloud resource name")
        if not re.fullmatch(r"[a-z]+-[a-z]+\d-[a-z]", self.zone):
            raise SetupError("--zone must name a zone such as us-central1-a")
        if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[a-zA-Z0-9.-]+)?", self.version):
            raise SetupError(f"--version must pin a Branchyard release, such as {RELEASE_VERSION}")
        if self.backend not in {"local", "microsandbox"}:
            raise SetupError("--backend must be local or microsandbox")
        if self.forward_secret not in {None, "GEMINI_API_KEY", "GOOGLE_API_KEY"}:
            raise SetupError("--forward-secret must name GEMINI_API_KEY or GOOGLE_API_KEY")
        families = "n2|c3" if self.backend == "microsandbox" else "n2|c3|e2"
        if not re.fullmatch(rf"(?:{families})-standard-\d+", self.machine_type):
            raise SetupError("--machine-type must be an x86 standard machine; Microsandbox needs n2 or c3")
        if self.service_account and not re.fullmatch(
            rf"[a-z][a-z0-9-]+@{re.escape(self.project)}\.iam\.gserviceaccount\.com", self.service_account
        ):
            raise SetupError("--service-account must name an existing account in --project")
        if self.disk_size < 30:
            raise SetupError("--disk-size must be at least 30 GB")
        if not re.fullmatch(
            r"[a-z0-9][a-z0-9./_-]+(?:@sha256:[0-9a-f]{64}|:\d+\.\d+\.\d+(?:-[a-zA-Z0-9.-]+)?)", self.image
        ):
            raise SetupError("--image must pin a published worker image by digest or release version")


def plan(config: Config) -> dict[str, Any]:
    """Describe work without running commands, writing files or contacting services."""
    config.validate()
    return {
        "mode": "plan",
        "target": {key: str(value) if isinstance(value, Path) else value for key, value in asdict(config).items()},
        "ownership_label": f"{OWNER_LABEL}={config.name}",
        "configuration_sha256": config.fingerprint,
        "remote": f"ssh://{config.alias}{config.remote_path}",
        "ssh_alias": config.alias,
        "actions": [
            "Check gcloud login, source repository and optional existing service account",
            "Create a dedicated Ubuntu 24.04 VM; refuse foreign or differently configured machines",
            "Install the pinned released Branchyard binaries and extract agy from the published worker image",
            "For microsandbox: enable nested virtualization, install the pinned runtime and import the worker image",
            "Transfer only a clean committed HEAD, or initialize an empty remote repository",
            "Write an owned SSH Host entry and one Include line, preserving existing SSH configuration",
            "Verify binary versions, SSH configuration and KVM access before recording deployment.json",
        ],
        "service_account": config.service_account or "none; no default Compute Engine account selected",
        "network": "private address; requires an existing route" if config.internal_ip else "external SSH address",
        "qualification": {"model": "not_checked", "microsandbox_hard_memory_ceiling": "not_qualified"},
        "credential_delivery": {
            "source_variable": config.forward_secret,
            "destination": "native by keystore: GEMINI_API_KEY in an owned 0600 file",
            "transport": "SSH standard input; no values in arguments, plans or logs",
        },
    }


class Runner:
    """Run argument vectors with bounded waits and private bootstrap output."""

    def __init__(self, config: Config):
        self.config = config
        self.env = dict(os.environ, CLOUDSDK_CORE_DISABLE_PROMPTS="1")

    def run(
        self,
        args: list[str],
        *,
        optional: bool = False,
        script: str | None = None,
        secret_input: str | None = None,
    ) -> str | None:
        try:
            if script is not None:
                path = self.config.state_dir / "install.log"
                with open(path, "a", opener=lambda p, flags: os.open(p, flags, 0o600)) as output:
                    os.fchmod(output.fileno(), 0o600)
                    result = subprocess.run(
                        args,
                        input=script,
                        text=True,
                        env=self.env,
                        stdout=output,
                        stderr=subprocess.STDOUT,
                        timeout=1800,
                        check=False,
                    )
                stdout, stderr = "", ""
            else:
                result = subprocess.run(
                    args, input=secret_input, text=True, env=self.env, capture_output=True, timeout=300, check=False
                )
                stdout, stderr = result.stdout, result.stderr
        except (OSError, subprocess.TimeoutExpired) as error:
            raise SetupError(f"Could not finish {args[0]}: {type(error).__name__}; check the tool and retry") from None
        if result.returncode:
            if optional and re.search(r"\b(?:NOT_FOUND|NotFound|404)\b", stderr):
                return None
            detail = f" Inspect {self.config.state_dir / 'install.log'} privately." if script is not None else ""
            raise SetupError(f"{args[0]} failed (exit {result.returncode}).{detail} Check access and rerun to resume.")
        return "" if secret_input is not None else stdout.strip()

    def json(self, args: list[str], *, optional: bool = False) -> dict[str, Any] | None:
        output = self.run(args, optional=optional)
        if output is None:
            return None
        try:
            value = json.loads(output)
        except json.JSONDecodeError:
            raise SetupError(f"{args[0]} returned invalid JSON; check the tool version") from None
        if not isinstance(value, dict):
            raise SetupError(f"{args[0]} returned an unexpected response; expected an object")
        return value


def gcloud(config: Config, *args: str) -> list[str]:
    """Select exactly one project for every cloud command."""
    return ["gcloud", *args, f"--project={config.project}", "--quiet"]


def ssh(config: Config, command: str) -> list[str]:
    """Use gcloud's existing SSH authentication and host-key handling."""
    args = gcloud(
        config,
        "compute",
        "ssh",
        config.name,
        f"--zone={config.zone}",
        f"--command={command}",
        "--ssh-flag=-oConnectionAttempts=12",
        "--ssh-flag=-oConnectTimeout=10",
    )
    if config.internal_ip:
        args.append("--internal-ip")
    return args


def inspect_instance(config: Config, run: Runner) -> dict[str, Any] | None:
    """Read the named VM; permission failures are never treated as absence."""
    instance = run.json(
        gcloud(config, "compute", "instances", "describe", config.name, f"--zone={config.zone}", "--format=json"),
        optional=True,
    )
    if instance is None:
        return None
    metadata = {item.get("key"): item.get("value") for item in instance.get("metadata", {}).get("items", [])}
    interfaces = instance.get("networkInterfaces", [])
    network = interfaces[0] if len(interfaces) == 1 else {}
    checks = (
        instance.get("name") == config.name,
        instance.get("labels", {}).get(OWNER_LABEL) == config.name,
        metadata.get("branchyard-configuration-sha256") == config.fingerprint,
        instance.get("machineType", "").rsplit("/", 1)[-1] == config.machine_type,
        instance.get("zone", "").rsplit("/", 1)[-1] == config.zone,
        bool(instance.get("advancedMachineFeatures", {}).get("enableNestedVirtualization"))
        == (config.backend == "microsandbox"),
        [item.get("email") for item in instance.get("serviceAccounts", [])]
        == ([config.service_account] if config.service_account else []),
        network.get("network", "").rsplit("/", 1)[-1] == config.network,
        network.get("subnetwork", "").rsplit("/", 1)[-1] == config.subnet,
        bool(network.get("accessConfigs")) != config.internal_ip,
    )
    if not all(checks):
        raise SetupError("existing VM is foreign or differs from the reviewed configuration; choose another --name")
    if instance.get("status") != "RUNNING":
        raise SetupError("owned VM is not RUNNING; start it with gcloud compute instances start, then rerun")
    disks = [disk for disk in instance.get("disks", []) if disk.get("boot")]
    if len(disks) != 1 or not disks[0].get("autoDelete"):
        raise SetupError("owned VM has an unexpected boot disk; inspect its disk and cleanup policy before retrying")
    disk = (
        run.json(
            gcloud(
                config,
                "compute",
                "disks",
                "describe",
                disks[0].get("source", "").rsplit("/", 1)[-1],
                f"--zone={config.zone}",
                "--format=json",
            )
        )
        or {}
    )
    source = disk.get("sourceImage", "")
    try:
        capacity = int(disk.get("sizeGb", 0))
    except (TypeError, ValueError):
        capacity = 0
    if capacity < config.disk_size or not re.search(
        r"/projects/ubuntu-os-cloud/global/images/ubuntu-2404-[^/]+$", source
    ):
        raise SetupError("owned VM boot disk is too small or not the selected Ubuntu 24.04 image")
    return instance


def bootstrap(config: Config, head: str | None) -> str:
    """Build the secret-free remote install script from validated values."""
    image = shlex.quote(config.image)
    release = shlex.quote(config.version)
    root = shlex.quote(config.remote_path)
    runtime_flag = " --runtime" if config.backend == "microsandbox" else ""
    script = f"""set -eu
umask 077
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
sudo apt-get update -q
sudo apt-get install -y -q git curl ca-certificates docker.io libcap-ng0 libstdc++6 python3
sudo systemctl enable --now docker
release_url=https://github.com/vamsiramakrishnan/branchyard/releases/download/v{config.version}
curl --proto '=https' --tlsv1.2 -fsSL "$release_url/install.sh" -o "$stage/install.sh"
sudo sh "$stage/install.sh" --version {release} --prefix /usr/local --no-setup{runtime_flag}
sudo docker pull {image}
container=$(sudo docker create {image})
trap 'sudo docker rm -f "$container" >/dev/null; rm -rf "$stage"' EXIT
sudo docker cp "$container:/usr/local/bin/agy" "$stage/agy"
sudo install -m 0755 "$stage/agy" /usr/local/bin/agy
sudo mkdir -p {root}
sudo chown "$(id -u):$(id -g)" {root}
cd {root}
"""
    if head:
        bundle = shlex.quote(f"/tmp/branchyard-{config.name}.bundle")
        script += f"""if [ ! -d .git ]; then git init -q -b main; fi
if ! git rev-parse --verify HEAD >/dev/null 2>&1; then
  [ -z "$(git status --porcelain)" ] || {{ echo 'remote workspace is not empty' >&2; exit 1; }}
  git fetch {bundle} HEAD
  git reset --hard FETCH_HEAD
fi
[ "$(git rev-parse HEAD)" = {shlex.quote(head)} ] || {{
  echo 'remote HEAD differs; reconcile it before retrying' >&2; exit 1;
}}
rm -f -- {bundle}
"""
    else:
        script += """if [ ! -d .git ]; then git init -q -b main; fi
if ! git rev-parse --verify HEAD >/dev/null 2>&1; then
  git -c user.name='Branchyard setup' -c user.email=setup@example.invalid \\
    commit --allow-empty -qm 'Initialize remote workspace'
fi
"""
    if config.backend == "microsandbox":
        script += f"""sudo usermod -aG kvm "$(id -un)"
sudo install -d -o "$(id -u)" -g "$(id -g)" -m 0700 /var/lib/by-msb
msb_url=https://github.com/superradcompany/microsandbox/releases/download/v{MSB_VERSION}
curl --proto '=https' --tlsv1.2 -fsSL "$msb_url/microsandbox-linux-x86_64.tar.gz" -o "$stage/msb.tar.gz"
printf '%s  %s\\n' {MSB_ARCHIVE_SHA256} "$stage/msb.tar.gz" | sha256sum -c -
tar -xzf "$stage/msb.tar.gz" -C "$stage"
mkdir -p /var/lib/by-msb/bin /var/lib/by-msb/lib
install -m 0755 "$stage/msb" /var/lib/by-msb/bin/msb
install -m 0644 "$stage/libkrunfw.so.5.6.1" /var/lib/by-msb/lib/libkrunfw.so.5.6.1
MSB_HOME=/var/lib/by-msb /var/lib/by-msb/bin/msb context --format json
sudo docker save {image} | MSB_HOME=/var/lib/by-msb /var/lib/by-msb/bin/msb image load --tag {image}
"""
    script += worker_configuration(config)
    script += "/usr/local/bin/by --version\n/usr/local/bin/agy --version\n"
    return script


def worker_configuration(config: Config) -> str:
    """Configure the owned worker through Branchyard's native project/server engines."""
    template = r"""state="$HOME/.local/state/branchyard/gce/@@NAME@@"
mkdir -p "$state"
chmod 0700 "$state"
sudo install -d /usr/local/libexec/branchyard
sudo mv -f /usr/local/bin/by /usr/local/libexec/branchyard/by
cat > "$stage/by" <<'BY_WRAPPER'
#!/bin/sh
export BRANCHYARD_USER_CONFIG="$HOME/.local/state/branchyard/gce/@@NAME@@/config.toml"
@@RUNTIME_ENV@@
if [ "${1-}" = serve ]; then
  for argument do
    case "$argument" in
      --config|--config=*) exec /usr/local/libexec/branchyard/by "$@" ;;
    esac
  done
  shift
  exec /usr/local/libexec/branchyard/by serve --config "$HOME/.local/state/branchyard/gce/@@NAME@@/server.json" "$@"
fi
exec /usr/local/libexec/branchyard/by "$@"
BY_WRAPPER
sudo install -m 0755 "$stage/by" /usr/local/bin/by
export BRANCHYARD_USER_CONFIG="$state/config.toml"
python3 - "$stage" "$state" <<'BY_CONFIG'
import json, sys
from pathlib import Path
stage, state = map(Path, sys.argv[1:])
source = "@" + str(state / "secrets/GEMINI_API_KEY")
project = {"harness": "antigravity", "isolated": True, "remote": "local",
    "provider": "@@BACKEND@@", "secrets": ["GEMINI_API_KEY"], "secret.GEMINI_API_KEY": source}
if project["provider"] == "microsandbox":
    project["microsandbox.image"] = @@IMAGE@@
server = {"path": str(state / "server.json"), "listen": "127.0.0.1:8421",
    "database": "sqlite", "tenancy": "single", "tls": False,
    "allow_providers": ["microsandbox"] if project["provider"] == "microsandbox" else [],
    "secrets": ["GEMINI_API_KEY"], "secret.GEMINI_API_KEY": source}
for name, value in [("project", project), ("server", server)]:
    (stage / (name + "-answers.json")).write_text(json.dumps(value))
BY_CONFIG
/usr/local/bin/by init project --defaults --apply --force --answers "$stage/project-answers.json" --json
/usr/local/bin/by init server --defaults --apply --force --answers "$stage/server-answers.json" --json
python3 - "$state/server.json" <<'BY_SERVER'
import json, sys
from pathlib import Path
path = Path(sys.argv[1])
config = json.loads(path.read_text())
config["harness_commands"] = {"antigravity": ["/usr/local/bin/agy"]}
config["allow_unapproved_tools"] = True
config["allow_client_commands"] = False
path.write_text(json.dumps(config, indent=2) + "\n")
path.chmod(0o600)
BY_SERVER
/usr/local/bin/by config validate
/usr/local/bin/by serve --config "$state/server.json" --check
"""
    return (
        template.replace("@@NAME@@", config.name)
        .replace("@@BACKEND@@", config.backend)
        .replace("@@IMAGE@@", json.dumps(config.image))
        .replace("@@RUNTIME_ENV@@", "export MSB_HOME=/var/lib/by-msb" if config.backend == "microsandbox" else "")
    )


def managed_ssh(config: Config, dry_run: str, run: Runner) -> Path:
    """Install one owned Host entry, refusing an existing alias or local symlink."""
    try:
        words = shlex.split(dry_run)
    except ValueError:
        raise SetupError("gcloud returned an incomplete SSH command; rerun its --dry-run privately") from None
    destinations = [word for word in words if re.fullmatch(r"[^\s@]+@[a-zA-Z0-9.:_-]+", word)]
    if not destinations:
        raise SetupError("gcloud did not report an SSH user and host; check gcloud compute ssh --dry-run")
    user, host = destinations[-1].rsplit("@", 1)
    options: dict[str, str] = {"HostName": host, "User": user, "IdentitiesOnly": "yes", "StrictHostKeyChecking": "yes"}
    for index, word in enumerate(words[:-1]):
        if word == "-i":
            options["IdentityFile"] = words[index + 1]
        elif word == "-o" and "=" in words[index + 1]:
            key, value = words[index + 1].split("=", 1)
            if key in {"HostKeyAlias", "UserKnownHostsFile", "CheckHostIP", "IdentityFile"}:
                options[key] = value
    if "IdentityFile" not in options or "UserKnownHostsFile" not in options:
        raise SetupError("gcloud did not report its identity and known-hosts files; refusing an incomplete SSH entry")
    if any("\n" in value or "\r" in value for value in options.values()):
        raise SetupError("gcloud returned an unusable SSH configuration value")
    directory = Path.home() / ".ssh"
    directory.mkdir(mode=0o700, exist_ok=True)
    owned = directory / "branchyard"
    owned.mkdir(mode=0o700, exist_ok=True)
    path = owned / f"{config.alias}.conf"
    marker = f"# Branchyard GCE {config.project}/{config.zone}/{config.name}\n"
    if path.is_symlink() or (path.exists() and not path.read_text().startswith(marker)):
        raise SetupError("deployment SSH entry is not owned by this helper; choose another --name")
    if not path.exists():
        resolved = run.run(["ssh", "-G", config.alias]) or ""
        hostname = next((line.partition(" ")[2] for line in resolved.splitlines() if line.startswith("hostname ")), "")
        if hostname and hostname != config.alias:
            raise SetupError("SSH alias already resolves through another configuration; choose another --name")
    body = marker + f"Host {config.alias}\n"
    body += "".join(f"  {key} {json.dumps(value)}\n" for key, value in options.items())
    default = directory / "config"
    previous = default.read_text() if default.exists() else ""
    include = f"Include {json.dumps(str(owned / '*.conf'))}\n"
    has_include = include.rstrip() in (line.rstrip() for line in previous.splitlines())
    if default.is_symlink() and not has_include:
        raise SetupError(f"~/.ssh/config is a symlink; add {include.strip()} manually, then retry")
    path.write_text(body)
    path.chmod(0o600)
    if not has_include:
        default.write_text(include + previous)
        default.chmod(0o600)
    return path


def apply(config: Config) -> dict[str, Any]:
    """Converge the owned VM and native SSH connection to the reviewed configuration."""
    config.validate()
    secret = None
    if config.forward_secret:
        secret = os.environ.get(config.forward_secret, "").strip()
        if not secret or len(secret) > 16384 or any(not 33 <= ord(c) <= 126 for c in secret):
            raise SetupError(f"export a single nonempty {config.forward_secret}, then rerun --forward-secret")
    for tool in ("gcloud", "ssh", "git"):
        if shutil.which(tool) is None:
            raise SetupError(f"install {tool}, then rerun the same command")
    config.state_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    config.state_dir.chmod(0o700)
    run = Runner(config)
    if not run.run(["gcloud", "auth", "list", "--filter=status:ACTIVE", "--format=value(account)"]):
        raise SetupError("no active gcloud account; run gcloud auth login, then retry")
    head = None
    if config.repo:
        if run.run(["git", "-C", str(config.repo), "status", "--porcelain"]):
            raise SetupError(
                "--repo has uncommitted or untracked changes; commit the intended files before copying HEAD"
            )
        head = run.run(["git", "-C", str(config.repo), "rev-parse", "--verify", "HEAD"])
        if not head or not re.fullmatch(r"[0-9a-f]{40,64}", head):
            raise SetupError("--repo needs a committed HEAD")
    if config.service_account:
        account = run.json(
            gcloud(config, "iam", "service-accounts", "describe", config.service_account, "--format=json")
        )
        if not account or account.get("email") != config.service_account:
            raise SetupError("could not verify the selected existing service account")
    instance = inspect_instance(config, run)
    if instance is None:
        args = gcloud(
            config,
            "compute",
            "instances",
            "create",
            config.name,
            f"--zone={config.zone}",
            f"--machine-type={config.machine_type}",
            f"--boot-disk-size={config.disk_size}GB",
            "--boot-disk-type=pd-balanced",
            "--image-family=ubuntu-2404-lts-amd64",
            "--image-project=ubuntu-os-cloud",
            f"--network={config.network}",
            f"--subnet={config.subnet}",
            f"--labels={OWNER_LABEL}={config.name}",
            f"--metadata=enable-oslogin=TRUE,branchyard-configuration-sha256={config.fingerprint}",
            "--no-scopes",
            f"--service-account={config.service_account}" if config.service_account else "--no-service-account",
        )
        if config.backend == "microsandbox":
            args.extend(["--enable-nested-virtualization", "--min-cpu-platform=Intel Cascade Lake"])
        if config.internal_ip:
            args.append("--no-address")
        run.run(args)
        instance = inspect_instance(config, run)
        if instance is None:
            raise SetupError("VM create returned without a verifiable machine; inspect the named VM and rerun")
    run.run(ssh(config, "true"))
    if config.repo:
        with tempfile.TemporaryDirectory(prefix="branchyard-gce-", dir=config.state_dir) as temporary:
            bundle = Path(temporary) / "source.bundle"
            run.run(["git", "-C", str(config.repo), "bundle", "create", str(bundle), "HEAD"])
            bundle.chmod(0o600)
            args = gcloud(
                config,
                "compute",
                "scp",
                str(bundle),
                f"{config.name}:/tmp/branchyard-{config.name}.bundle",
                f"--zone={config.zone}",
            )
            if config.internal_ip:
                args.append("--internal-ip")
            run.run(args)
    run.run(ssh(config, "bash -se"), script=bootstrap(config, head))
    if secret is not None:
        run.run(
            ssh(config, "/usr/local/bin/by harnesses login antigravity --api-key --json"),
            secret_input=secret,
        )
    check = "/usr/local/bin/by --version && /usr/local/bin/agy --version"
    if config.backend == "microsandbox":
        check += " && test -r /dev/kvm && test -w /dev/kvm"
    run.run(ssh(config, check))
    args = gcloud(config, "compute", "ssh", config.name, f"--zone={config.zone}", "--dry-run")
    if config.internal_ip:
        args.append("--internal-ip")
    path = managed_ssh(config, run.run(args) or "", run)
    run.run(["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15", config.alias, "/usr/local/bin/by --version"])
    result = {
        "mode": "applied",
        "project": config.project,
        "zone": config.zone,
        "name": config.name,
        "backend": config.backend,
        "remote": f"ssh://{config.alias}{config.remote_path}",
        "remote_command": "/usr/local/bin/by",
        "ssh_config": str(path),
        "source_head": head,
        "image": config.image,
        "version": config.version,
        "qualification": {"model": "not_checked", "microsandbox_hard_memory_ceiling": "not_qualified"},
        "credential_source": config.forward_secret,
        "credential_destination": "native by keystore; GEMINI_API_KEY",
        "project_dependencies": "not_run",
        "workspace_approval": {
            "review_command": shlex.join(
                ["ssh", config.alias, f"cd {shlex.quote(config.remote_path)} && /usr/local/bin/by workspace show"]
            ),
            "trust_command": shlex.join(
                ["ssh", config.alias, f"cd {shlex.quote(config.remote_path)} && /usr/local/bin/by workspace trust"]
            ),
            "server_policy": "allow_workspace_scripts needs explicit operator approval for this repository",
            "server_config": f"~/.local/state/branchyard/gce/{config.name}/server.json",
        },
    }
    destination = config.state_dir / "deployment.json"
    staged = destination.with_suffix(".json.new")
    staged.write_text(json.dumps(result, indent=2) + "\n")
    staged.chmod(0o600)
    staged.replace(destination)
    return result


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("project", "zone", "name"):
        parser.add_argument(f"--{name}", required=True)
    parser.add_argument("--state-dir", type=Path)
    parser.add_argument("--version", default=RELEASE_VERSION)
    parser.add_argument("--machine-type", default="n2-standard-4")
    parser.add_argument(
        "--service-account", help="existing account; omitted means no account, never the project default"
    )
    parser.add_argument("--image", help="defaults to the published worker image matching --version")
    parser.add_argument("--backend", choices=("local", "microsandbox"), default="local")
    parser.add_argument("--repo", type=Path, help="copy only this repository's clean committed HEAD")
    parser.add_argument("--network", default="default")
    parser.add_argument("--subnet", default="default")
    parser.add_argument("--disk-size", type=int, default=100)
    parser.add_argument("--internal-ip", action="store_true", help="private VM; requires an existing operator route")
    parser.add_argument(
        "--forward-secret",
        choices=("GEMINI_API_KEY", "GOOGLE_API_KEY"),
        help="read this environment variable and store it via SSH stdin in the worker's native 0600 keystore",
    )
    parser.add_argument("--apply", action="store_true", help="create/reuse the named VM and prepare its SSH remote")
    args = vars(parser.parse_args(argv))
    should_apply = args.pop("apply")
    if args["image"] is None:
        args["image"] = f"{IMAGE_REPOSITORY}:{args['version']}"
    if args["state_dir"] is None:
        args["state_dir"] = (
            Path.home() / ".local/state/branchyard/compute-gce" / args["project"] / args["zone"] / args["name"]
        )
    for key in ("state_dir", "repo"):
        if args[key] is not None:
            args[key] = args[key].expanduser().resolve()
    config = Config(**args)
    try:
        print(json.dumps(apply(config) if should_apply else plan(config), indent=2))
    except SetupError as error:
        print(f"compute-gce: {error}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("compute-gce: interrupted; rerun the same command to inspect and resume", file=sys.stderr)
        return 130
    return 0


if __name__ == "__main__":
    sys.exit(main())
