#!/usr/bin/env python3
"""Hermetic guided cloud orchestration: ownership, readiness, release pins and private errors."""

import argparse
import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
import cloud_setup as cloud
import substrate_gke as gke

IMAGE = "ghcr.io/vamsiramakrishnan/branchyard-agy@sha256:" + "a" * 64


class CloudSetupTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.config = gke.Config(
            "test-project",
            "us-central1-a",
            "test-cluster",
            "test-snapshots",
            "us-central1-docker.pkg.dev/test-project/test-cluster",
            "1.37.1-gke.1",
            "worker@test-project.iam.gserviceaccount.com",
            self.root / "source",
            self.root,
        )
        self.base = [
            "--kind",
            "gce",
            "--project",
            "test-project",
            "--zone",
            "us-central1-a",
            "--name",
            "test-worker",
            "--repo",
            str(self.root),
            "--version",
            "0.0.1",
        ]

    def main(self, args):
        out, err = io.StringIO(), io.StringIO()
        with (
            patch.object(Path, "home", return_value=self.root),
            contextlib.redirect_stdout(out),
            contextlib.redirect_stderr(err),
        ):
            code = cloud.main(args)
        return code, out.getvalue(), err.getvalue()

    def test_plan_is_offline_and_invalid_inputs_do_not_create_state(self):
        with (
            patch.object(cloud, "command", side_effect=AssertionError("offline subprocess")),
            patch.object(cloud.urllib.request, "urlopen", side_effect=AssertionError("offline network")),
        ):
            code, out, err = self.main(self.base)
            self.assertEqual(code, 0, err)
            self.assertEqual(json.loads(out)["runtime"], "local")
            for extra in (["--zone=../../escape"], ["--version=latest"], ["--runtime=microvm"], ["--name=ends-"]):
                code, out, err = self.main([*self.base, *extra])
                self.assertEqual(code, 1)
            self.assertFalse((self.root / ".local").exists())

    def test_image_metadata_is_bounded_version_bound_and_pinned(self):
        response = MagicMock()
        response.__enter__.return_value = response
        response.geturl.return_value = "https://release.example.invalid/runtime-images.json"
        good = {"branchyard_version": "0.0.1", "agy_image": IMAGE}
        with patch.object(cloud.urllib.request, "urlopen", return_value=response):
            response.read.return_value = json.dumps(good).encode()
            self.assertEqual(cloud.image_for_release("0.0.1"), IMAGE)
            for payload in (
                b"[]",
                b"not-json private-token",
                json.dumps({**good, "branchyard_version": "9.9.9"}).encode(),
                json.dumps({**good, "agy_image": "ghcr.io/foreign/image:latest"}).encode(),
                b"a" * 65537,
            ):
                response.read.return_value = payload
                with self.assertRaises(gke.SetupError) as error:
                    cloud.image_for_release("0.0.1")
                self.assertNotIn("private-token", str(error.exception))
            response.read.return_value = json.dumps(good).encode()
            response.geturl.return_value = "http://example.invalid/metadata"
            with self.assertRaises(gke.SetupError):
                cloud.image_for_release("0.0.1")
        with self.assertRaises(gke.SetupError):
            cloud.image_for_release("latest")

    def test_command_failure_does_not_copy_diagnostics_or_untrusted_executable(self):
        failure = subprocess.CompletedProcess([], 1, "private-token", "private-model-key")
        with patch.object(subprocess, "run", return_value=failure):
            with self.assertRaises(gke.SetupError) as error:
                cloud.command(["/tmp/private-token", "private-model-key"])
            self.assertNotIn("private", str(error.exception))
        with patch.object(subprocess, "run", side_effect=OSError("private-token")):
            with self.assertRaises(gke.SetupError) as error:
                cloud.command(["/tmp/private-token"])
            self.assertNotIn("private-token", str(error.exception))

    def test_private_replacement_does_not_follow_a_symlink(self):
        target = self.root / "target"
        target.write_text("preserve")
        destination = self.root / "secret"
        destination.symlink_to(target)
        cloud.private_file(destination, "new-token")
        self.assertEqual(target.read_text(), "preserve")
        self.assertEqual(destination.read_text(), "new-token")
        self.assertEqual(destination.stat().st_mode & 0o777, 0o600)

    def test_compatible_gke_release_is_selected_numerically(self):
        self.assertEqual(
            cloud.gke_version({"validMasterVersions": ["1.36.9-gke.1", "1.37.2-gke.10", "1.37.10-gke.2"]}),
            "1.37.10-gke.2",
        )
        for metadata in ({}, {"validMasterVersions": ["1.38.0-gke.1"]}, {"validMasterVersions": "bad"}):
            with self.assertRaises(gke.SetupError):
                cloud.gke_version(metadata)

    def test_bucket_and_account_names_are_bounded_stable_and_deployment_specific(self):
        name = cloud.bucket_name("p" * 30, "c" * 30)
        self.assertLessEqual(len(name), 63)
        email = cloud.account_email("test-project", "us-central1-a", "c" * 30)
        self.assertEqual(email, cloud.account_email("test-project", "us-central1-a", "c" * 30))
        self.assertLessEqual(len(email.split("@", 1)[0]), 30)
        self.assertNotEqual(email, cloud.account_email("test-project", "us-west1-a", "c" * 30))

    def test_gce_runtime_version_and_named_secret_reach_the_helper(self):
        remote = "ssh://branchyard-test-worker-01234567/srv/branchyard/test-worker"
        with (
            patch.object(cloud.shutil, "which", return_value="/bin/tool"),
            patch.object(cloud, "image_for_release", return_value=IMAGE),
            patch.object(cloud, "command", return_value=json.dumps({"remote": remote})) as command,
        ):
            code, out, err = self.main(
                [*self.base, "--runtime=microsandbox", "--forward-secret=GEMINI_API_KEY", "--apply"]
            )
        self.assertEqual(code, 0, err)
        self.assertEqual(json.loads(out)["remote.url"], remote)
        argv = command.call_args.args[0]
        self.assertEqual(argv[argv.index("--backend") + 1], "microsandbox")
        self.assertEqual(argv[argv.index("--version") + 1], "0.0.1")
        self.assertEqual(argv[argv.index("--forward-secret") + 1], "GEMINI_API_KEY")
        self.assertNotIn("--service-account", argv)

    def test_malformed_provider_json_is_not_echoed(self):
        with (
            patch.object(cloud.shutil, "which", return_value="/bin/tool"),
            patch.object(cloud, "image_for_release", return_value=IMAGE),
            patch.object(cloud, "command", return_value="private-token-not-json"),
        ):
            code, out, err = self.main([*self.base, "--apply"])
        self.assertEqual(code, 1)
        self.assertNotIn("private-token", out + err)

    def test_gke_auto_account_is_visible_in_the_offline_plan(self):
        args = [*self.base, "--kind=gke", "--runtime=gvisor"]
        code, out, err = self.main(args)
        self.assertEqual(code, 0, err)
        plan = json.loads(out)
        self.assertEqual(plan["service_account"], cloud.account_email("test-project", "us-central1-a", "test-worker"))
        self.assertEqual(plan["node_role"], "roles/container.defaultNodeServiceAccount")
        self.assertEqual(plan["actor_egress"], "unrestricted")

    def test_auto_account_only_grants_its_owned_identity_and_reuses_binding(self):
        run = MagicMock()
        marker = f"Branchyard deployment {self.config.project}/{self.config.zone}/{self.config.cluster}"
        account = {"email": self.config.service_account, "description": marker}
        role = "roles/container.defaultNodeServiceAccount"
        policy = {"bindings": [{"role": role, "members": [f"serviceAccount:{self.config.service_account}"]}]}
        run.json.side_effect = [None, account, {"bindings": []}, account, policy]
        cloud.ensure_node_account(self.config, run)
        cloud.ensure_node_account(self.config, run)
        calls = [call.args[0] for call in run.run.call_args_list]
        self.assertEqual(sum("create" in args for args in calls), 1)
        bindings = [args for args in calls if "add-iam-policy-binding" in args]
        self.assertEqual(len(bindings), 1)
        self.assertIn("--condition=None", bindings[0])
        self.assertIn(f"--member=serviceAccount:{self.config.service_account}", bindings[0])
        self.assertIn(f"--role={role}", bindings[0])
        record = json.loads((self.root / "node-account.json").read_text())
        self.assertEqual(record["email"], self.config.service_account)

    def test_foreign_auto_account_is_not_modified(self):
        run = MagicMock()
        run.json.return_value = {"email": self.config.service_account, "description": "someone else"}
        with self.assertRaisesRegex(gke.SetupError, "not owned"):
            cloud.ensure_node_account(self.config, run)
        run.run.assert_not_called()

    def test_pinned_source_cache_survives_retry_without_recloning(self):
        source = self.root / "source"
        calls = []

        def command(args, **kwargs):
            calls.append(args)
            if args[:2] == ["git", "clone"]:
                Path(args[-1]).mkdir()
            return gke.SOURCE_PIN if args[:2] == ["git", "rev-parse"] else ""

        with patch.object(cloud, "command", side_effect=command):
            cloud.ensure_source(source)
            cloud.ensure_source(source)
        self.assertEqual(sum(args[:2] == ["git", "clone"] for args in calls), 1)
        with patch.object(cloud, "command", return_value="different"), self.assertRaises(gke.SetupError):
            cloud.ensure_source(source)
        self.assertTrue(source.exists())

    def test_failed_clone_cleans_only_its_temporary_directory(self):
        with (
            patch.object(cloud, "command", side_effect=gke.SetupError("clone failed")),
            self.assertRaises(gke.SetupError),
        ):
            cloud.ensure_source(self.root / "source")
        self.assertEqual(list(self.root.iterdir()), [])

    def test_tunnel_uses_atomic_ephemeral_binding_and_disables_websockets(self):
        args = argparse.Namespace(
            kubeconfig="/state/kube",
            context="context",
            service="api",
            port=0,
            remote_port=443,
            ready_file=str(self.root / "ready.json"),
        )
        self.assertIn(":443", cloud.forwarding_command(args))
        child = MagicMock()
        child.pid = 456
        child.poll.return_value = 1
        child.stdout = io.StringIO("Forwarding from 127.0.0.1:43001 -> 443\n")
        event = MagicMock()
        event.is_set.return_value = False
        event.wait.return_value = True
        with (
            patch.object(cloud.threading, "Event", return_value=event),
            patch.object(cloud.signal, "signal", return_value=0),
            patch.object(subprocess, "Popen", return_value=child) as spawn,
            contextlib.redirect_stdout(io.StringIO()),
        ):
            self.assertEqual(cloud.tunnel(args), 0)
        self.assertEqual(args.port, 43001)
        self.assertEqual(spawn.call_args.kwargs["env"]["KUBECTL_PORT_FORWARD_WEBSOCKETS"], "false")
        self.assertFalse(Path(args.ready_file).exists())

    def test_tunnel_waits_for_readiness_and_keeps_initial_port_identity(self):
        process = MagicMock()
        process.pid = 123
        process.poll.return_value = None
        with (
            patch.object(subprocess, "Popen", return_value=process) as spawn,
            patch.object(cloud, "ready_tunnel", side_effect=[None, 43001]),
            patch.object(cloud.threading.Event, "wait", return_value=False),
        ):
            self.assertEqual(cloud.ensure_tunnel(self.config, "api", 443, 0), 43001)
        record = json.loads((self.root / "api.pid").read_text())
        self.assertEqual(record, {"pid": 123, "requested_port": 0})
        spawn.assert_called_once()
        with (
            patch.object(cloud, "process_matches", return_value=True),
            patch.object(cloud, "ready_tunnel", return_value=43001),
            patch.object(subprocess, "Popen") as spawn,
        ):
            self.assertEqual(cloud.ensure_tunnel(self.config, "api", 443, 43001), 43001)
            spawn.assert_not_called()

    def test_foreign_tunnel_pid_is_never_adopted(self):
        cloud.private_file(self.root / "api.pid", json.dumps({"pid": 123, "requested_port": 0}))
        with (
            patch.object(cloud, "process_matches", return_value=False),
            patch.object(os, "kill"),
            patch.object(subprocess, "Popen") as spawn,
            self.assertRaisesRegex(gke.SetupError, "another process"),
        ):
            cloud.ensure_tunnel(self.config, "api", 443, 0)
        spawn.assert_not_called()

    def test_own_process_arguments_are_checked(self):
        if Path("/proc/self/cmdline").exists():
            expected = [os.fsdecode(arg) for arg in Path("/proc/self/cmdline").read_bytes().rstrip(b"\0").split(b"\0")]
            self.assertTrue(cloud.process_matches(os.getpid(), expected))
            self.assertFalse(cloud.process_matches(os.getpid(), ["unrelated"]))
        self.assertFalse(cloud.process_matches(0, []))

    def test_ready_listener_requires_both_owned_processes_and_matching_cluster_arguments(self):
        args = argparse.Namespace(kubeconfig="/state/kube", context="context", service="api", port=0, remote_port=443)
        expected = [
            sys.executable,
            str(Path(cloud.__file__).resolve()),
            "--tunnel",
            "--kubeconfig",
            args.kubeconfig,
            "--context",
            args.context,
            "--service",
            "api",
            "--port",
            "0",
            "--remote-port",
            "443",
        ]
        ready = self.root / "ready.json"
        record = {"pid": 123, "child_pid": 456, "port": 43001, "child_command": cloud.forwarding_command(args)}
        cloud.private_file(ready, json.dumps(record))
        with (
            patch.object(cloud, "process_matches", return_value=True),
            patch.object(cloud.socket, "create_connection", return_value=MagicMock()) as connect,
        ):
            self.assertEqual(cloud.ready_tunnel(ready, 123, expected), 43001)
            connect.assert_called_once_with(("127.0.0.1", 43001), timeout=1)
            record["child_command"] = ["kubectl", "--context=foreign"]
            cloud.private_file(ready, json.dumps(record))
            self.assertIsNone(cloud.ready_tunnel(ready, 123, expected))
        with (
            patch.object(cloud, "process_matches", return_value=False),
            patch.object(cloud.socket, "create_connection") as connect,
        ):
            self.assertIsNone(cloud.ready_tunnel(ready, 123, expected))
            connect.assert_not_called()

    def test_connection_waits_for_both_forwards_and_keeps_token_contents_private(self):
        def command(args, **kwargs):
            if "token" in args:
                return "fake-private-control-token"
            return json.dumps(
                {
                    "items": [
                        {
                            "spec": {
                                "signerName": "servicedns.podcert.ate.dev/identity",
                                "trustBundle": "public-ca",
                            }
                        }
                    ]
                }
            )

        with (
            patch.object(cloud, "command", side_effect=command),
            patch.object(cloud, "ensure_tunnel", side_effect=[43001, 43002]) as ensure,
        ):
            answers = cloud.connect(self.config)
        self.assertEqual(answers["substrate.endpoint"], "https://127.0.0.1:43001")
        self.assertEqual(answers["substrate.router"], "http://127.0.0.1:43002/")
        self.assertEqual(len(ensure.call_args_list), 2)
        self.assertEqual(json.loads((self.root / "ports.json").read_text()), {"api": 43001, "atenet-router": 43002})
        self.assertNotIn("fake-private-control-token", json.dumps(answers))
        self.assertEqual((self.root / "control.token").stat().st_mode & 0o777, 0o600)

    def test_gke_preflights_precede_account_and_template_smoke(self):
        args = argparse.Namespace(
            project="test-project",
            zone="us-central1-a",
            name="test-cluster",
            service_account=None,
            runtime="gvisor",
            by="by",
        )
        events = []
        answers = {"provider": "substrate", "harness": "antigravity"}

        def command(argv, **kwargs):
            if argv[:3] == ["gcloud", "container", "get-server-config"]:
                return json.dumps({"validMasterVersions": ["1.37.1-gke.1"]})
            events.append(argv[:3])
            return "{}"

        with (
            patch.object(cloud, "ensure_source"),
            patch.object(cloud, "command", side_effect=command),
            patch.object(cloud.gke, "prerequisites", side_effect=lambda *a: events.append("preflight")),
            patch.object(cloud, "ensure_node_account", side_effect=lambda *a: events.append("account")),
            patch.object(cloud.gke, "apply", side_effect=lambda *a: events.append("apply")) as apply,
            patch.object(cloud, "connect", return_value=answers),
        ):
            self.assertEqual(cloud.apply_gke(args, self.root, IMAGE), answers)
        self.assertEqual(events[:3], ["preflight", "account", "apply"])
        config = apply.call_args.args[0]
        self.assertEqual(config.worker_replicas, 2)
        self.assertEqual(config.service_account, cloud.account_email(args.project, args.zone, args.name))
        self.assertIn(["by", "substrate", "setup"], events)
        self.assertIn(["by", "substrate", "check"], events)

    def test_refresh_only_reconnects_an_owned_deployment(self):
        saved = {
            "mode": "applied",
            "source_revision": gke.SOURCE_PIN,
            "substrate_version": gke.BUILD_VERSION,
            "project": self.config.project,
            "zone": self.config.zone,
            "cluster": self.config.cluster,
            "bucket": self.config.bucket,
            "registry": self.config.registry,
            "worker": {"runtime": "gvisor"},
            "context": self.config.kube_context,
            "kubeconfig": str(self.config.kube_path),
        }
        cloud.private_file(self.root / "deployment.json", json.dumps(saved))
        cluster = {
            "status": "RUNNING",
            "endpoint": "192.0.2.1",
            "currentMasterVersion": "1.37.1-gke.1",
            "resourceLabels": {gke.OWNER_LABEL: self.config.cluster},
        }
        answers = {"substrate.endpoint": "https://127.0.0.1:43001"}
        with (
            patch.object(cloud, "command", side_effect=[json.dumps(cluster), "https://192.0.2.1"]) as command,
            patch.object(cloud, "connect", return_value=answers) as connect,
            patch.object(cloud, "ensure_source") as clone,
            patch.object(gke, "apply") as apply,
        ):
            code, out, err = self.main(["--refresh-state", str(self.root)])
        self.assertEqual(code, 0, err)
        self.assertEqual(json.loads(out), answers)
        self.assertEqual(connect.call_args.args[0].runtime, "gvisor")
        self.assertEqual([call.args[0][0] for call in command.call_args_list], ["gcloud", "kubectl"])
        clone.assert_not_called()
        apply.assert_not_called()

    def test_refresh_rejects_foreign_metadata_and_context_before_minting_tokens(self):
        saved = {
            "mode": "applied",
            "source_revision": "foreign",
            "substrate_version": gke.BUILD_VERSION,
        }
        cloud.private_file(self.root / "deployment.json", json.dumps(saved))
        with patch.object(cloud, "command") as command, patch.object(cloud, "connect") as connect:
            code, out, err = self.main(["--refresh-state", str(self.root)])
        self.assertEqual(code, 1, err)
        self.assertEqual(out, "")
        command.assert_not_called()
        connect.assert_not_called()
        self.root.chmod(0o755)
        with self.assertRaisesRegex(gke.SetupError, "private"):
            cloud.refresh_state(self.root)

    def test_google_alias_names_the_canonical_remote_secret(self):
        remote = "ssh://branchyard-test-worker-01234567/srv/branchyard/test-worker"
        with (
            patch.object(cloud.shutil, "which", return_value="/bin/tool"),
            patch.object(cloud, "image_for_release", return_value=IMAGE),
            patch.object(cloud, "command", return_value=json.dumps({"remote": remote})),
        ):
            code, out, err = self.main([*self.base, "--forward-secret=GOOGLE_API_KEY", "--apply"])
        self.assertEqual(code, 0, err)
        answers = json.loads(out)
        self.assertEqual(answers["secrets"], ["GEMINI_API_KEY"])
        self.assertEqual(answers["secret.GEMINI_API_KEY"], "GOOGLE_API_KEY")

    def test_secret_value_instead_of_name_is_never_echoed(self):
        code, out, err = self.main([*self.base, "--forward-secret=private-model-key"])
        self.assertEqual(code, 1)
        self.assertNotIn("private-model-key", out + err)


if __name__ == "__main__":
    unittest.main()
