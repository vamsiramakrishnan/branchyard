#!/usr/bin/env python3
"""Plan or provision a dedicated GKE cluster using a pinned Agent Substrate checkout."""

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
from dataclasses import asdict, dataclass, replace
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

SOURCE_PIN = "1d7ca8ced056192a1801d6565251adcaab3eb0c9"
SOURCE_URL = "https://github.com/agent-substrate/substrate.git"
BUILD_VERSION = SOURCE_PIN[:7]
AGENTGATEWAY_IMAGE = (
    "cr.agentgateway.dev/agentgateway:v0.0.0-alpha.9e78d1da"
    "@sha256:8177311eb0444df2829cc00410147e960a3bd7261492f2c8d52b48be6d693a45"
)
OWNER_LABEL = "branchyard-deployment"
VERSION_LABEL = "ate.dev/substrate-version"
CERTIFICATE_APIS = ("podcertificaterequests", "clustertrustbundles")
BETA_APIS = ",".join(f"certificates.k8s.io/v1beta1/{name}" for name in CERTIFICATE_APIS)


class SetupError(Exception):
    """An actionable setup failure, containing no command output or credentials."""


@dataclass(frozen=True)
class Config:
    project: str
    zone: str
    cluster: str
    bucket: str
    registry: str
    version: str
    service_account: str
    source: Path
    state_dir: Path
    machine_type: str = "c3-standard-4"
    nodes: int = 2
    disk_size: int = 100
    network: str = "default"
    subnet: str = "default"
    private_nodes: bool = False
    master_cidr: str = "172.28.0.0/28"
    kubeconfig: Path | None = None
    context: str | None = None
    adopt_existing: bool = False
    rollout_timeout: int = 300
    runtime: str = "gvisor"
    worker_namespace: str = "branchyard"
    worker_name: str | None = None
    worker_replicas: int = 1
    atenet_dataplane: str = "envoy"

    @property
    def pool_name(self) -> str:
        return self.worker_name or f"branchyard-{self.runtime}"

    @property
    def region(self) -> str:
        return self.zone.rsplit("-", 1)[0]

    @property
    def repository(self) -> str:
        return self.registry.rsplit("/", 1)[1]

    @property
    def kube_path(self) -> Path:
        return self.kubeconfig or self.state_dir / "kubeconfig"

    @property
    def kube_context(self) -> str:
        return self.context or f"gke_{self.project}_{self.zone}_{self.cluster}"

    def validate(self) -> None:
        if self.state_dir.is_relative_to(self.source):
            raise SetupError("--state-dir must be outside the clean upstream checkout")
        for name in ("project", "cluster", "network", "subnet"):
            if not re.fullmatch(r"[a-z][a-z0-9-]{0,61}[a-z0-9]", getattr(self, name)):
                raise SetupError(f"--{name} must be a lowercase Google Cloud resource name")
        if not re.fullmatch(r"[a-z]+-[a-z]+\d-[a-z]", self.zone):
            raise SetupError("--zone must be a zone such as us-central1-a")
        if not re.fullmatch(r"[a-z0-9][a-z0-9-]{1,61}[a-z0-9]", self.bucket):
            raise SetupError("--bucket must be a 3-63 character bucket name without dots")
        registry_prefix = f"{self.region}-docker.pkg.dev/{self.project}/"
        if not self.registry.startswith(registry_prefix) or not re.fullmatch(
            r"[a-z][a-z0-9-]{0,62}", self.registry.removeprefix(registry_prefix)
        ):
            raise SetupError(f"--registry must be {registry_prefix}REPOSITORY")
        match = re.fullmatch(r"1\.(\d+)\.\d+-gke\.\d+", self.version)
        if not match or int(match[1]) < 36:
            raise SetupError("--version must pin a full GKE release at least 1.36, such as 1.37.1-gke.2097000")
        if not re.fullmatch(
            rf"[a-z][a-z0-9-]+@{re.escape(self.project)}\.iam\.gserviceaccount\.com", self.service_account
        ):
            raise SetupError("--service-account must name an existing dedicated node account in --project")
        if not re.fullmatch(r"(?:n2|c3)-standard-\d+", self.machine_type):
            raise SetupError(
                "--machine-type must be an n2-standard or c3-standard machine supporting nested virtualization"
            )
        if self.nodes < 1 or self.disk_size < 100 or self.rollout_timeout < 1:
            raise SetupError("--nodes and --rollout-timeout must be positive; --disk-size must be at least 100 GB")
        if bool(self.kubeconfig) != bool(self.context):
            raise SetupError("supply --kubeconfig and --context together, or omit both for an isolated kubeconfig")
        if self.runtime not in {"gvisor", "microvm"} or self.worker_replicas < 1:
            raise SetupError("--runtime must be gvisor or microvm, and --worker-replicas must be positive")
        if self.atenet_dataplane not in {"envoy", "agentgateway"}:
            raise SetupError("--atenet-dataplane must be envoy or agentgateway")
        for name in (self.worker_namespace, self.pool_name):
            if not re.fullmatch(r"[a-z][a-z0-9-]{0,61}[a-z0-9]", name):
                raise SetupError("worker namespace and name must be lowercase Kubernetes names")
        if self.context and (self.context.startswith("-") or any(c.isspace() for c in self.context)):
            raise SetupError("--context must be a single kubeconfig context name")


def plan(config: Config) -> dict[str, Any]:
    """Produce an offline plan; this function runs no commands and creates no files."""
    config.validate()
    return {
        "mode": "plan",
        "target": {key: str(value) if isinstance(value, Path) else value for key, value in asdict(config).items()},
        "source": {"url": SOURCE_URL, "revision": SOURCE_PIN, "build_version": BUILD_VERSION},
        "context": config.kube_context,
        "kubeconfig": str(config.kube_path),
        "ownership_label": f"{OWNER_LABEL}={config.cluster}",
        "agentgateway_image": AGENTGATEWAY_IMAGE if config.atenet_dataplane == "agentgateway" else None,
        "worker": {
            "namespace": config.worker_namespace,
            "name": config.pool_name,
            "runtime": config.runtime,
            "replicas": config.worker_replicas,
            "limits": {"cpu": "2", "memory": "4Gi"},
        },
        "required_certificate_apis": BETA_APIS.split(","),
        "actions": [
            "Check pinned source, Go 1.27+, gcloud, kubectl and asset tools; Envoy also needs Docker/buildx",
            "For agentgateway, export the pinned source privately and apply the compatible immutable image overlay",
            "Inspect cluster, bucket, repository and dedicated node account; refuse foreign or incompatible resources",
            "Create absent dedicated resources; adopt compatible unlabeled resources only with --adopt-existing",
            "Disable node auto-upgrade and preserve all node labels while setting the exact Substrate build version",
            "Verify the selected context, exact certificate API discovery, Workload Identity and nested virtualization",
            "Reconcile storage roles on this bucket and image-reader roles on this repository only",
            "Run pinned ate-setup deploy ate-system with explicit context and bounded rollout waits",
            "For microvm: run pinned install-microvm-deps.sh and verify its SandboxConfig",
            "Publish pinned worker images; create/check the dedicated WorkerPool and wait for ready workers",
        ],
        "next": "Use docs/substrate-setup.md to build the agy bridge image and configure authenticated access",
    }


class Runner:
    """Invoke argument vectors without shell interpolation or printing child output."""

    def __init__(self, config: Config):
        self.config = config
        self.env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith("ATE_")
            and key not in {"VERSION", "KATA_VER", "CH_VER", "ARCH", "OUT", "GOOS", "GOARCH", "EXPECTED_JWT_ISSUER"}
        }
        self.env.update(
            PROJECT_ID=config.project,
            CLUSTER_NAME=config.cluster,
            CLUSTER_LOCATION=config.zone,
            BUCKET_NAME=config.bucket,
            KO_DOCKER_REPO=config.registry,
            KO_DEFAULTPLATFORMS="linux/amd64",
            KUBECONFIG=str(config.kube_path),
            KUBECTL_CONTEXT=config.kube_context,
            DOCKER_CONFIG=str(config.state_dir / "docker"),
            NO_DEV_ENV="1",
            GOTOOLCHAIN="local",
            GOMAXPROCS="3",
            GOFLAGS="-p=3",
            CLOUDSDK_CORE_DISABLE_PROMPTS="1",
        )

    def run(self, args: list[str], *, optional: bool = False, install: bool = False) -> str | None:
        try:
            if install:
                log = self.config.state_dir / "install.log"
                with open(log, "a+", opener=lambda path, flags: os.open(path, flags, 0o600)) as handle:
                    os.fchmod(handle.fileno(), 0o600)
                    handle.seek(0, os.SEEK_END)
                    start = handle.tell()
                    result = subprocess.run(
                        args,
                        cwd=self.config.source,
                        env=self.env,
                        text=True,
                        stdout=handle,
                        stderr=subprocess.STDOUT,
                        timeout=3600,
                        check=False,
                    )
                    handle.seek(start)
                    output = handle.read()
                diagnostic = ""
            else:
                result = subprocess.run(
                    args,
                    cwd=self.config.source,
                    env=self.env,
                    text=True,
                    capture_output=True,
                    timeout=1200,
                    check=False,
                )
                output, diagnostic = result.stdout, result.stderr
        except (OSError, subprocess.TimeoutExpired) as error:
            raise SetupError(
                f"Could not finish {args[0]} {args[1]}: {type(error).__name__}; check the tool and retry"
            ) from None
        if result.returncode:
            # Permission and network failures must never be mistaken for missing resources.
            if optional and re.search(r"\b(?:NOT_FOUND|NotFound|404)\b", diagnostic):
                return None
            recovery = (
                f" Inspect {self.config.state_dir / 'install.log'} privately."
                if install
                else " Check access and tool support."
            )
            raise SetupError(
                f"{args[0]} {' '.join(args[1:4])} failed (exit {result.returncode}).{recovery} Rerun to resume."
            )
        return output.strip()

    def json(self, args: list[str], *, optional: bool = False) -> dict[str, Any] | None:
        output = self.run(args, optional=optional)
        if output is None:
            return None
        try:
            value = json.loads(output)
        except json.JSONDecodeError:
            raise SetupError(f"{args[0]} returned invalid JSON; check the installed tool version") from None
        if not isinstance(value, dict):
            raise SetupError(f"{args[0]} returned an unexpected response; expected an object")
        return value


def gcloud(config: Config, *args: str) -> list[str]:
    return ["gcloud", *args, f"--project={config.project}", "--quiet"]


def kubectl(config: Config, *args: str) -> list[str]:
    return [
        "kubectl",
        f"--kubeconfig={config.kube_path}",
        f"--context={config.kube_context}",
        "--request-timeout=30s",
        *args,
    ]


def owned(config: Config, resource: dict[str, Any], field: str, kind: str) -> None:
    owner = resource.get(field, {}).get(OWNER_LABEL)
    if owner == config.cluster:
        return
    if owner is not None:
        raise SetupError(f"{kind} belongs to another {OWNER_LABEL}; choose dedicated resource names")
    if not config.adopt_existing:
        raise SetupError(
            f"{kind} has no {OWNER_LABEL} label; review it and pass --adopt-existing "
            "to adopt a matching dedicated resource"
        )


def bucket_creation_identity(config: Config, bucket: dict[str, Any]) -> dict[str, str] | None:
    """Identify one immutable bucket creation from raw Cloud Storage metadata."""
    project = str(bucket.get("projectNumber", ""))
    created = bucket.get("timeCreated")
    if (
        bucket.get("name") != config.bucket
        or not project.isdigit()
        or not isinstance(created, str)
        or not created
        or bucket.get("location", "").lower() != config.region
    ):
        return None
    return {"name": config.bucket, "projectNumber": project, "location": config.region, "timeCreated": created}


def record_bucket_creation(config: Config, phase: str, receipt: dict[str, str] | None = None) -> None:
    """Atomically save a private intent, then the successful creation's receipt."""
    record = {
        "target": {"project": config.project, "cluster": config.cluster, "bucket": config.bucket},
        "phase": phase,
        "receipt": receipt,
    }
    descriptor, name = tempfile.mkstemp(dir=config.state_dir, prefix=".bucket-creation-")
    temporary = Path(name)
    try:
        with os.fdopen(descriptor, "w") as handle:
            os.fchmod(handle.fileno(), 0o600)
            json.dump(record, handle)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, config.state_dir / "bucket-creation.json")
    finally:
        temporary.unlink(missing_ok=True)


def recorded_bucket_creation(config: Config, bucket: dict[str, Any]) -> bool:
    """Allow label recovery only for the exact successfully recorded creation."""
    path = config.state_dir / "bucket-creation.json"
    if not path.exists():
        return False
    try:
        record = json.loads(path.read_text())
    except (OSError, ValueError):
        raise SetupError("bucket creation record is unreadable; inspect it or choose a new --state-dir") from None
    identity = bucket_creation_identity(config, bucket)
    return (
        isinstance(record, dict)
        and record.get("target") == {"project": config.project, "cluster": config.cluster, "bucket": config.bucket}
        and record.get("phase") == "created"
        and identity is not None
        and record.get("receipt") == identity
    )


def check_cluster(config: Config, cluster: dict[str, Any], build_version: str) -> None:
    owned(config, cluster, "resourceLabels", "cluster")
    if cluster.get("autopilot", {}).get("enabled"):
        raise SetupError("Substrate requires GKE Standard; the selected cluster is Autopilot")
    expected = {
        "name": config.cluster,
        "location": config.zone,
        "network": config.network,
        "subnetwork": config.subnet,
    }
    for key, value in expected.items():
        if str(cluster.get(key, "")).rsplit("/", 1)[-1] != value:
            raise SetupError(f"cluster {key} differs from the plan; choose another dedicated cluster")
    if cluster.get("status") != "RUNNING":
        raise SetupError("cluster is not RUNNING; let its current operation finish and rerun")
    if cluster.get("currentMasterVersion", "").split(".")[:2] != config.version.split(".")[:2]:
        raise SetupError(
            "cluster Kubernetes minor version differs from --version; this helper does not upgrade clusters"
        )
    if cluster.get("releaseChannel", {}).get("channel", "UNSPECIFIED") not in {"UNSPECIFIED", "NONE"}:
        raise SetupError("cluster must use release channel None to disable worker auto-upgrade")
    if cluster.get("workloadIdentityConfig", {}).get("workloadPool") != f"{config.project}.svc.id.goog":
        raise SetupError(
            "cluster lacks the requested Workload Identity pool; use a correctly configured dedicated cluster"
        )
    if bool(cluster.get("privateClusterConfig", {}).get("enablePrivateNodes")) != config.private_nodes:
        raise SetupError("cluster private-node setting differs; use --private-nodes for a private-node cluster")
    pools = cluster.get("nodePools", [])
    if len(pools) != 1 or pools[0].get("name") != "default-pool":
        raise SetupError("this helper requires one dedicated default-pool; it does not adopt multi-pool clusters")
    if pools[0].get("version", "").split(".")[:2] != config.version.split(".")[:2]:
        raise SetupError("worker Kubernetes minor version must match --version; this helper does not upgrade nodes")
    node = pools[0].get("config", {})
    if pools[0].get("initialNodeCount") != config.nodes or node.get("diskSizeGb", 0) < config.disk_size:
        raise SetupError("node count or disk size differs from the plan; set --nodes and --disk-size to match")
    for key, desired in (
        ("machineType", config.machine_type),
        ("serviceAccount", config.service_account),
        ("imageType", "UBUNTU_CONTAINERD"),
    ):
        if node.get(key) != desired:
            raise SetupError(f"node pool {key} differs from the plan; this helper does not replace worker nodes")
    if not node.get("advancedMachineFeatures", {}).get("enableNestedVirtualization"):
        raise SetupError(
            "node pool lacks nested virtualization; use a pool created with --enable-nested-virtualization"
        )
    if node.get("workloadMetadataConfig", {}).get("mode") != "GKE_METADATA":
        raise SetupError("node pool must use GKE_METADATA for Workload Identity")
    if node.get("spot") or node.get("preemptible") or pools[0].get("autoscaling", {}).get("enabled"):
        raise SetupError("worker nodes must be non-spot, non-preemptible and not autoscaled by this dedicated profile")
    if node.get("labels", {}).get(VERSION_LABEL, build_version) != build_version:
        raise SetupError("node pool belongs to a different Substrate build; use the upstream rolling upgrade procedure")


def inspect(
    config: Config, run: Runner, build_version: str
) -> tuple[str, dict[str, Any] | None, dict[str, Any] | None, dict[str, Any] | None]:
    project = run.json(gcloud(config, "projects", "describe", config.project, "--format=json(projectNumber)")) or {}
    number = str(project.get("projectNumber", ""))
    if not number.isdigit():
        raise SetupError("gcloud did not return the target project number")
    account = (
        run.json(gcloud(config, "iam", "service-accounts", "describe", config.service_account, "--format=json")) or {}
    )
    if account.get("disabled") or account.get("email") != config.service_account:
        raise SetupError("the dedicated node service account is missing, disabled or does not match")
    cluster = run.json(
        gcloud(config, "container", "clusters", "describe", config.cluster, f"--zone={config.zone}", "--format=json"),
        optional=True,
    )
    bucket = run.json(
        gcloud(config, "storage", "buckets", "describe", f"gs://{config.bucket}", "--raw", "--format=json"),
        optional=True,
    )
    repository = run.json(
        gcloud(
            config,
            "artifacts",
            "repositories",
            "describe",
            config.repository,
            f"--location={config.region}",
            "--format=json",
        ),
        optional=True,
    )
    if cluster:
        check_cluster(config, cluster, build_version)
    if bucket:
        if OWNER_LABEL in bucket.get("labels", {}) or not recorded_bucket_creation(config, bucket):
            owned(config, bucket, "labels", "bucket")
        if not str(bucket.get("projectNumber", "")).isdigit():
            raise SetupError(
                "raw bucket metadata has no projectNumber; cannot verify ownership with this gcloud version"
            )
        if str(bucket["projectNumber"]) != number or bucket.get("location", "").lower() != config.region:
            raise SetupError("bucket belongs to a different project or region; choose a dedicated bucket")
        if not bucket.get("iamConfiguration", {}).get("uniformBucketLevelAccess", {}).get("enabled"):
            raise SetupError("bucket must already have uniform bucket-level access enabled")
    if repository:
        owned(config, repository, "labels", "repository")
        expected_name = f"projects/{config.project}/locations/{config.region}/repositories/{config.repository}"
        if (
            repository.get("name") != expected_name
            or repository.get("format") != "DOCKER"
            or repository.get("mode", "STANDARD_REPOSITORY") != "STANDARD_REPOSITORY"
        ):
            raise SetupError("repository identity or format differs; choose a dedicated standard Docker repository")
    return number, cluster, bucket, repository


def prerequisites(config: Config, run: Runner) -> str:
    tools = ("git", "go", "gcloud", "kubectl", "curl", "tar", "zstd", "bash")
    for tool in (*tools, *(("docker",) if config.atenet_dataplane == "envoy" else ())):
        if not shutil.which(tool):
            raise SetupError(f"missing {tool}; install it from its official distribution and rerun")
    if run.run(["git", "rev-parse", "HEAD"]) != SOURCE_PIN or run.run(["git", "status", "--porcelain"]):
        raise SetupError(f"--source must be a clean checkout of {SOURCE_URL} at {SOURCE_PIN}")
    version = run.run(["go", "version"]) or ""
    match = re.search(r"go(\d+)\.(\d+)\.", version)
    if not match or (int(match[1]), int(match[2])) < (1, 27):
        raise SetupError(
            "the pinned upstream checkout requires Go 1.27+ on PATH; automatic toolchain downloads are disabled"
        )
    if config.atenet_dataplane == "envoy":
        try:
            run.run(["docker", "buildx", "version"])
            run.run(["docker", "info", "--format={{.ServerVersion}}"])
        except SetupError:
            raise SetupError(
                "Docker needs a running daemon and the buildx plugin; "
                "install buildx from Docker's official distribution"
            ) from None
    run.env["VERSION"] = BUILD_VERSION
    return BUILD_VERSION


def source_digest(source: Path) -> str:
    """Hash the private export, including names, modes and symlink targets."""
    digest = hashlib.sha256()
    for path in sorted(source.rglob("*")):
        relative = path.relative_to(source).as_posix()
        if relative == ".branchyard-source.json":
            continue
        if path.is_symlink():
            content = ["symlink", os.readlink(path)]
        elif path.is_file():
            with path.open("rb") as handle:
                content = ["file", hashlib.file_digest(handle, "sha256").hexdigest()]
        elif path.is_dir():
            content = ["directory"]
        else:
            raise SetupError("private source export contains an unsupported file type; inspect it before retrying")
        digest.update(json.dumps([relative, path.lstat().st_mode & 0o777, content]).encode() + b"\n")
    return digest.hexdigest()


def installer_source(config: Config, run: Runner) -> Path:
    """Prepare the compatible image overlay without modifying the supplied checkout."""
    if config.atenet_dataplane == "envoy":
        return config.source
    target = config.state_dir / "agentgateway-source"
    owner = {"source_revision": SOURCE_PIN, "agentgateway_image": AGENTGATEWAY_IMAGE}
    if target.exists() or target.is_symlink():
        try:
            marker = target / ".branchyard-source.json"
            if target.is_symlink() or marker.is_symlink():
                raise ValueError("symlink source")
            recorded = json.loads(marker.read_text())
            if recorded != {**owner, "sha256": source_digest(target)}:
                raise ValueError("modified source")
        except (OSError, ValueError, TypeError):
            raise SetupError("private agentgateway source is unowned or modified; inspect it before retrying") from None
        return target
    try:
        with tempfile.TemporaryDirectory(prefix="agentgateway-", dir=config.state_dir) as temporary:
            stage = Path(temporary)
            archive = stage / "source.tar"
            run.run(["git", "archive", "--format=tar", f"--output={archive}", SOURCE_PIN])
            exported = stage / "source"
            exported.mkdir(mode=0o700)
            with tarfile.open(archive) as contents:
                contents.extractall(exported, filter="data")
            component = exported / "manifests/ate-install/components/agentgateway"
            base = component.with_name("agentgateway-pinned-base")
            if not (exported / "go.mod").is_file() or not (component / "kustomization.yaml").is_file() or base.exists():
                raise SetupError("pinned source has an unexpected agentgateway layout; no installer was run")
            component.rename(base)
            component.mkdir(mode=0o700)
            name, reference = AGENTGATEWAY_IMAGE.split(":v", 1)
            tag, digest = ("v" + reference).split("@", 1)
            overlay = {
                "apiVersion": "kustomize.config.k8s.io/v1alpha1",
                "kind": "Component",
                "components": ["../agentgateway-pinned-base"],
                "images": [
                    {"name": "ghcr.io/agentgateway/agentgateway", "newName": name, "newTag": tag, "digest": digest}
                ],
            }
            (component / "kustomization.yaml").write_text(json.dumps(overlay, indent=2) + "\n")
            (exported / ".branchyard-source.json").write_text(json.dumps({**owner, "sha256": source_digest(exported)}))
            exported.rename(target)
    except (OSError, tarfile.TarError) as error:
        raise SetupError(
            f"could not prepare private agentgateway source: {type(error).__name__}; inspect state and retry"
        ) from None
    return target


def ensure_resources(
    config: Config,
    run: Runner,
    build_version: str,
    cluster: dict[str, Any] | None,
    bucket: dict[str, Any] | None,
    repository: dict[str, Any] | None,
) -> dict[str, Any]:
    label = f"{OWNER_LABEL}={config.cluster}"
    if cluster is None:
        args = gcloud(
            config,
            "container",
            "clusters",
            "create",
            config.cluster,
            f"--zone={config.zone}",
            f"--cluster-version={config.version}",
            "--release-channel=None",
            f"--machine-type={config.machine_type}",
            f"--num-nodes={config.nodes}",
            f"--disk-size={config.disk_size}",
            "--image-type=UBUNTU_CONTAINERD",
            "--enable-nested-virtualization",
            "--no-enable-autoupgrade",
            f"--workload-pool={config.project}.svc.id.goog",
            "--workload-metadata=GKE_METADATA",
            f"--service-account={config.service_account}",
            f"--network={config.network}",
            f"--subnetwork={config.subnet}",
            f"--enable-kubernetes-unstable-apis={BETA_APIS}",
            f"--labels={label}",
            f"--node-labels={VERSION_LABEL}={build_version}",
        )
        if config.private_nodes:
            args += ["--enable-private-nodes", f"--master-ipv4-cidr={config.master_cidr}", "--enable-ip-alias"]
        run.run(args)
        cluster = (
            run.json(
                gcloud(
                    config,
                    "container",
                    "clusters",
                    "describe",
                    config.cluster,
                    f"--zone={config.zone}",
                    "--format=json",
                )
            )
            or {}
        )
        check_cluster(config, cluster, build_version)
    elif cluster.get("resourceLabels", {}).get(OWNER_LABEL) != config.cluster:
        run.run(
            gcloud(
                config,
                "container",
                "clusters",
                "update",
                config.cluster,
                f"--zone={config.zone}",
                f"--update-labels={label}",
            )
        )
    pool = cluster["nodePools"][0]
    pool_args = (
        "container",
        "node-pools",
        "update",
        "default-pool",
        f"--cluster={config.cluster}",
        f"--zone={config.zone}",
    )
    if pool.get("management", {}).get("autoUpgrade", True):
        run.run(gcloud(config, *pool_args, "--no-enable-autoupgrade"))
    labels = dict(pool.get("config", {}).get("labels", {}))
    if labels.get(VERSION_LABEL) != build_version:
        labels[VERSION_LABEL] = build_version
        run.run(
            gcloud(
                config,
                *pool_args,
                "--node-labels=" + ",".join(f"{key}={value}" for key, value in sorted(labels.items())),
            )
        )
    if bucket is None:
        record_bucket_creation(config, "creating")
        run.run(
            gcloud(
                config,
                "storage",
                "buckets",
                "create",
                f"gs://{config.bucket}",
                f"--location={config.region}",
                "--uniform-bucket-level-access",
            )
        )
        bucket = (
            run.json(
                gcloud(config, "storage", "buckets", "describe", f"gs://{config.bucket}", "--raw", "--format=json")
            )
            or {}
        )
        receipt = bucket_creation_identity(config, bucket)
        if receipt is None:
            raise SetupError("created bucket has incomplete raw metadata; inspect ownership before retrying")
        record_bucket_creation(config, "created", receipt)
    if bucket.get("labels", {}).get(OWNER_LABEL) != config.cluster:
        run.run(gcloud(config, "storage", "buckets", "update", f"gs://{config.bucket}", f"--update-labels={label}"))
    if repository is None:
        run.run(
            gcloud(
                config,
                "artifacts",
                "repositories",
                "create",
                config.repository,
                f"--location={config.region}",
                "--repository-format=docker",
                f"--labels={label}",
            )
        )
    elif repository.get("labels", {}).get(OWNER_LABEL) != config.cluster:
        run.run(
            gcloud(
                config,
                "artifacts",
                "repositories",
                "update",
                config.repository,
                f"--location={config.region}",
                f"--update-labels={label}",
            )
        )
    return cluster


def verify_context(config: Config, run: Runner, cluster: dict[str, Any], build_version: str) -> None:
    if config.kubeconfig is None:
        run.run(gcloud(config, "container", "clusters", "get-credentials", config.cluster, f"--zone={config.zone}"))
    server = (
        run.run(kubectl(config, "config", "view", "--minify", "-o", "jsonpath={.clusters[0].cluster.server}")) or ""
    )
    private = cluster.get("privateClusterConfig", {})
    endpoints = {cluster.get("endpoint"), private.get("privateEndpoint"), private.get("publicEndpoint")}
    parsed_server = urlsplit(server)
    if parsed_server.scheme != "https" or not parsed_server.hostname or parsed_server.hostname not in endpoints:
        raise SetupError(
            "selected kubeconfig context does not point at the described cluster; no Kubernetes changes were made"
        )
    discovery = run.json(kubectl(config, "get", "--raw", "/apis/certificates.k8s.io/v1beta1"), optional=True)
    served = {item.get("name") for item in (discovery or {}).get("resources", [])}
    if not set(CERTIFICATE_APIS) <= served:
        raise SetupError(
            "cluster does not serve both required certificates.k8s.io/v1beta1 APIs. Enable them with "
            f"gcloud container clusters update {config.cluster} --project={config.project} --zone={config.zone} "
            f"--enable-kubernetes-unstable-apis={BETA_APIS}, then rerun. GKE 1.36 also needs replacement nodes; "
            "this helper never replaces existing nodes. GKE 1.37 alone does not prove these APIs are enabled."
        )
    nodes = run.json(kubectl(config, "get", "nodes", "-o", "json")) or {}
    if not nodes.get("items"):
        raise SetupError("cluster has no nodes; wait for the node pool and rerun")
    for node in nodes["items"]:
        if node.get("metadata", {}).get("labels", {}).get(VERSION_LABEL, build_version) != build_version:
            raise SetupError("a node runs another Substrate build; use the upstream rolling upgrade procedure")


def ensure_iam(config: Config, run: Runner, number: str) -> None:
    principal = f"principal://iam.googleapis.com/projects/{number}/locations/global/workloadIdentityPools/{config.project}.svc.id.goog/subject/ns/ate-system/sa"
    bucket_args = ("storage", "buckets")
    bucket_policy = (
        run.json(gcloud(config, *bucket_args, "get-iam-policy", f"gs://{config.bucket}", "--format=json")) or {}
    )
    repository_args = ("artifacts", "repositories")
    repo_policy = (
        run.json(
            gcloud(
                config,
                *repository_args,
                "get-iam-policy",
                config.repository,
                f"--location={config.region}",
                "--format=json",
            )
        )
        or {}
    )
    bindings = [
        (
            bucket_policy,
            [*bucket_args, "add-iam-policy-binding", f"gs://{config.bucket}"],
            role,
            f"{principal}/{account}",
        )
        for account in ("atelet", "ate-api-server")
        for role in ("roles/storage.objectAdmin", "roles/storage.bucketViewer")
    ]
    bindings += [
        (
            repo_policy,
            [*repository_args, "add-iam-policy-binding", config.repository, f"--location={config.region}"],
            "roles/artifactregistry.reader",
            member,
        )
        for member in (f"{principal}/atelet", f"serviceAccount:{config.service_account}")
    ]
    for policy, command, role, member in bindings:
        if not any(
            b.get("role") == role and member in b.get("members", []) and not b.get("condition")
            for b in policy.get("bindings", [])
        ):
            run.run(gcloud(config, *command, f"--role={role}", f"--member={member}", "--condition=None"))


def ate_setup(config: Config, *command: str) -> list[str]:
    return [
        "go",
        "run",
        "./cmd/ate-setup",
        "--no-dev-env",
        f"--kubeconfig={config.kube_path}",
        f"--context={config.kube_context}",
        f"--rollout-timeout={config.rollout_timeout}s",
        f"--atenet-dataplane={config.atenet_dataplane}",
        *command,
    ]


def worker_images(config: Config, output: str) -> dict[str, str]:
    """Parse upstream's named image results, refusing duplicates or unpinned foreign refs."""
    images: dict[str, str] = {}
    for line in output.splitlines():
        name, separator, ref = line.partition(": ")
        if separator and name in {"ateom-gvisor", "ateom-microvm"}:
            if name in images or not re.fullmatch(rf"{re.escape(config.registry)}/{name}@sha256:[a-f0-9]{{64}}", ref):
                raise SetupError(
                    "upstream worker image output is duplicated, unpinned or outside the selected registry"
                )
            images[name] = ref
    if set(images) != {"ateom-gvisor", "ateom-microvm"}:
        raise SetupError("upstream did not publish both named worker image references; inspect install.log")
    return images


def ensure_workers(config: Config, run: Runner, build_version: str) -> dict[str, Any]:
    namespace = run.json(kubectl(config, "get", "namespace", config.worker_namespace, "-o", "json"), optional=True)
    pool = run.json(
        kubectl(config, "get", "workerpool", config.pool_name, "-n", config.worker_namespace, "-o", "json"),
        optional=True,
    )
    for resource, kind in ((namespace, "worker namespace"), (pool, "WorkerPool")):
        if resource:
            owned(config, resource.get("metadata", {}), "labels", kind)
    output = run.run(ate_setup(config, "publish", "worker-images"), install=True) or ""
    ref = worker_images(config, output)[f"ateom-{config.runtime}"]
    selector = {"workload": "branchyard", "branchyard-runtime": config.runtime}
    desired = {
        "apiVersion": "ate.dev/v1alpha1",
        "kind": "WorkerPool",
        "metadata": {
            "name": config.pool_name,
            "namespace": config.worker_namespace,
            "labels": {OWNER_LABEL: config.cluster, **selector},
        },
        "spec": {
            "replicas": config.worker_replicas,
            "sandboxClass": config.runtime,
            "workerImage": ref,
            "template": {
                "labels": selector,
                "nodeSelector": {VERSION_LABEL: build_version},
                "resources": {"limits": {"cpu": "2", "memory": "4Gi"}, "requests": {"cpu": "500m", "memory": "4Gi"}},
            },
        },
    }
    if pool and pool.get("spec") != desired["spec"]:
        raise SetupError(
            "existing WorkerPool spec differs; use another --worker-name or the upstream upgrade procedure"
        )
    if namespace is None:
        manifest = config.state_dir / "worker-namespace.json"
        manifest.write_text(
            json.dumps(
                {
                    "apiVersion": "v1",
                    "kind": "Namespace",
                    "metadata": {"name": config.worker_namespace, "labels": {OWNER_LABEL: config.cluster}},
                }
            )
        )
        run.run(kubectl(config, "create", "-f", str(manifest)))
    elif namespace.get("metadata", {}).get("labels", {}).get(OWNER_LABEL) != config.cluster:
        run.run(kubectl(config, "label", "namespace", config.worker_namespace, f"{OWNER_LABEL}={config.cluster}"))
    if pool is None:
        manifest = config.state_dir / "worker-pool.json"
        manifest.write_text(json.dumps(desired, indent=2) + "\n")
        run.run(kubectl(config, "create", "-f", str(manifest)))
    elif pool.get("metadata", {}).get("labels", {}).get(OWNER_LABEL) != config.cluster:
        run.run(
            kubectl(
                config,
                "label",
                "workerpool",
                config.pool_name,
                "-n",
                config.worker_namespace,
                f"{OWNER_LABEL}={config.cluster}",
            )
        )
    run.run(
        kubectl(
            config,
            "wait",
            f"workerpool/{config.pool_name}",
            "-n",
            config.worker_namespace,
            f"--for=jsonpath={{.status.readyReplicas}}={config.worker_replicas}",
            f"--timeout={config.rollout_timeout}s",
        ),
        install=True,
    )
    return {
        "namespace": config.worker_namespace,
        "name": config.pool_name,
        "runtime": config.runtime,
        "image": ref,
        "selector": selector,
        "limits": {"cpu": "2", "memory": "4Gi"},
        "sandbox_config": "microvm" if config.runtime == "microvm" else "gvisor-default",
    }


def apply(config: Config, run: Runner | None = None) -> dict[str, Any]:
    """Re-observe resources on each run and install only into the validated target."""
    config.validate()
    run = run or Runner(config)
    run.config = config
    print("Checking pinned source and local tools...", file=sys.stderr, flush=True)
    build_version = prerequisites(config, run)
    config.state_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    run.config = replace(config, source=installer_source(config, run))
    print("Inspecting the dedicated Google Cloud resources...", file=sys.stderr, flush=True)
    number, cluster, bucket, repository = inspect(config, run, build_version)
    config.state_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    (config.state_dir / "docker").mkdir(exist_ok=True, mode=0o700)
    print("Reconciling resources, cluster prerequisites and scoped IAM...", file=sys.stderr, flush=True)
    cluster = ensure_resources(config, run, build_version, cluster, bucket, repository)
    verify_context(config, run, cluster, build_version)
    ensure_iam(config, run, number)
    run.run(gcloud(config, "auth", "configure-docker", config.registry.split("/", 1)[0]))
    print("Installing pinned Substrate; private progress is in install.log...", file=sys.stderr, flush=True)
    run.run(ate_setup(config, "deploy", "ate-system"), install=True)
    if config.runtime == "microvm":
        installer_config = run.config
        if config.atenet_dataplane == "agentgateway":
            # This upstream script needs git metadata; its generated assets
            # belong outside both the pristine checkout and the verified export.
            run.config = config
            run.env["OUT"] = str(config.state_dir / "microvm-assets")
        try:
            run.run(["bash", "hack/install-microvm-deps.sh", "--install"], install=True)
        finally:
            run.config = installer_config
            if config.atenet_dataplane == "agentgateway":
                run.env.pop("OUT", None)
        run.run(kubectl(config, "get", "sandboxconfig", "microvm", "-o", "name"))
    print("Publishing and checking the dedicated worker pool...", file=sys.stderr, flush=True)
    worker = ensure_workers(config, run, build_version)
    result = {
        "mode": "applied",
        "project": config.project,
        "cluster": config.cluster,
        "zone": config.zone,
        "bucket": config.bucket,
        "registry": config.registry,
        "source_revision": SOURCE_PIN,
        "worker": worker,
        "substrate_version": build_version,
        "atenet_dataplane": config.atenet_dataplane,
        "agentgateway_image": AGENTGATEWAY_IMAGE if config.atenet_dataplane == "agentgateway" else None,
        "context": config.kube_context,
        "kubeconfig": str(config.kube_path),
        "next": "Configure a bridge image and authenticated access using docs/substrate-setup.md",
    }
    (config.state_dir / "deployment.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("project", "zone", "cluster", "bucket", "registry", "version", "service-account"):
        parser.add_argument(f"--{name}", required=True)
    parser.add_argument("--source", required=True, type=Path, help=f"clean upstream checkout at {SOURCE_PIN}")
    parser.add_argument(
        "--state-dir", type=Path, help="defaults to a separate state directory per project/zone/cluster"
    )
    parser.add_argument("--machine-type", default="c3-standard-4")
    parser.add_argument("--nodes", type=int, default=2)
    parser.add_argument("--disk-size", type=int, default=100)
    parser.add_argument("--network", default="default")
    parser.add_argument("--subnet", default="default")
    parser.add_argument(
        "--private-nodes", action="store_true", help="requires existing outbound connectivity such as Cloud NAT"
    )
    parser.add_argument("--runtime", choices=("gvisor", "microvm"), default="gvisor")
    parser.add_argument("--atenet-dataplane", choices=("envoy", "agentgateway"), default="envoy")
    parser.add_argument("--worker-namespace", default="branchyard")
    parser.add_argument("--worker-name", help="defaults to branchyard-RUNTIME")
    parser.add_argument("--worker-replicas", type=int, default=1)
    parser.add_argument("--master-cidr", default="172.28.0.0/28")
    parser.add_argument("--kubeconfig", type=Path)
    parser.add_argument("--context")
    parser.add_argument(
        "--adopt-existing", action="store_true", help="adopt only compatible, unlabeled dedicated resources"
    )
    parser.add_argument("--rollout-timeout", type=int, default=300, help="seconds per upstream rollout wait")
    parser.add_argument(
        "--apply", action="store_true", help="create/adopt resources and run the pinned upstream installer"
    )
    args = vars(parser.parse_args(argv))
    should_apply = args.pop("apply")
    if args["state_dir"] is None:
        args["state_dir"] = (
            Path.home() / ".local/state/branchyard/substrate-gke" / args["project"] / args["zone"] / args["cluster"]
        )
    for name in ("source", "state_dir", "kubeconfig"):
        if args.get(name) is not None:
            args[name] = args[name].expanduser().resolve()
    config = Config(**args)
    try:
        print(json.dumps(apply(config) if should_apply else plan(config), indent=2))
    except SetupError as error:
        print(f"substrate-gke: {error}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("substrate-gke: interrupted; rerun the same command to inspect resources and resume", file=sys.stderr)
        return 130
    return 0


if __name__ == "__main__":
    sys.exit(main())
