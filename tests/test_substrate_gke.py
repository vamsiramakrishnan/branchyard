#!/usr/bin/env python3
"""The GKE helper uses fake CLI tools: offline plans, refusal, scoped IAM and resumable installation."""

import copy
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
import substrate_gke as gke

ROOT = Path(__file__).resolve().parents[1]

FAKE = r"""
import json, os, sys
from pathlib import Path
args = sys.argv[1:]
tool = Path(sys.argv[0]).name
state_path = Path(os.environ["FAKE_STATE"])
state = json.loads(state_path.read_text())
with Path(os.environ["FAKE_LOG"]).open("a") as log:
    log.write(json.dumps({"tool": tool, "args": args, "context": os.environ.get("KUBECTL_CONTEXT"),
                          "kubeconfig": os.environ.get("KUBECONFIG"), "version": os.environ.get("VERSION")}) + "\n")
def done(value=""):
    state_path.write_text(json.dumps(state))
    print(json.dumps(value) if isinstance(value, (dict, list)) else value)
    sys.exit(0)
def fail(text, code=1):
    state_path.write_text(json.dumps(state))
    print(text, file=sys.stderr)
    sys.exit(code)
def flag(prefix):
    return next((arg.split("=", 1)[1] for arg in args if arg.startswith(prefix + "=")), "")
if tool == "git":
    if args[0] == "rev-parse": done(state["pin"])
    if args[0] == "status": done(state.get("dirty", ""))
    done("1d7ca8c")
if tool == "go":
    if args == ["version"]: done(state.get("go_version", "go version go1.27.3 linux/amd64"))
    if "worker-images" in args:
        if state.get("bad_worker_output"): done(state["bad_worker_output"])
        registry = os.environ["KO_DOCKER_REPO"]
        done("Worker images for 1d7ca8c:\n" + "\n".join(
            f"ateom-{runtime}: {registry}/ateom-{runtime}@sha256:" + "a" * 64
            for runtime in ("gvisor", "microvm")))
    if state.get("check_streaming"):
        print("streaming marker", flush=True)
        log = Path(os.environ["DOCKER_CONFIG"]).parent / "install.log"
        if not log.exists() or "streaming marker" not in log.read_text(): fail("installer output was not streamed")
    if state.get("fail_install"):
        state["fail_install"] = False
        fail("private-token-should-not-appear-in-output")
    done("installed")
if tool in ("docker", "bash"): done()
if tool == "kubectl":
    if "get" in args and ("namespace" in args or "workerpool" in args):
        kind = "namespace" if "namespace" in args else "workerpool"
        if state.get(kind) is None: fail("NotFound")
        done(state[kind])
    if "create" in args:
        manifest = json.loads(Path(args[args.index("-f") + 1]).read_text())
        kind = manifest["kind"].lower()
        if state.get(kind) is not None: fail("AlreadyExists")
        state[kind] = manifest
        done()
    if "label" in args:
        kind = "namespace" if "namespace" in args else "workerpool"
        state[kind]["metadata"].setdefault("labels", {})["branchyard-deployment"] = "test-substrate"
        done()
    if "wait" in args: done("ready")
    if "view" in args: done(state.get("server", "https://192.0.2.1"))
    if "--raw" in args: done({"resources": [{"name": x} for x in state["apis"]]})
    if "nodes" in args: done({"items": [{"metadata": {"labels": state.get("node_labels", {})}}]})
    done("sandboxconfig/microvm")
if tool == "gcloud":
    if args[:2] == ["projects", "describe"]: done({"projectNumber": "12345"})
    if args[:3] == ["iam", "service-accounts", "describe"]:
        done({"email": state["desired_cluster"]["nodePools"][0]["config"]["serviceAccount"]})
    if args[:3] == ["container", "node-pools", "update"]:
        pool = state["cluster"]["nodePools"][0]
        if "--no-enable-autoupgrade" in args: pool["management"]["autoUpgrade"] = False
        if flag("--node-labels"):
            pool["config"]["labels"] = dict(x.split("=", 1) for x in flag("--node-labels").split(","))
        done()
    kinds = {("container", "clusters"): "cluster", ("storage", "buckets"): "bucket",
             ("artifacts", "repositories"): "repository"}
    kind = kinds.get(tuple(args[:2]))
    if kind:
        verb = args[2]
        if verb == "get-credentials": done()
        if verb == "describe":
            if state.get("describe_failure"): fail(state["describe_failure"])
            if state[kind] is None: fail("NOT_FOUND")
            if kind == "bucket" and "--raw" not in args:
                done({"name": "test-snapshots", "location": state[kind]["location"],
                      "uniform_bucket_level_access": True, "labels": state[kind].get("labels", {})})
            done(state[kind])
        if verb == "create":
            if state[kind] is not None: fail("ALREADY_EXISTS")
            state[kind] = state["desired_" + kind]
            done()
        if verb == "update":
            field = "resourceLabels" if kind == "cluster" else "labels"
            state[kind].setdefault(field, {}).update(dict(x.split("=", 1) for x in flag("--update-labels").split(",")))
            done()
        if verb == "get-iam-policy": done(state.setdefault(kind + "_policy", {"bindings": []}))
        if verb == "add-iam-policy-binding":
            binding = {"role": flag("--role"), "members": [flag("--member")]}
            state.setdefault(kind + "_policy", {"bindings": []})["bindings"].append(binding)
            done()
    if args[:2] == ["auth", "configure-docker"]: done()
fail("unexpected fake command " + tool + " " + repr(args))
"""


class SubstrateGke(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = Path(self.tmp.name)
        self.bin = self.path / "bin"
        self.bin.mkdir()
        self.source = self.path / "source"
        self.source.mkdir()
        self.state_path = self.path / "fake-state.json"
        self.log = self.path / "calls.jsonl"
        fake = self.bin / "fake"
        fake.write_text(f"#!{sys.executable}\n" + FAKE)
        fake.chmod(0o755)
        for name in ("git", "go", "gcloud", "kubectl", "docker", "curl", "tar", "zstd", "bash"):
            (self.bin / name).symlink_to(fake)
        self.env = dict(os.environ, PATH=str(self.bin), FAKE_STATE=str(self.state_path), FAKE_LOG=str(self.log))
        self.args = [
            "--project=test-project",
            "--zone=us-central1-a",
            "--cluster=test-substrate",
            "--bucket=test-snapshots",
            "--registry=us-central1-docker.pkg.dev/test-project/substrate",
            "--version=1.37.1-gke.2097000",
            "--service-account=substrate-node@test-project.iam.gserviceaccount.com",
            f"--source={self.source}",
            f"--state-dir={self.path / 'state'}",
        ]
        labels = {gke.OWNER_LABEL: "test-substrate"}
        cluster = {
            "name": "test-substrate",
            "location": "us-central1-a",
            "network": "default",
            "subnetwork": "default",
            "resourceLabels": labels,
            "status": "RUNNING",
            "currentMasterVersion": "1.37.1-gke.2097000",
            "endpoint": "192.0.2.1",
            "workloadIdentityConfig": {"workloadPool": "test-project.svc.id.goog"},
            "nodePools": [
                {
                    "name": "default-pool",
                    "initialNodeCount": 2,
                    "version": "1.37.1-gke.2097000",
                    "management": {"autoUpgrade": False},
                    "config": {
                        "machineType": "c3-standard-4",
                        "diskSizeGb": 100,
                        "serviceAccount": "substrate-node@test-project.iam.gserviceaccount.com",
                        "imageType": "UBUNTU_CONTAINERD",
                        "advancedMachineFeatures": {"enableNestedVirtualization": True},
                        "workloadMetadataConfig": {"mode": "GKE_METADATA"},
                        "labels": {gke.VERSION_LABEL: "1d7ca8c"},
                    },
                }
            ],
        }
        bucket = {
            "projectNumber": "12345",
            "location": "US-CENTRAL1",
            "iamConfiguration": {"uniformBucketLevelAccess": {"enabled": True}},
            "labels": labels,
        }
        repository = {
            "name": "projects/test-project/locations/us-central1/repositories/substrate",
            "format": "DOCKER",
            "labels": labels,
        }
        self.state = {
            "pin": gke.SOURCE_PIN,
            "apis": list(gke.CERTIFICATE_APIS),
            "cluster": None,
            "bucket": None,
            "repository": None,
            "desired_cluster": cluster,
            "desired_bucket": bucket,
            "desired_repository": repository,
        }
        self.save()

    def save(self):
        self.state_path.write_text(json.dumps(self.state))

    def invoke(self, *args):
        return subprocess.run(
            [sys.executable, str(ROOT / "tools/substrate_gke.py"), *self.args, *args],
            env=self.env,
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def adopt(self):
        for kind in ("cluster", "bucket", "repository"):
            self.state[kind] = self.state["desired_" + kind]
        self.save()

    def mutations(self):
        return [
            call
            for call in self.calls()
            if call["tool"] == "gcloud"
            and any(verb in call["args"] for verb in ("create", "update", "add-iam-policy-binding"))
        ]

    def test_default_plan_is_offline_even_when_source_is_absent(self):
        self.source.rmdir()
        result = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        output = json.loads(result.stdout)
        self.assertEqual(output["source"]["revision"], gke.SOURCE_PIN)
        self.assertEqual(output["target"]["registry"], "us-central1-docker.pkg.dev/test-project/substrate")
        self.assertEqual(self.calls(), [])
        self.assertFalse((self.path / "state").exists())

    def test_create_and_repeat_preserve_context_and_scope_iam(self):
        first = self.invoke("--apply")
        self.assertEqual(first.returncode, 0, first.stderr)
        first_mutations = len(self.mutations())
        second = self.invoke("--apply")
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertEqual(len(self.mutations()), first_mutations)
        creates = [call for call in self.calls() if call["tool"] == "gcloud" and "create" in call["args"]]
        self.assertEqual(len(creates), 3)
        cluster_create = next(call["args"] for call in creates if "clusters" in call["args"])
        self.assertIn(f"--enable-kubernetes-unstable-apis={gke.BETA_APIS}", cluster_create)
        self.assertIn("--no-enable-autoupgrade", cluster_create)
        iam = [call["args"] for call in self.calls() if "add-iam-policy-binding" in call["args"]]
        self.assertEqual(len(iam), 6)
        self.assertFalse(any("projects" in call for call in iam))
        self.assertEqual(sum("--role=roles/storage.objectAdmin" in call for call in iam), 2)
        install = next(call for call in self.calls() if call["tool"] == "go" and "run" in call["args"])
        self.assertIn("--context=gke_test-project_us-central1-a_test-substrate", install["args"])
        self.assertEqual(install["version"], "1d7ca8c")
        self.assertTrue(all(call["context"] == install["context"] for call in self.calls()))

    def test_failure_resumes_without_duplicate_resources_or_secret_output(self):
        self.state["fail_install"] = True
        self.state["check_streaming"] = True
        self.save()
        failed = self.invoke("--apply")
        self.assertEqual(failed.returncode, 1)
        self.assertNotIn("private-token", failed.stdout + failed.stderr)
        self.assertIn("install.log", failed.stderr)
        resumed = self.invoke("--apply")
        self.assertEqual(resumed.returncode, 0, resumed.stderr)
        self.assertEqual(sum(call["tool"] == "gcloud" and "create" in call["args"] for call in self.calls()), 3)
        self.assertEqual((self.path / "state/install.log").stat().st_mode & 0o777, 0o600)

    def test_foreign_resource_refused_before_any_mutation_even_with_adoption(self):
        self.adopt()
        self.state["repository"]["labels"] = {gke.OWNER_LABEL: "another-cluster"}
        self.save()
        result = self.invoke("--apply", "--adopt-existing")
        self.assertEqual(result.returncode, 1)
        self.assertIn("another", result.stderr)
        self.assertEqual(self.mutations(), [])

    def test_unlabeled_adoption_preserves_labels_and_separates_node_updates(self):
        self.adopt()
        for kind in ("cluster", "bucket", "repository"):
            self.state[kind]["resourceLabels" if kind == "cluster" else "labels"] = {"purpose": "qualification"}
        pool = self.state["cluster"]["nodePools"][0]
        pool["management"]["autoUpgrade"] = True
        pool["config"]["labels"] = {"existing": "value"}
        self.save()
        refused = self.invoke("--apply")
        self.assertEqual(refused.returncode, 1)
        self.assertEqual(self.mutations(), [])
        accepted = self.invoke("--apply", "--adopt-existing")
        self.assertEqual(accepted.returncode, 0, accepted.stderr)
        updates = [call["args"] for call in self.calls() if "node-pools" in call["args"]]
        self.assertEqual(len(updates), 2)
        self.assertTrue(
            any("--node-labels=ate.dev/substrate-version=1d7ca8c,existing=value" in args for args in updates)
        )
        self.assertFalse(
            any(
                "--no-enable-autoupgrade" in args and any(x.startswith("--node-labels=") for x in args)
                for args in updates
            )
        )

    def test_missing_beta_api_on_137_prevents_installer(self):
        self.adopt()
        self.state["apis"] = ["clustertrustbundles"]
        self.save()
        result = self.invoke("--apply")
        self.assertEqual(result.returncode, 1)
        self.assertIn("1.37 alone", result.stderr)
        self.assertFalse(any(call["tool"] == "go" and "run" in call["args"] for call in self.calls()))
        self.assertEqual(self.mutations(), [])

    def test_explicit_context_is_preserved_and_not_replaced_with_get_credentials(self):
        self.adopt()
        kubeconfig = self.path / "external-kubeconfig"
        result = self.invoke("--apply", f"--kubeconfig={kubeconfig}", "--context=chosen-alias")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any("get-credentials" in call["args"] for call in self.calls()))
        self.assertTrue(all(call["context"] == "chosen-alias" for call in self.calls()))
        self.assertFalse(kubeconfig.exists())

    def test_wrong_context_missing_prerequisite_and_permission_error_fail_closed(self):
        self.adopt()
        for field, value, error in (
            ("server", "https://192.0.2.2", "context"),
            ("server", "", "context"),
            ("go_version", "go version go1.26.1 linux/amd64", "Go 1.27"),
            ("describe_failure", "PERMISSION_DENIED private-token", "failed"),
            ("dirty", " M go.mod", "clean checkout"),
        ):
            with self.subTest(field=field):
                self.state[field] = value
                self.save()
                result = self.invoke("--apply")
                self.assertEqual(result.returncode, 1)
                self.assertIn(error, result.stderr)
                self.assertNotIn("private-token", result.stderr)
                self.assertEqual(self.mutations(), [])
                del self.state[field]

    def test_incompatible_cluster_or_storage_is_refused_before_mutation(self):
        self.adopt()
        original = copy.deepcopy(self.state)
        variations = [
            ("autopilot", {"enabled": True}),
            ("workloadIdentityConfig", {"workloadPool": "other.svc.id.goog"}),
            ("currentMasterVersion", "1.35.1-gke.1"),
        ]
        for field, value in variations:
            with self.subTest(field=field):
                self.state = copy.deepcopy(original)
                self.state["cluster"][field] = value
                self.save()
                self.assertEqual(self.invoke("--apply", "--adopt-existing").returncode, 1)
                self.assertEqual(self.mutations(), [])
        for field, value in (
            ("advancedMachineFeatures", {}),
            ("spot", True),
            ("labels", {gke.VERSION_LABEL: "different-version"}),
        ):
            with self.subTest(field=field):
                self.state = copy.deepcopy(original)
                self.state["cluster"]["nodePools"][0]["config"][field] = value
                self.save()
                self.assertEqual(self.invoke("--apply", "--adopt-existing").returncode, 1)
                self.assertEqual(self.mutations(), [])
        self.state = copy.deepcopy(original)
        self.state["bucket"]["projectNumber"] = "99999"
        self.save()
        self.assertEqual(self.invoke("--apply", "--adopt-existing").returncode, 1)
        self.assertEqual(self.mutations(), [])

    def test_private_endpoint_context_matches_the_described_cluster(self):
        self.adopt()
        self.state["server"] = "https://172.28.0.2"
        self.state["cluster"]["privateClusterConfig"] = {"enablePrivateNodes": True, "privateEndpoint": "172.28.0.2"}
        self.save()
        result = self.invoke("--apply", "--private-nodes")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_bucket_ownership_uses_raw_metadata_and_refuses_missing_project_number(self):
        self.adopt()
        accepted = self.invoke("--apply")
        self.assertEqual(accepted.returncode, 0, accepted.stderr)
        describe = next(call["args"] for call in self.calls() if call["args"][:3] == ["storage", "buckets", "describe"])
        self.assertIn("--raw", describe)
        self.state = json.loads(self.state_path.read_text())
        del self.state["bucket"]["projectNumber"]
        self.save()
        self.log.unlink()
        rejected = self.invoke("--apply")
        self.assertEqual(rejected.returncode, 1)
        self.assertIn("cannot verify ownership", rejected.stderr)
        self.assertEqual(self.mutations(), [])

    def test_worker_pool_is_pinned_sized_selected_and_reused(self):
        result = self.invoke("--apply", "--runtime=microvm")
        self.assertEqual(result.returncode, 0, result.stderr)
        worker = json.loads(result.stdout)["worker"]
        self.assertEqual(worker["sandbox_config"], "microvm")
        self.assertEqual(worker["name"], "branchyard-microvm")
        self.assertEqual(worker["selector"], {"workload": "branchyard", "branchyard-runtime": "microvm"})
        state = json.loads(self.state_path.read_text())
        pool = state["workerpool"]
        self.assertEqual(pool["spec"]["sandboxClass"], "microvm")
        self.assertEqual(pool["spec"]["template"]["labels"], worker["selector"])
        self.assertEqual(pool["spec"]["template"]["resources"]["limits"], {"cpu": "2", "memory": "4Gi"})
        self.assertEqual(pool["spec"]["template"]["nodeSelector"], {gke.VERSION_LABEL: gke.BUILD_VERSION})
        self.assertTrue(any(call["tool"] == "bash" and "--install" in call["args"] for call in self.calls()))
        again = self.invoke("--apply", "--runtime=microvm")
        self.assertEqual(again.returncode, 0, again.stderr)
        self.assertEqual(sum(call["tool"] == "kubectl" and "create" in call["args"] for call in self.calls()), 2)

    def test_foreign_worker_pool_and_spec_changes_are_refused(self):
        result = self.invoke("--apply")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.state = json.loads(self.state_path.read_text())
        self.state["workerpool"]["metadata"]["labels"][gke.OWNER_LABEL] = "foreign"
        self.save()
        refused = self.invoke("--apply", "--adopt-existing")
        self.assertEqual(refused.returncode, 1)
        self.assertIn("WorkerPool belongs", refused.stderr)
        self.state["workerpool"]["metadata"]["labels"][gke.OWNER_LABEL] = "test-substrate"
        self.state["workerpool"]["spec"]["replicas"] = 9
        self.save()
        refused = self.invoke("--apply")
        self.assertEqual(refused.returncode, 1)
        self.assertIn("WorkerPool spec differs", refused.stderr)
        self.assertEqual(sum(call["tool"] == "kubectl" and "create" in call["args"] for call in self.calls()), 2)

    def test_worker_image_output_must_name_both_pinned_registry_refs(self):
        self.adopt()
        for output in (
            "ateom-gvisor: example.com/image:latest",
            "unrelated: gs://bucket/object",
            "ateom-gvisor: us-central1-docker.pkg.dev/test-project/substrate/ateom-gvisor@sha256:" + "a" * 64,
        ):
            with self.subTest(output=output):
                self.state["bad_worker_output"] = output
                self.save()
                failed = self.invoke("--apply")
                self.assertEqual(failed.returncode, 1)
                self.assertFalse(any(call["tool"] == "kubectl" and "create" in call["args"] for call in self.calls()))

    def test_interruption_returns_a_resume_instruction_without_a_traceback(self):
        error = io.StringIO()
        with patch.object(gke, "apply", side_effect=KeyboardInterrupt), redirect_stderr(error):
            self.assertEqual(gke.main([*self.args, "--apply"]), 130)
        self.assertIn("resume", error.getvalue())
        self.assertNotIn("Traceback", error.getvalue())

    def test_unsafe_or_incomplete_arguments_are_refused_without_commands(self):
        for arguments in (
            ("--version=1.35.1-gke.1",),
            ("--context=other",),
            ("--registry=foreign.example/repo",),
            ("--service-account=123-compute@developer.gserviceaccount.com",),
            ("--machine-type=e2-standard-4",),
        ):
            with self.subTest(arguments=arguments):
                result = self.invoke(*arguments)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(self.calls(), [])


if __name__ == "__main__":
    unittest.main()
