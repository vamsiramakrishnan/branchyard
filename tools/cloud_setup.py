#!/usr/bin/env python3
"""Connect the guided CLI to dedicated cloud deployment and Substrate setup."""

import argparse
import hashlib
import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

import substrate_gke as gke


def command(args: list[str], *, cwd: Path | None = None, timeout: int = 120, input_text: str | None = None) -> str:
    """Run a bounded operation, never copying provider diagnostics into errors."""
    tool = Path(args[0]).name
    label = tool if tool in {"by", "git", "gcloud", "kubectl", "python3", "python"} else "setup tool"
    try:
        result = subprocess.run(
            args, cwd=cwd, input=input_text, text=True, capture_output=True, timeout=timeout, check=False
        )
    except (OSError, subprocess.TimeoutExpired):
        raise gke.SetupError(f"Could not finish {label}; check installation and connectivity, then retry") from None
    if result.returncode:
        raise gke.SetupError(f"{label} failed; check access and retry")
    return result.stdout.strip()


def image_for_release(version: str) -> str:
    """Read a published immutable actor image from the matching release."""
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:[-+][A-Za-z0-9.-]+)?", version):
        raise gke.SetupError("invalid Branchyard release version")
    url = f"https://github.com/vamsiramakrishnan/branchyard/releases/download/v{version}/runtime-images.json"
    try:
        with urllib.request.urlopen(url, timeout=30) as response:
            if urlsplit(response.geturl()).scheme != "https":
                raise gke.SetupError("release metadata redirected outside HTTPS")
            raw = response.read(65537)
        if len(raw) > 65536:
            raise gke.SetupError("release metadata exceeds its size limit")
        metadata = json.loads(raw)
    except (OSError, ValueError):
        raise gke.SetupError(
            "Could not read this release's runtime image; check the published release and retry"
        ) from None
    if not isinstance(metadata, dict) or metadata.get("branchyard_version") != version:
        raise gke.SetupError("runtime image metadata does not match this Branchyard release")
    image = metadata.get("agy_image", "")
    if not isinstance(image, str) or not re.fullmatch(
        r"ghcr\.io/vamsiramakrishnan/branchyard-agy@sha256:[0-9a-f]{64}", image
    ):
        raise gke.SetupError("release metadata contains no pinned Branchyard agy image")
    return image


def gke_version(config: dict[str, Any]) -> str:
    """Select an available release compatible with the pinned certificate APIs."""
    versions = config.get("validMasterVersions", [])
    if not isinstance(versions, list):
        raise gke.SetupError("gcloud returned an invalid GKE version list")
    compatible = [
        version
        for version in versions
        if isinstance(version, str) and re.fullmatch(r"1\.(?:36|37)\.\d+-gke\.\d+", version)
    ]
    if compatible:
        return max(compatible, key=lambda version: tuple(int(part) for part in re.findall(r"\d+", version)))
    raise gke.SetupError("this zone offers no compatible GKE 1.36/1.37 release; choose another zone")


def private_file(path: Path, text: str) -> None:
    """Replace operator-owned state with private permissions."""
    with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, prefix=f".{path.name}.", delete=False) as output:
        temporary = Path(output.name)
        try:
            output.write(text)
            output.flush()
            os.fsync(output.fileno())
            temporary.replace(path)
        finally:
            temporary.unlink(missing_ok=True)


def ensure_source(path: Path) -> None:
    """Cache exactly the source pin; interrupted preparation cannot poison its destination."""
    if not path.exists():
        with tempfile.TemporaryDirectory(prefix=".substrate-source-", dir=path.parent) as temporary:
            checkout = Path(temporary) / "checkout"
            command(["git", "clone", "--filter=blob:none", "--no-checkout", gke.SOURCE_URL, str(checkout)], timeout=300)
            command(["git", "checkout", "--detach", gke.SOURCE_PIN], cwd=checkout, timeout=300)
            if not path.exists():
                checkout.replace(path)
    revision = command(["git", "rev-parse", "HEAD"], cwd=path)
    if revision != gke.SOURCE_PIN or command(["git", "status", "--porcelain"], cwd=path):
        raise gke.SetupError("cached upstream source differs from its pin; inspect the private source cache")


def forwarding_command(args: argparse.Namespace) -> list[str]:
    """Let kubectl bind an ephemeral port atomically when no recorded port exists."""
    local = str(args.port) if args.port else ""
    return [
        "kubectl",
        f"--kubeconfig={args.kubeconfig}",
        f"--context={args.context}",
        "-n",
        "ate-system",
        "port-forward",
        f"svc/{args.service}",
        f"{local}:{args.remote_port}",
        "--address=127.0.0.1",
    ]


def process_matches(pid: int, expected: list[str]) -> bool:
    """Reuse only an actual process launched with this deployment's exact arguments."""
    if pid < 1:
        return False
    try:
        proc = Path(f"/proc/{pid}/cmdline")
        if Path("/proc").is_dir():
            return proc.read_bytes().rstrip(b"\0").split(b"\0") == [os.fsencode(arg) for arg in expected]
        observed = command(["ps", "-p", str(pid), "-o", "command="])
        return observed == " ".join(expected)
    except (OSError, gke.SetupError):
        return False


def tunnel(args: argparse.Namespace) -> int:
    """Restart failed transports without ever replaying an application request."""
    ready = Path(args.ready_file)
    stopped = threading.Event()
    child: subprocess.Popen[str] | None = None

    def stop(signum: int, frame: Any) -> None:
        stopped.set()
        if child is not None and child.poll() is None:
            child.terminate()

    previous = {sig: signal.signal(sig, stop) for sig in (signal.SIGINT, signal.SIGTERM)}
    env = dict(os.environ, KUBECTL_PORT_FORWARD_WEBSOCKETS="false")
    try:
        while not stopped.is_set():
            ready.unlink(missing_ok=True)
            command_args = forwarding_command(args)
            child = subprocess.Popen(
                command_args,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                env=env,
            )
            try:
                if child.stdout is None:
                    raise gke.SetupError("kubectl forwarding output was not connected")
                for line in child.stdout:
                    print(line, end="", flush=True)
                    match = re.fullmatch(r"Forwarding from 127\.0\.0\.1:(\d+) -> \d+\s*", line)
                    if match:
                        args.port = int(match[1])
                        private_file(
                            ready,
                            json.dumps(
                                {
                                    "pid": os.getpid(),
                                    "child_pid": child.pid,
                                    "port": args.port,
                                    "child_command": command_args,
                                }
                            ),
                        )
                child.wait()
            finally:
                ready.unlink(missing_ok=True)
                if child.poll() is None:
                    child.terminate()
                    try:
                        child.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait(timeout=5)
                if child.stdout is not None:
                    child.stdout.close()
            if stopped.wait(1):
                break
    finally:
        ready.unlink(missing_ok=True)
        for sig, handler in previous.items():
            signal.signal(sig, handler)
    return 0


def ready_tunnel(ready_file: Path, pid: int, expected: list[str]) -> int | None:
    """Accept a listener only after its owned kubectl child reports successful binding."""
    if not process_matches(pid, expected):
        return None
    try:
        record = json.loads(ready_file.read_text())
        port = record["port"]
        child_pid = record["child_pid"]
        child_args = record["child_command"]
        settings = argparse.Namespace(
            **{
                key.replace("-", "_"): expected[expected.index(f"--{key}") + 1]
                for key in ("kubeconfig", "context", "service", "remote-port", "port")
            }
        )
        settings.port = int(settings.port)
        original_args = forwarding_command(settings)
        settings.port = port
        bound_args = forwarding_command(settings)
        if (
            record["pid"] != pid
            or type(port) is not int
            or not 1 <= port <= 65535
            or type(child_pid) is not int
            or not isinstance(child_args, list)
            or not child_args
            or child_args[0] != "kubectl"
            or child_args not in (original_args, bound_args)
            or not process_matches(child_pid, child_args)
        ):
            return None
        with socket.create_connection(("127.0.0.1", port), timeout=1):
            return port
    except (OSError, ValueError, KeyError, TypeError):
        return None


def ensure_tunnel(config: gke.Config, service: str, remote: int, port: int) -> int:
    """Wait for a dedicated supervisor and its verified listener before returning."""
    state = config.state_dir
    ready = state / f"{service}.ready.json"
    expected = [
        sys.executable,
        str(Path(__file__).resolve()),
        "--tunnel",
        "--kubeconfig",
        str(config.kube_path),
        "--context",
        config.kube_context,
        "--service",
        service,
        "--port",
        str(port),
        "--remote-port",
        str(remote),
        "--ready-file",
        str(ready),
    ]
    pid_path = state / f"{service}.pid"
    process = None
    if pid_path.exists():
        try:
            record = json.loads(pid_path.read_text())
            pid, requested = record["pid"], record["requested_port"]
            if type(pid) is not int or pid < 1 or type(requested) is not int or requested not in {0, port}:
                raise ValueError
            expected[expected.index("--port") + 1] = str(requested)
        except (ValueError, KeyError, TypeError):
            raise gke.SetupError(
                "invalid saved tunnel PID; inspect this deployment's private connection files"
            ) from None
        if process_matches(pid, expected):
            found = ready_tunnel(ready, pid, expected)
            if found is not None:
                return found
        else:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                pass
            else:
                raise gke.SetupError("saved tunnel PID belongs to another process; inspect it before retrying")
    else:
        pid = 0
    if not pid or not process_matches(pid, expected):
        expected[expected.index("--port") + 1] = str(port)
        ready.unlink(missing_ok=True)
        with open(state / f"{service}.log", "a", opener=lambda name, flags: os.open(name, flags, 0o600)) as log:
            os.fchmod(log.fileno(), 0o600)
            process = subprocess.Popen(
                expected, stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True
            )
        pid = process.pid
        private_file(pid_path, json.dumps({"pid": pid, "requested_port": port}))
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        found = ready_tunnel(ready, pid, expected)
        if found is not None:
            return found
        if process is not None and process.poll() is not None:
            break
        threading.Event().wait(0.05)
    raise gke.SetupError(f"{service} forwarding did not become ready; inspect its private log, check ports and rerun")


def connect(config: gke.Config) -> dict[str, Any]:
    """Mint Control credentials and create supervised local endpoint forwards."""
    state = config.state_dir
    token = command(
        gke.kubectl(
            config, "create", "token", "default", "-n", "ate-system", "--audience=api.ate-system.svc", "--duration=24h"
        )
    )
    if not re.fullmatch(r"[A-Za-z0-9._~-]{16,}", token):
        raise gke.SetupError("Control token command returned no usable token; check Kubernetes access and retry")
    private_file(state / "control.token", token + "\n")
    bundles = json.loads(command(gke.kubectl(config, "get", "clustertrustbundles", "-o", "json")))
    if not isinstance(bundles, dict) or not isinstance(bundles.get("items"), list):
        raise gke.SetupError("Kubernetes returned an invalid service DNS trust bundle response")
    ca = "".join(
        item["spec"]["trustBundle"]
        for item in bundles["items"]
        if isinstance(item, dict)
        and isinstance(item.get("spec"), dict)
        and item["spec"].get("signerName") == "servicedns.podcert.ate.dev/identity"
        and isinstance(item["spec"].get("trustBundle"), str)
    )
    if not ca:
        raise gke.SetupError("Substrate has no service DNS trust bundle; inspect the controller and retry")
    private_file(state / "control-ca.pem", ca)
    ports_file = state / "ports.json"
    ports = json.loads(ports_file.read_text()) if ports_file.exists() else {"api": 0, "atenet-router": 0}
    if not isinstance(ports, dict) or any(
        type(ports.get(service)) is not int or not 0 <= ports[service] <= 65535 for service in ("api", "atenet-router")
    ):
        raise gke.SetupError("invalid saved forwarding ports; inspect this deployment's private connection files")
    for service, remote in (("api", 443), ("atenet-router", 80)):
        ports[service] = ensure_tunnel(config, service, remote, ports[service])
    if ports["api"] == ports["atenet-router"]:
        raise gke.SetupError("API and router forwarding unexpectedly share a port; inspect this deployment")
    private_file(ports_file, json.dumps(ports))
    return {
        "provider": "substrate",
        "harness": "antigravity",
        "isolated": True,
        "substrate.endpoint": f"https://127.0.0.1:{ports['api']}",
        "substrate.router": f"http://127.0.0.1:{ports['atenet-router']}/",
        "substrate.atespace": "branchyard",
        "substrate.template": f"agy-{config.runtime}",
        "substrate.key": str(state / "bridge.key"),
        "substrate.api_token_file": str(state / "control.token"),
        "substrate.ca": str(state / "control-ca.pem"),
        "substrate.server_name": "api.ate-system.svc",
        "substrate.egress": "unrestricted",
    }


def apply_gke(args: argparse.Namespace, state: Path, image: str) -> dict[str, Any]:
    """Provision pinned Substrate and verify its generated bridge template."""
    source = state / "substrate-source"
    ensure_source(source)
    available = json.loads(
        command(
            [
                "gcloud",
                "container",
                "get-server-config",
                f"--project={args.project}",
                f"--zone={args.zone}",
                "--format=json",
            ]
        )
    )
    region = args.zone.rsplit("-", 1)[0]
    config = gke.Config(
        project=args.project,
        zone=args.zone,
        cluster=args.name,
        bucket=bucket_name(args.project, args.name),
        registry=f"{region}-docker.pkg.dev/{args.project}/{args.name}",
        version=gke_version(available),
        service_account=args.service_account or account_email(args.project, args.zone, args.name),
        source=source,
        state_dir=state,
        runtime=args.runtime,
        machine_type="n2-standard-8",
        nodes=1,
        worker_replicas=2,
    )
    run = gke.Runner(config)
    gke.prerequisites(config, run)
    if not args.service_account:
        ensure_node_account(config, run)
    gke.apply(config, run)
    answers = connect(config)
    fixture = state / "template-setup"
    fixture.mkdir(exist_ok=True)
    command(["git", "init", "-q", str(fixture)])
    by = args.by
    command([by, "init", "project", "--defaults", "--answers", json.dumps(answers), "--apply", "--json"], cwd=fixture)
    print("Preparing the bridge image template and checking actor execution...", file=sys.stderr, flush=True)
    command(
        [
            by,
            "substrate",
            "setup",
            "--image",
            image,
            "--storage",
            f"gs://{config.bucket}/branchyard",
            "--runtime",
            args.runtime,
            "--sandbox-config",
            "microvm" if args.runtime == "microvm" else "gvisor-default",
            "--worker-label",
            "workload=branchyard",
            "--worker-label",
            f"branchyard-runtime={args.runtime}",
            "--yes",
            "--json",
            "--cpu",
            "1",
            "--memory",
            "2Gi",
        ],
        cwd=fixture,
        timeout=600,
    )
    command([by, "substrate", "check", "--smoke", "--harness", "agy", "--json"], cwd=fixture, timeout=600)
    private_file(state / "project-answers.json", json.dumps(answers))
    return answers


def bucket_name(project: str, name: str) -> str:
    """Derive a stable bucket name within the Cloud Storage length limit."""
    value = f"{project}-{name}-snapshots"
    if len(value) <= 63:
        return value
    return f"{value[:54].rstrip('-')}-{hashlib.sha256(value.encode()).hexdigest()[:8]}"


def account_email(project: str, zone: str, name: str) -> str:
    """Give an automatically created node account a bounded, deployment-specific name."""
    suffix = hashlib.sha256(f"{project}/{zone}/{name}".encode()).hexdigest()[:8]
    return f"by-{name[:18]}-{suffix}@{project}.iam.gserviceaccount.com"


def ensure_node_account(config: gke.Config, run: gke.Runner) -> None:
    """Create only an owned node identity and its explicit unconditional node role."""
    marker = f"Branchyard deployment {config.project}/{config.zone}/{config.cluster}"
    args = gke.gcloud(config, "iam", "service-accounts", "describe", config.service_account, "--format=json")
    account = run.json(args, optional=True)
    if account is None:
        run.run(
            gke.gcloud(
                config,
                "iam",
                "service-accounts",
                "create",
                config.service_account.split("@", 1)[0],
                f"--description={marker}",
                f"--display-name=Branchyard {config.cluster}",
            )
        )
        account = run.json(args)
    if (
        not account
        or account.get("email") != config.service_account
        or account.get("description") != marker
        or account.get("disabled")
    ):
        raise gke.SetupError("automatic node account is not owned by this deployment; choose another name")
    role = "roles/container.defaultNodeServiceAccount"
    member = f"serviceAccount:{config.service_account}"
    policy = run.json(gke.gcloud(config, "projects", "get-iam-policy", config.project, "--format=json")) or {}
    if not isinstance(policy.get("bindings", []), list) or not all(
        isinstance(binding, dict) for binding in policy.get("bindings", [])
    ):
        raise gke.SetupError("project returned an invalid IAM policy response")
    bound = any(
        binding.get("role") == role and member in binding.get("members", []) and not binding.get("condition")
        for binding in policy.get("bindings", [])
    )
    if not bound:
        run.run(
            gke.gcloud(
                config,
                "projects",
                "add-iam-policy-binding",
                config.project,
                f"--member={member}",
                f"--role={role}",
                "--condition=None",
            )
        )
    private_file(
        config.state_dir / "node-account.json",
        json.dumps(
            {
                "email": config.service_account,
                "role": role,
                "ownership": marker,
            }
        ),
    )


def validate(args: argparse.Namespace) -> None:
    """Validate all caller-controlled values before subprocesses or state writes."""
    if args.refresh_state:
        if (
            args.tunnel
            or args.apply
            or any(
                getattr(args, key)
                for key in (
                    "kind",
                    "project",
                    "zone",
                    "name",
                    "service_account",
                    "runtime",
                    "repo",
                    "version",
                    "forward_secret",
                )
            )
        ):
            raise gke.SetupError("refresh-state takes only a saved deployment directory")
        return
    if args.tunnel:
        if (
            not args.kubeconfig
            or not args.context
            or args.context.startswith("-")
            or any(character.isspace() for character in args.context)
            or args.service not in {"api", "atenet-router"}
            or not args.ready_file
            or not Path(args.ready_file).is_absolute()
            or args.port is None
            or not 0 <= args.port <= 65535
            or args.remote_port != {"api": 443, "atenet-router": 80}.get(args.service)
        ):
            raise gke.SetupError("invalid internal tunnel configuration")
        return
    for field in ("project", "name"):
        if not re.fullmatch(r"[a-z][a-z0-9-]{1,28}[a-z0-9]", getattr(args, field) or ""):
            raise gke.SetupError(f"{field} must be a 3-30 character lowercase resource name")
    if not re.fullmatch(r"[a-z]+-[a-z]+\d-[a-z]", args.zone or ""):
        raise gke.SetupError("zone must name a zone such as us-central1-a")
    if args.kind not in {"gke", "gce"}:
        raise gke.SetupError("choose gke or gce")
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:[-+][A-Za-z0-9.-]+)?", args.version or ""):
        raise gke.SetupError("version must pin a Branchyard release")
    allowed = {"gke": {"gvisor", "microvm"}, "gce": {"local", "microsandbox"}}
    args.runtime = args.runtime or ("gvisor" if args.kind == "gke" else "local")
    if args.runtime not in allowed[args.kind]:
        raise gke.SetupError("runtime does not belong to the selected compute backend")
    if args.service_account and not re.fullmatch(
        rf"[a-z][a-z0-9-]+@{re.escape(args.project)}\.iam\.gserviceaccount\.com", args.service_account
    ):
        raise gke.SetupError("service account must name an existing account in the selected project")
    if args.kind == "gce" and (not args.repo or not Path(args.repo).is_dir()):
        raise gke.SetupError("GCE setup needs an existing repository directory")
    if not args.by or any(character in args.by for character in "\r\n\0"):
        raise gke.SetupError("by executable must name one installed program")
    if args.forward_secret and args.kind != "gce":
        raise gke.SetupError("secret forwarding is available only for the selected GCE worker")
    if args.forward_secret and args.forward_secret not in {"GEMINI_API_KEY", "GOOGLE_API_KEY"}:
        raise gke.SetupError("forward-secret must name GEMINI_API_KEY or GOOGLE_API_KEY, never its value")


def refresh_state(state: Path) -> dict[str, Any]:
    """Refresh only an owned deployment's Control token and local transports."""
    state = state.expanduser().resolve()
    details = state.stat()
    if details.st_uid != os.getuid() or details.st_mode & 0o077:
        raise gke.SetupError("saved cloud state must be private and owned by the current user")
    saved = json.loads((state / "deployment.json").read_text())
    if (
        not isinstance(saved, dict)
        or saved.get("mode") != "applied"
        or saved.get("source_revision") != gke.SOURCE_PIN
        or saved.get("substrate_version") != gke.BUILD_VERSION
    ):
        raise gke.SetupError("saved deployment does not belong to this supported Substrate setup")
    for key in ("project", "cluster"):
        if not isinstance(saved.get(key), str) or not re.fullmatch(r"[a-z][a-z0-9-]{1,28}[a-z0-9]", saved[key]):
            raise gke.SetupError("saved deployment has an invalid resource identity")
    zone = saved.get("zone", "")
    if not isinstance(zone, str) or not re.fullmatch(r"[a-z]+-[a-z]+\d-[a-z]", zone):
        raise gke.SetupError("saved deployment has an invalid zone")
    context = f"gke_{saved['project']}_{zone}_{saved['cluster']}"
    if saved.get("context") != context or saved.get("kubeconfig") != str(state / "kubeconfig"):
        raise gke.SetupError("saved deployment does not use its dedicated kubeconfig and context")
    worker = saved.get("worker", {})
    runtime = worker.get("runtime") if isinstance(worker, dict) else None
    if runtime not in {"gvisor", "microvm"}:
        raise gke.SetupError("saved deployment has an unsupported actor runtime")
    cluster = json.loads(
        command(
            [
                "gcloud",
                "container",
                "clusters",
                "describe",
                saved["cluster"],
                f"--project={saved['project']}",
                f"--zone={zone}",
                "--format=json",
            ]
        )
    )
    if not isinstance(cluster, dict) or cluster.get("status") != "RUNNING":
        raise gke.SetupError("saved GKE cluster is not RUNNING; inspect its current operation before retrying")
    if cluster.get("resourceLabels", {}).get(gke.OWNER_LABEL) != saved["cluster"]:
        raise gke.SetupError("saved GKE cluster does not carry this deployment's ownership label")
    selected = command(
        [
            "kubectl",
            f"--kubeconfig={state / 'kubeconfig'}",
            f"--context={context}",
            "config",
            "view",
            "--minify",
            "-o",
            "jsonpath={.clusters[0].cluster.server}",
        ]
    )
    private = cluster.get("privateClusterConfig", {})
    endpoints = {cluster.get("endpoint"), private.get("privateEndpoint"), private.get("publicEndpoint")}
    parsed = urlsplit(selected)
    if parsed.scheme != "https" or not parsed.hostname or parsed.hostname not in endpoints:
        raise gke.SetupError("saved kubeconfig does not point at the owned GKE cluster")
    config = gke.Config(
        project=saved["project"],
        zone=zone,
        cluster=saved["cluster"],
        bucket=saved["bucket"],
        registry=saved["registry"],
        version=cluster.get("currentMasterVersion", ""),
        service_account=account_email(saved["project"], zone, saved["cluster"]),
        source=state / "substrate-source",
        state_dir=state,
        runtime=runtime,
    )
    config.validate()
    return connect(config)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tunnel", action="store_true")
    parser.add_argument("--refresh-state", type=Path)
    for field in ("kubeconfig", "context", "service", "ready-file"):
        parser.add_argument(f"--{field}")
    parser.add_argument("--port", type=int)
    parser.add_argument("--remote-port", type=int)
    for field in ("kind", "project", "zone", "name", "service-account", "runtime", "repo", "version"):
        parser.add_argument(f"--{field}")
    parser.add_argument("--forward-secret")
    parser.add_argument("--by", default=os.environ.get("BRANCHYARD_SETUP_BY", "by"))
    parser.add_argument("--apply", action="store_true")
    args = parser.parse_args(argv)
    try:
        validate(args)
        if args.tunnel:
            return tunnel(args)
        if args.refresh_state:
            print(json.dumps(refresh_state(args.refresh_state), indent=2))
            return 0
        state = Path.home() / ".local/state/branchyard/cloud" / args.kind / args.project / args.zone / args.name
        if not args.apply:
            print(
                json.dumps(
                    {
                        "kind": args.kind,
                        "project": args.project,
                        "zone": args.zone,
                        "name": args.name,
                        "runtime": args.runtime,
                        "service_account": args.service_account
                        or (account_email(args.project, args.zone, args.name) if args.kind == "gke" else None),
                        "node_role": "roles/container.defaultNodeServiceAccount"
                        if args.kind == "gke" and not args.service_account
                        else None,
                        "actor_egress": "unrestricted" if args.kind == "gke" else None,
                        "version": args.version,
                        "state": str(state),
                        "billing": "resources remain billable until deleted",
                        "forward_secret": args.forward_secret,
                        "model": "not called",
                    },
                    indent=2,
                )
            )
            return 0
        required = ["gcloud", "git", "python3"] + (["go", "kubectl", "docker"] if args.kind == "gke" else ["ssh"])
        for tool in required:
            if not shutil.which(tool):
                raise gke.SetupError(f"install {tool}, then rerun by setup --guided")
        image = image_for_release(args.version)
        state.mkdir(parents=True, exist_ok=True, mode=0o700)
        state.chmod(0o700)
        if args.kind == "gke":
            answers = apply_gke(args, state, image)
        else:
            command_args = [
                sys.executable,
                str(Path(__file__).with_name("compute_gce.py")),
                "--project",
                args.project,
                "--zone",
                args.zone,
                "--name",
                args.name,
                "--version",
                args.version,
                "--backend",
                args.runtime,
                "--image",
                image,
                "--repo",
                args.repo,
                "--state-dir",
                str(state),
                "--apply",
            ]
            if args.service_account:
                command_args.extend(["--service-account", args.service_account])
            if args.forward_secret:
                command_args.extend(["--forward-secret", args.forward_secret])
            deployment = json.loads(command(command_args, timeout=1800))
            if not isinstance(deployment, dict) or not isinstance(deployment.get("remote"), str):
                raise gke.SetupError("GCE helper returned no native SSH deployment URL")
            remote = deployment["remote"]
            if not re.fullmatch(r"ssh://branchyard-[a-z0-9-]+/srv/branchyard/[a-z0-9-]+", remote):
                raise gke.SetupError("GCE helper returned an unexpected SSH deployment URL")
            answers = {"remote": "server", "remote.url": remote, "harness": "antigravity"}
            if args.forward_secret:
                answers.update(
                    {
                        "isolated": True,
                        "secrets": ["GEMINI_API_KEY"],
                        "secret.GEMINI_API_KEY": args.forward_secret,
                    }
                )
            if args.runtime == "microsandbox":
                answers.update({"provider": "microsandbox", "microsandbox.image": image})
            if deployment.get("workspace_approval"):
                print(
                    "Remote workspace scripts need explicit review and trust; "
                    "follow the worker deployment record before its first task.",
                    file=sys.stderr,
                )
            if deployment.get("project_dependencies") == "not_run":
                print("Project dependency installation has not run on the worker.", file=sys.stderr)
        print(json.dumps(answers, indent=2))
        return 0
    except gke.SetupError as error:
        print(f"cloud setup: {error}", file=sys.stderr)
        return 1
    except (OSError, ValueError, KeyError, TypeError, AttributeError):
        print(
            "cloud setup: could not read deployment state or a provider response; "
            "check private files and access, then retry",
            file=sys.stderr,
        )
        return 1
    except KeyboardInterrupt:
        print("cloud setup: interrupted; rerun with the same deployment name to resume", file=sys.stderr)
        return 130


if __name__ == "__main__":
    sys.exit(main())
