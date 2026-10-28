#!/usr/bin/env python3
"""Exercise the GCE helper with fake cloud/SSH tools and a real temporary git repository."""

import hashlib
import json
import os
import shlex
import subprocess
import sys
import tempfile
import unittest
from dataclasses import asdict, replace
from pathlib import Path
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
import compute_gce as gce

ROOT = Path(__file__).resolve().parents[1]
FAKE = r"""
import json,os,shlex,sys
from pathlib import Path
args=sys.argv[1:]
state_path=Path(os.environ["FAKE_STATE"])
state=json.loads(state_path.read_text())
tool=Path(sys.argv[0]).name
credential = any("harnesses login antigravity --api-key" in arg for arg in args)
script=sys.stdin.read() if "--command=bash -se" in args else None
received = sys.stdin.read() if credential else None
if credential:
 state["credential_matches_source"] = received == os.environ.get(state.get("credential_source", "GEMINI_API_KEY"))
with Path(os.environ["FAKE_LOG"]).open("a") as log:
 log.write(json.dumps({"tool":tool,"args":args,"script":script})+"\n")
def done(value=""):
 state_path.write_text(json.dumps(state))
 print(json.dumps(value) if isinstance(value,(dict,list)) else value)
 sys.exit(0)
def fail(message,code=1):
 print(message,file=sys.stderr)
 sys.exit(code)
if tool=="ssh":
 done("hostname "+state.get("alias_host",args[-1]))
if args[:2]==["auth","list"]:done(state.get("account","developer@example.invalid"))
if args[:3]==["iam","service-accounts","describe"]:done({"email":args[3]})
if args[:3]==["compute","instances","describe"]:
 if state.get("describe_error"):fail(state["describe_error"])
 if state.get("instance") is None:fail(state.get("missing_error","NOT_FOUND"))
 done(state["instance"])
if args[:3]==["compute","instances","create"]:
 if state.get("create_error"):fail(state["create_error"])
 state["instance"]=state["desired"]
 done()
if args[:3]==["compute","disks","describe"]:
 done(state.get("disk",{"sizeGb":"100","sourceImage":"https://compute.googleapis.com/compute/v1/projects/ubuntu-os-cloud/global/images/ubuntu-2404-noble-amd64-v20261028"}))
if args[:3]==["compute","firewall-rules","describe"]:
 if state.get("firewall_error"):fail(state["firewall_error"])
 if state.get("firewall") is None:fail("NOT_FOUND")
 done(state["firewall"])
if args[:3]==["compute","firewall-rules","create"]:
 if state.get("firewall_create_error"):fail("PERMISSION_DENIED")
 if not state.get("firewall_disappears"):
  state["firewall"]=state["desired_firewall"]
 done()
if args[:2]==["compute","ssh"]:
 if credential and state.get("credential_error"):
  print(received)
  fail(received)
 if "--dry-run" in args:
  proxy=[]
  if "--tunnel-through-iap" in args:
   proxy=["-o","ProxyCommand gcloud compute start-iap-tunnel example %p --listen-on-stdin --project=test-project",
    "-o","ProxyUseFdpass=no"]
  done(shlex.join(["ssh","-i","/tmp/key","-o","UserKnownHostsFile=/tmp/known","-o","HostKeyAlias=compute.123","-o","CheckHostIP=no",*proxy,"user@192.0.2.1"]))
 if script is not None and state.get("bootstrap_error"):fail("private-token-must-not-be-printed")
 if script is None and state.get("ssh_error"):
  fail("private-token-must-not-be-printed",state.get("ssh_error_exit",255))
 done()
if args[:2]==["compute","scp"]:done()
fail("unexpected fake command")
"""


class ComputeGceTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.home = self.root / "home"
        self.home.mkdir()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        fake = self.bin / "fake"
        fake.write_text(f"#!{sys.executable}\n" + FAKE)
        fake.chmod(0o755)
        for tool in ("gcloud", "ssh"):
            (self.bin / tool).symlink_to(fake)
        self.log = self.root / "commands.jsonl"
        self.state_path = self.root / "cloud.json"
        self.env = dict(
            os.environ,
            PATH=f"{self.bin}:/usr/bin:/bin",
            HOME=str(self.home),
            FAKE_STATE=str(self.state_path),
            FAKE_LOG=str(self.log),
            GEMINI_API_KEY="private-model-key-must-not-appear",
        )
        self.config = gce.Config("test-project", "us-central1-a", "branchyard-test", self.root / "state")
        self.state = {"instance": None, "desired": self.instance(self.config)}
        self.save()

    def instance(self, config):
        return {
            "name": config.name,
            "zone": f"projects/{config.project}/zones/{config.zone}",
            "machineType": f"zones/{config.zone}/machineTypes/{config.machine_type}",
            "status": "RUNNING",
            "labels": {gce.OWNER_LABEL: config.name},
            "metadata": {"items": [{"key": "branchyard-configuration-sha256", "value": config.fingerprint}]},
            "advancedMachineFeatures": {"enableNestedVirtualization": config.backend == "microsandbox"},
            "serviceAccounts": [{"email": config.service_account}] if config.service_account else [],
            "networkInterfaces": [
                {
                    "network": f"global/networks/{config.network}",
                    "subnetwork": f"regions/us-central1/subnetworks/{config.subnet}",
                    "accessConfigs": [] if config.internal_ip else [{"natIP": "192.0.2.1"}],
                }
            ],
            "disks": [{"boot": True, "autoDelete": True, "source": "zones/us-central1-a/disks/test-disk"}],
            "tags": {"items": [config.iap_firewall["name"]] if config.tunnel_through_iap else []},
        }

    def firewall(self, config):
        return {
            **config.iap_firewall,
            "network": f"projects/{config.project}/global/networks/{config.network}",
            "description": config.iap_description,
            "disabled": False,
            "id": "owned-firewall-id",
        }

    def save(self):
        self.state_path.write_text(json.dumps(self.state))

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def apply(self, config=None):
        with patch.dict(os.environ, self.env, clear=True):
            return gce.apply(config or self.config)

    def test_forwarded_secret_uses_stdin_and_is_absent_from_all_serialized_records(self):
        config = replace(self.config, forward_secret="GEMINI_API_KEY")
        self.assertEqual(config.fingerprint, self.config.fingerprint)
        report = self.apply(config)
        state = json.loads(self.state_path.read_text())
        self.assertTrue(state["credential_matches_source"])
        calls = self.calls()
        transfer = [
            call for call in calls if any("harnesses login antigravity --api-key" in arg for arg in call["args"])
        ]
        self.assertEqual(len(transfer), 1)
        self.assertIsNone(transfer[0]["script"])
        serialized = json.dumps(report) + self.log.read_text() + (config.state_dir / "install.log").read_text()
        self.assertNotIn(self.env["GEMINI_API_KEY"], serialized)
        self.assertEqual(report["credential_source"], "GEMINI_API_KEY")
        self.assertEqual(report["qualification"]["model"], "not_checked")

    def test_credential_failure_discards_remote_echoes_and_keeps_errors_redacted(self):
        self.state["credential_error"] = True
        self.save()
        with self.assertRaises(gce.SetupError) as error:
            self.apply(replace(self.config, forward_secret="GEMINI_API_KEY"))
        self.assertNotIn(self.env["GEMINI_API_KEY"], str(error.exception))
        self.assertNotIn(self.env["GEMINI_API_KEY"], self.log.read_text())
        self.assertNotIn(self.env["GEMINI_API_KEY"], (self.config.state_dir / "install.log").read_text())
        self.assertFalse((self.config.state_dir / "deployment.json").exists())

    def test_missing_or_invalid_selected_credentials_stop_before_cloud_mutation(self):
        for value in ("", "two\nlines", "with spaces"):
            with (
                patch.dict(os.environ, dict(self.env, GEMINI_API_KEY=value), clear=True),
                self.assertRaisesRegex(gce.SetupError, "export a single nonempty"),
            ):
                gce.apply(replace(self.config, forward_secret="GEMINI_API_KEY"))
        with self.assertRaisesRegex(gce.SetupError, "forward-secret"):
            gce.plan(replace(self.config, forward_secret="UNSUPPORTED_KEY"))
        self.assertFalse(self.log.exists())
        self.assertFalse(self.config.state_dir.exists())

    def test_worker_configuration_uses_native_engines_and_a_file_backed_server_registry(self):
        script = gce.worker_configuration(self.config)
        for required in [
            "by init project",
            "by init server",
            "secrets/GEMINI_API_KEY",
            "BRANCHYARD_USER_CONFIG",
            'config["harness_commands"]',
            'config["allow_client_commands"] = False',
            "by serve --config",
            "--check",
        ]:
            self.assertIn(required, script)
        self.assertNotIn(self.env["GEMINI_API_KEY"], script)
        result = subprocess.run(["bash", "-n"], input=script, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_plan_is_offline_and_names_cloud_and_ssh_changes(self):
        with patch.object(subprocess, "run", side_effect=AssertionError("offline plan ran a command")):
            report = gce.plan(self.config)
        self.assertEqual(report["mode"], "plan")
        self.assertEqual(report["service_account"], "none; no default Compute Engine account selected")
        self.assertIn("ssh://branchyard-", report["remote"])
        self.assertEqual(report["qualification"]["model"], "not_checked")
        self.assertFalse(self.config.state_dir.exists())
        self.assertFalse(self.log.exists())

    def test_iap_plan_is_offline_and_direct_fingerprints_remain_compatible(self):
        legacy = asdict(self.config)
        for key in ("state_dir", "repo", "forward_secret", "tunnel_through_iap"):
            legacy.pop(key)
        self.assertEqual(
            self.config.fingerprint, hashlib.sha256(json.dumps(legacy, sort_keys=True).encode()).hexdigest()
        )
        config = replace(self.config, tunnel_through_iap=True, internal_ip=True)
        with patch.object(subprocess, "run", side_effect=AssertionError("offline plan ran a command")):
            report = gce.plan(config)
        self.assertNotEqual(config.fingerprint, self.config.fingerprint)
        self.assertEqual(report["ssh_transport"], "iap")
        self.assertEqual(report["network"], "private address; SSH through IAP")
        self.assertTrue(report["target"]["internal_ip"])
        rule = report["iap_firewall"]
        self.assertEqual(rule["sourceRanges"], ["35.235.240.0/20"])
        self.assertEqual(rule["allowed"], [{"IPProtocol": "tcp", "ports": ["22"]}])
        self.assertEqual(rule["targetTags"], [config.alias + "-iap"])
        self.assertIn(config.fingerprint, rule["description"])
        self.assertIn("firewall-rules delete", rule["cleanup_command"])
        self.assertIsNone(gce.plan(self.config)["iap_firewall"])
        self.assertFalse(self.log.exists())

    def test_iap_create_and_resume_keep_the_owned_rule_and_tunnel_every_cloud_ssh(self):
        config = replace(self.config, tunnel_through_iap=True, internal_ip=True)
        self.state.update(desired=self.instance(config), desired_firewall=self.firewall(config))
        self.save()
        first = self.apply(config)
        self.assertEqual(self.apply(config), first)
        calls = self.calls()
        creates = [c["args"] for c in calls if c["args"][:3] == ["compute", "instances", "create"]]
        self.assertEqual(len(creates), 1)
        self.assertIn("--no-address", creates[0])
        self.assertIn(f"--tags={config.alias}-iap", creates[0])
        rules = [c["args"] for c in calls if c["args"][:3] == ["compute", "firewall-rules", "create"]]
        self.assertEqual(len(rules), 1)
        self.assertIn("--source-ranges=35.235.240.0/20", rules[0])
        self.assertIn("--rules=tcp:22", rules[0])
        for call in calls:
            if call["args"][:2] == ["compute", "ssh"]:
                self.assertIn("--tunnel-through-iap", call["args"])
                self.assertNotIn("--internal-ip", call["args"])
        self.assertEqual(first["iap_firewall"]["id"], "owned-firewall-id")
        self.assertEqual(first["ssh_transport"], "iap")
        self.assertIn("ProxyCommand gcloud compute start-iap-tunnel", Path(first["ssh_config"]).read_text())
        self.assertIn("--project=test-project", Path(first["ssh_config"]).read_text())
        self.assertEqual(gce.transport(self.config), [])
        self.assertEqual(gce.transport(replace(self.config, internal_ip=True)), ["--internal-ip"])

    def test_iap_refuses_foreign_or_widened_rules_before_creating_a_machine(self):
        config = replace(self.config, tunnel_through_iap=True)
        for changes in (
            {"description": "someone else's rule"},
            {"sourceRanges": ["0.0.0.0/0"]},
            {"allowed": [{"IPProtocol": "tcp"}]},
            {"priority": 999},
            {"network": "global/networks/other"},
            {"disabled": True},
            {"targetTags": ["shared-tag"]},
            {"sourceTags": ["other-source"]},
            {"targetServiceAccounts": ["other@example.invalid"]},
        ):
            with self.subTest(changes=changes):
                self.state["firewall"] = dict(self.firewall(config), **changes)
                self.save()
                with self.assertRaisesRegex(gce.SetupError, "IAP firewall rule is foreign"):
                    self.apply(config)
        self.assertFalse(any("create" in call["args"] for call in self.calls()))

    def test_iap_rule_permission_failure_and_removed_rule_stop_before_ssh(self):
        config = replace(self.config, tunnel_through_iap=True)
        self.state["firewall_error"] = "PERMISSION_DENIED private-token-must-not-be-printed"
        self.save()
        with self.assertRaises(gce.SetupError) as error:
            self.apply(config)
        self.assertNotIn("private-token", str(error.exception))
        self.assertFalse(any("create" in call["args"] for call in self.calls()))
        self.state.pop("firewall_error")
        self.state.update(desired=self.instance(config), firewall_disappears=True)
        self.save()
        with self.assertRaisesRegex(gce.SetupError, "organization-managed network enforcement"):
            self.apply(config)
        self.assertFalse(any(call["args"][:2] == ["compute", "ssh"] for call in self.calls()))

    def test_iap_rule_create_failure_resumes_the_owned_machine_without_mutating_foreign_rules(self):
        config = replace(self.config, tunnel_through_iap=True)
        self.state.update(
            desired=self.instance(config), desired_firewall=self.firewall(config), firewall_create_error=True
        )
        self.save()
        with self.assertRaises(gce.SetupError):
            self.apply(config)
        self.state = json.loads(self.state_path.read_text())
        self.state["firewall_create_error"] = False
        self.save()
        self.apply(config)
        self.assertEqual(sum(call["args"][:3] == ["compute", "instances", "create"] for call in self.calls()), 1)
        self.assertFalse(any("update" in call["args"] or "delete" in call["args"] for call in self.calls()))

    def test_iap_reuse_requires_the_owned_target_tag(self):
        config = replace(self.config, tunnel_through_iap=True)
        self.state["instance"] = self.instance(config)
        self.state["instance"]["tags"] = {}
        self.save()
        with self.assertRaisesRegex(gce.SetupError, "VM is foreign or differs"):
            self.apply(config)
        self.assertFalse(any("create" in call["args"] for call in self.calls()))

    def test_ssh_connection_failures_explain_the_selected_route_without_echoing_output(self):
        for iap in (False, True):
            with self.subTest(iap=iap):
                config = replace(self.config, tunnel_through_iap=iap)
                self.state.update(
                    instance=None, desired=self.instance(config), ssh_error=True, desired_firewall=self.firewall(config)
                )
                self.save()
                with (
                    patch.object(gce.time, "monotonic", side_effect=[0, 0, 180, 180]),
                    patch.object(gce.time, "sleep"),
                    self.assertRaisesRegex(gce.SetupError, "IAP tunnel IAM" if iap else "private VPC route") as error,
                ):
                    self.apply(config)
                self.assertNotIn("private-token", str(error.exception))

    def test_initial_ssh_readiness_retries_transient_guest_login_with_injected_time(self):
        run = Mock(spec=gce.Runner)
        run.run.side_effect = [gce.SshNotReady("publickey propagation"), gce.SshNotReady("connection refused"), ""]
        elapsed = [0.0]

        def advance(seconds):
            elapsed[0] += seconds

        gce.wait_for_ssh(self.config, run, now=lambda: elapsed[0], pause=advance)
        self.assertEqual(elapsed[0], 10)
        self.assertEqual(run.run.call_count, 3)
        for call in run.run.call_args_list:
            self.assertIn("--ssh-flag=-oConnectionAttempts=1", call.args[0])
            self.assertEqual(call.kwargs["timeout"], 30)

    def test_initial_ssh_readiness_stops_at_the_deadline_and_fails_fast_on_cloud_iam(self):
        run = Mock(spec=gce.Runner)
        run.run.side_effect = gce.SshNotReady("guest login did not become ready")
        elapsed = [0.0]

        def advance(seconds):
            elapsed[0] += seconds

        with self.assertRaisesRegex(gce.SetupError, "within 180 seconds"):
            gce.wait_for_ssh(self.config, run, now=lambda: elapsed[0], pause=advance)
        self.assertEqual(elapsed[0], 180)
        self.assertEqual(run.run.call_count, 36)
        run.reset_mock()
        run.run.side_effect = gce.SetupError("cloud authorization denied")
        pause = Mock()
        with self.assertRaisesRegex(gce.SetupError, "cloud authorization denied"):
            gce.wait_for_ssh(self.config, run, now=lambda: 0, pause=pause)
        self.assertEqual(run.run.call_count, 1)
        pause.assert_not_called()

    def test_ssh_timeouts_are_retryable_but_explicit_cloud_authorization_errors_are_not(self):
        run = gce.Runner(self.config)
        args = gce.ssh(self.config, "true")
        with (
            patch.object(subprocess, "run", side_effect=subprocess.TimeoutExpired(args, 7)) as command,
            self.assertRaises(gce.SshNotReady),
        ):
            run.run(args, timeout=7)
        self.assertEqual(command.call_args.kwargs["timeout"], 7)
        result = subprocess.CompletedProcess(args, 255, stdout="", stderr="PERMISSION_DENIED private-token")
        with patch.object(subprocess, "run", return_value=result), self.assertRaises(gce.SetupError) as error:
            run.run(args)
        self.assertNotIsInstance(error.exception, gce.SshNotReady)
        self.assertNotIn("private-token", str(error.exception))

    def test_validation_rejects_option_values_and_unpinned_images(self):
        for changes in (
            {"name": "--flag"},
            {"zone": "bad"},
            {"machine_type": "t2a-standard-4"},
            {"image": "ghcr.io/project/image:latest"},
            {"version": "latest"},
            {"disk_size": 1},
            {"service_account": "default"},
            {"backend": "unknown"},
        ):
            with self.subTest(changes=changes), self.assertRaises(gce.SetupError):
                gce.plan(replace(self.config, **changes))
        gce.plan(replace(self.config, image="ghcr.io/project/image@sha256:" + "a" * 64))

    def test_create_then_repeat_reuses_machine_and_preserves_user_ssh_config(self):
        ssh_dir = self.home / ".ssh"
        ssh_dir.mkdir()
        original = "Host work\n  HostName work.example.invalid\n"
        (ssh_dir / "config").write_text(original)
        first = self.apply()
        second = self.apply()
        self.assertEqual(first, second)
        creates = [call for call in self.calls() if call["args"][:3] == ["compute", "instances", "create"]]
        self.assertEqual(len(creates), 1)
        self.assertIn("--no-service-account", creates[0]["args"])
        self.assertIn("--no-scopes", creates[0]["args"])
        self.assertNotIn("--enable-nested-virtualization", creates[0]["args"])
        content = (ssh_dir / "config").read_text()
        self.assertTrue(content.endswith(original))
        self.assertEqual(content.count("Include "), 1)
        owned = Path(first["ssh_config"])
        self.assertIn('StrictHostKeyChecking "yes"', owned.read_text())
        self.assertEqual(owned.stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.config.state_dir.stat().st_mode & 0o777, 0o700)
        self.assertEqual(json.loads((self.config.state_dir / "deployment.json").read_text()), first)
        self.assertNotIn(self.env["GEMINI_API_KEY"], json.dumps(first) + self.log.read_text())

    def test_foreign_or_incompatible_machine_is_refused_before_bootstrap(self):
        for change in ("owner", "fingerprint", "machine", "nested", "account", "zone", "status"):
            instance = self.instance(self.config)
            if change == "owner":
                instance["labels"] = {}
            elif change == "fingerprint":
                instance["metadata"] = {}
            elif change == "machine":
                instance["machineType"] = "e2-standard-8"
            elif change == "nested":
                instance["advancedMachineFeatures"]["enableNestedVirtualization"] = True
            elif change == "account":
                instance["serviceAccounts"] = [{"email": "other@test-project.iam.gserviceaccount.com"}]
            elif change == "zone":
                instance["zone"] = "us-west1-a"
            else:
                instance["status"] = "TERMINATED"
            self.state["instance"] = instance
            self.save()
            with self.subTest(change=change), self.assertRaises(gce.SetupError):
                self.apply()
        self.assertFalse(any(call["args"][:2] == ["compute", "ssh"] for call in self.calls()))

    def test_standard_gcloud_missing_resource_diagnostic_creates_a_fresh_machine(self):
        self.state["missing_error"] = (
            "ERROR: (gcloud.compute.instances.describe) Could not fetch resource:\n"
            " - The resource 'projects/test-project/zones/us-central1-a/instances/branchyard-test' was not found\n"
        )
        self.save()
        report = self.apply()
        self.assertEqual(report["mode"], "applied")
        creates = [call for call in self.calls() if call["args"][:3] == ["compute", "instances", "create"]]
        self.assertEqual(len(creates), 1)

    def test_capacity_failure_names_the_machine_and_zone_remedies_without_echoing_output(self):
        for diagnostic in (
            "ZONE_RESOURCE_POOL_EXHAUSTED_WITH_DETAILS: private-token-must-not-be-printed",
            "The zone 'projects/test-project/zones/us-central1-a' does not have enough resources available",
        ):
            with self.subTest(diagnostic=diagnostic):
                self.state["create_error"] = diagnostic
                self.save()
                with self.assertRaisesRegex(gce.SetupError, "capacity.*--zone or --machine-type") as error:
                    self.apply()
                self.assertNotIn("private-token", str(error.exception))
        self.assertFalse(any(call["args"][:2] == ["compute", "ssh"] for call in self.calls()))

    def test_permission_diagnostic_wins_over_embedded_missing_markers(self):
        for diagnostic in (
            "PERMISSION_DENIED: cannot inspect project-404; resource was not found",
            "Required 'compute.instances.get' permission for "
            "'projects/project-404/zones/us-central1-a/instances/branchyard-test'",
        ):
            with self.subTest(diagnostic=diagnostic):
                self.state["describe_error"] = diagnostic
                self.save()
                with self.assertRaises(gce.SetupError):
                    self.apply()
                self.assertFalse(any("create" in call["args"] for call in self.calls()))

    def test_permission_failure_is_not_treated_as_a_missing_machine(self):
        self.state["describe_error"] = "PERMISSION_DENIED private-token-must-not-be-printed"
        self.save()
        with self.assertRaises(gce.SetupError) as context:
            self.apply()
        self.assertNotIn("private-token", str(context.exception))
        self.assertFalse(any("create" in call["args"] for call in self.calls()))

    def test_changed_network_and_boot_disk_are_refused(self):
        self.state["instance"] = self.instance(self.config)
        self.state["instance"]["networkInterfaces"][0]["network"] = "global/networks/other"
        self.save()
        with self.assertRaisesRegex(gce.SetupError, "differs"):
            self.apply()
        self.state["instance"] = self.instance(self.config)
        self.state["disk"] = {"sizeGb": "10", "sourceImage": "another-system"}
        self.save()
        with self.assertRaisesRegex(gce.SetupError, "boot disk"):
            self.apply()

    def test_missing_login_and_tool_have_actionable_remedies(self):
        self.state["account"] = ""
        self.save()
        with self.assertRaisesRegex(gce.SetupError, "gcloud auth login"):
            self.apply()
        with (
            patch.dict(os.environ, self.env, clear=True),
            patch.object(gce.shutil, "which", return_value=None),
            self.assertRaisesRegex(gce.SetupError, "install gcloud"),
        ):
            gce.apply(self.config)

    def test_bootstrap_failure_is_private_and_resume_reuses_owned_machine(self):
        self.state["bootstrap_error"] = True
        self.save()
        with self.assertRaises(gce.SetupError) as context:
            self.apply()
        self.assertNotIn("private-token", str(context.exception))
        self.assertIn("install.log", str(context.exception))
        self.assertIn("private-token", (self.config.state_dir / "install.log").read_text())
        state = json.loads(self.state_path.read_text())
        state["bootstrap_error"] = False
        self.state_path.write_text(json.dumps(state))
        self.apply()
        self.assertEqual(sum("create" in call["args"] for call in self.calls()), 1)

    def test_native_ssh_alias_conflict_is_refused(self):
        self.state["alias_host"] = "unrelated.example.invalid"
        self.save()
        with self.assertRaisesRegex(gce.SetupError, "SSH alias already"):
            self.apply()
        self.assertFalse((self.home / ".ssh/config").exists())

    def test_microsandbox_uses_nested_virtualization_and_pinned_runtime(self):
        config = replace(self.config, backend="microsandbox", internal_ip=True)
        self.state["desired"] = self.instance(config)
        self.save()
        report = self.apply(config)
        create = next(call["args"] for call in self.calls() if "create" in call["args"])
        self.assertIn("--enable-nested-virtualization", create)
        self.assertIn("--no-address", create)
        scripts = [call["script"] for call in self.calls() if call["script"]]
        self.assertEqual(len(scripts), 2)
        for required in (
            "--runtime",
            gce.MSB_ARCHIVE_SHA256,
            "msb context --format json",
            "msb image load",
            "MSB_HOME=/var/lib/by-msb",
        ):
            self.assertIn(required, scripts[0])
        self.assertIn('usermod -aG kvm "$(id -un)"', scripts[0])
        self.assertNotIn("by init project", scripts[0])
        self.assertIn("by init project", scripts[1])
        self.assertIn("by init server", scripts[1])
        self.assertIn("stage=$(mktemp -d)", scripts[1])
        self.assertIn(f"cd {config.remote_path}", scripts[1])
        self.assertEqual(report["qualification"]["microsandbox_hard_memory_ceiling"], "not_qualified")

    def test_generated_bootstrap_is_valid_bash_for_both_backends_and_repository_modes(self):
        for backend in ("local", "microsandbox"):
            for head in (None, "a" * 40):
                script = gce.bootstrap(replace(self.config, backend=backend), head)
                syntax = subprocess.run(["bash", "-n"], input=script, text=True, capture_output=True, check=False)
                self.assertEqual(syntax.returncode, 0, syntax.stderr)
                self.assertNotIn("msb init", script)
                self.assertNotIn(self.env["GEMINI_API_KEY"], script)
            configured = gce.configure_worker(replace(self.config, backend=backend))
            syntax = subprocess.run(["bash", "-n"], input=configured, text=True, capture_output=True, check=False)
            self.assertEqual(syntax.returncode, 0, syntax.stderr)
            self.assertIn("umask 077", configured)
            self.assertNotIn(self.env["GEMINI_API_KEY"], configured)

    def test_existing_selected_service_account_is_verified_without_iam_mutations(self):
        config = replace(self.config, service_account="worker@test-project.iam.gserviceaccount.com")
        self.state["desired"] = self.instance(config)
        self.save()
        self.apply(config)
        self.assertTrue(any(call["args"][:3] == ["iam", "service-accounts", "describe"] for call in self.calls()))
        self.assertFalse(any("add-iam-policy-binding" in call["args"] for call in self.calls()))

    def test_repository_transfer_uses_a_clean_committed_bundle(self):
        repo = self.root / "source"
        repo.mkdir()
        subprocess.run(["git", "init", "-qb", "main", str(repo)], check=True)
        (repo / "README.md").write_text("fixture\n")
        subprocess.run(["git", "-C", str(repo), "add", "."], check=True)
        subprocess.run(
            [
                "git",
                "-C",
                str(repo),
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
            check=True,
        )
        config = replace(self.config, repo=repo)
        report = self.apply(config)
        self.assertRegex(report["source_head"], r"^[0-9a-f]{40}$")
        self.assertTrue(any(call["args"][:2] == ["compute", "scp"] for call in self.calls()))
        iap = replace(config, tunnel_through_iap=True, internal_ip=True)
        self.state = json.loads(self.state_path.read_text())
        self.state.update(instance=None, desired=self.instance(iap), desired_firewall=self.firewall(iap))
        self.save()
        self.apply(iap)
        transfer = [call["args"] for call in self.calls() if call["args"][:2] == ["compute", "scp"]][-1]
        self.assertIn("--tunnel-through-iap", transfer)
        self.assertNotIn("--internal-ip", transfer)
        (repo / "private.txt").write_text("not committed")
        count = len(self.calls())
        with self.assertRaisesRegex(gce.SetupError, "uncommitted or untracked"):
            self.apply(config)
        self.assertFalse(any(call["args"][:1] == ["compute"] for call in self.calls()[count:]))

    def test_cli_default_plan_and_errors_are_machine_readable_and_secret_free(self):
        args = [
            sys.executable,
            str(ROOT / "tools/compute_gce.py"),
            "--project=test-project",
            "--zone=us-central1-a",
            "--name=branchyard-test",
            f"--state-dir={self.config.state_dir}",
        ]
        result = subprocess.run(args, env=self.env, text=True, capture_output=True, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["mode"], "plan")
        self.assertFalse(self.log.exists())
        invalid = subprocess.run([*args, "--version=latest"], env=self.env, text=True, capture_output=True, check=False)
        self.assertEqual(invalid.returncode, 1)
        self.assertIn("pin a Branchyard release", invalid.stderr)
        self.assertNotIn(self.env["GEMINI_API_KEY"], invalid.stdout + invalid.stderr)

    def test_default_release_and_custom_version_select_matching_worker_images(self):
        args = [
            sys.executable,
            str(ROOT / "tools/compute_gce.py"),
            "--project=test-project",
            "--zone=us-central1-a",
            "--name=branchyard-test",
            f"--state-dir={self.config.state_dir}",
        ]
        for flags, version in (([], gce.RELEASE_VERSION), (["--version=2.3.4"], "2.3.4")):
            out = subprocess.run([*args, *flags], env=self.env, text=True, capture_output=True, check=False)
            self.assertEqual(out.returncode, 0, out.stderr)
            target = json.loads(out.stdout)["target"]
            self.assertEqual(target["version"], version)
            self.assertEqual(target["image"], f"{gce.IMAGE_REPOSITORY}:{version}")
        self.assertEqual(self.config.version, gce.RELEASE_VERSION)
        self.assertEqual(self.config.image, f"{gce.IMAGE_REPOSITORY}:{gce.RELEASE_VERSION}")
        self.assertFalse(self.log.exists())

    def test_gcloud_response_shape_is_checked(self):
        runner = gce.Runner(self.config)
        for response in ("not-json", "[]"):
            with patch.object(runner, "run", return_value=response), self.assertRaises(gce.SetupError):
                runner.json(["gcloud", "example"])
        with patch.object(runner, "run", return_value=None):
            self.assertIsNone(runner.json(["gcloud", "example"], optional=True))

    def test_ssh_owned_file_refuses_replacement_and_newline_metadata(self):
        run = gce.Runner(self.config)
        original = "ssh -i /tmp/key -o UserKnownHostsFile=/tmp/known user@192.0.2.1"
        with patch.dict(os.environ, self.env, clear=True), patch.object(run, "run", return_value=""):
            path = gce.managed_ssh(self.config, original, run)
            path.write_text("owned by someone else\n")
            with self.assertRaisesRegex(gce.SetupError, "not owned"):
                gce.managed_ssh(self.config, original, run)
            path.unlink()
            with self.assertRaisesRegex(gce.SetupError, "unusable SSH"):
                gce.managed_ssh(
                    self.config, "ssh -i '/tmp/key\nother' -o UserKnownHostsFile=/tmp/known user@192.0.2.1", run
                )
            with self.assertRaisesRegex(gce.SetupError, "SSH user and host"):
                gce.managed_ssh(self.config, "ssh example.invalid", run)
            with self.assertRaisesRegex(gce.SetupError, "incomplete SSH"):
                gce.managed_ssh(self.config, "ssh user@192.0.2.1", run)
            with self.assertRaisesRegex(gce.SetupError, "incomplete SSH command"):
                gce.managed_ssh(self.config, "ssh 'incomplete", run)

    def test_iap_alias_preserves_gcloud_proxy_command_as_a_command_with_both_option_separators(self):
        config = replace(self.config, tunnel_through_iap=True)
        proxy = (
            "/usr/bin/python3 /sdk/gcloud.py compute start-iap-tunnel example %p "
            "--listen-on-stdin --project=test-project"
        )
        for separator in ("=", " "):
            with self.subTest(separator=separator):
                command = shlex.join(
                    [
                        "ssh",
                        "-i",
                        "/tmp/key",
                        "-o",
                        "UserKnownHostsFile=/tmp/known",
                        "-o",
                        "HostKeyAlias=compute.123",
                        "-o",
                        "ProxyCommand" + separator + proxy,
                        "-o",
                        "ProxyUseFdpass=no",
                        "user@compute.123",
                    ]
                )
                run = gce.Runner(config)
                with patch.dict(os.environ, self.env, clear=True), patch.object(run, "run", return_value=""):
                    path = gce.managed_ssh(config, command, run)
                result = subprocess.run(
                    ["ssh", "-F", str(path), "-G", config.alias],
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                options = dict(line.split(" ", 1) for line in result.stdout.splitlines())
                self.assertEqual(options["proxycommand"], proxy)
                self.assertEqual(options["proxyusefdpass"], "no")
                self.assertEqual(options["hostkeyalias"], "compute.123")

    def test_iap_alias_refuses_missing_or_multiline_proxy_configuration(self):
        config = replace(self.config, tunnel_through_iap=True)
        run = gce.Runner(config)
        base = ["ssh", "-i", "/tmp/key", "-o", "UserKnownHostsFile=/tmp/known"]
        with patch.dict(os.environ, self.env, clear=True), patch.object(run, "run", return_value=""):
            with self.assertRaisesRegex(gce.SetupError, "bypass IAP"):
                gce.managed_ssh(config, shlex.join([*base, "user@compute.123"]), run)
            with self.assertRaisesRegex(gce.SetupError, "unusable SSH"):
                gce.managed_ssh(
                    config, shlex.join([*base, "-o", "ProxyCommand cmd\nHost forged", "user@compute.123"]), run
                )
        self.assertFalse((self.home / ".ssh/config").exists())

    def test_symlinked_ssh_config_requires_an_explicit_existing_include(self):
        run = gce.Runner(self.config)
        command = "ssh -i /tmp/key -o UserKnownHostsFile=/tmp/known user@192.0.2.1"
        directory = self.home / ".ssh"
        directory.mkdir()
        actual = self.root / "ssh-config"
        actual.write_text("Host work\n  HostName work.example.invalid\n")
        (directory / "config").symlink_to(actual)
        with patch.dict(os.environ, self.env, clear=True), patch.object(run, "run", return_value=""):
            with self.assertRaisesRegex(gce.SetupError, "symlink"):
                gce.managed_ssh(self.config, command, run)
            body = actual.read_text()
            actual.write_text(f"Include {json.dumps(str(directory / 'branchyard/*.conf'))}\n" + body)
            before = actual.read_text()
            gce.managed_ssh(self.config, command, run)
            self.assertEqual(actual.read_text(), before)


if __name__ == "__main__":
    unittest.main()
