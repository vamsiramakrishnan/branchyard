#!/usr/bin/env python3
"""Run the shipped image bootstrap with fake privilege-changing tools and a fake bridge."""

import json
import os
import shlex
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "deploy/substrate-entrypoint.sh"
BRIDGE = "/usr/local/libexec/branchyard-bridge"

FAKE = r"""
import json, os, subprocess, sys
from pathlib import Path
name = Path(sys.argv[0]).name
args = sys.argv[1:]
with Path(os.environ["FAKE_LOG"]).open("a") as handle:
    handle.write(json.dumps([name, args]) + "\n")
if name == "id":
    print(os.environ.get("FAKE_UID", "0"))
elif name == "mkdir":
    if os.environ.get("FAIL_MKDIR"):
        sys.exit(1)
    paths = args[args.index("--") + 1:]
    root = Path(os.environ["FAKE_ROOT"])
    if any(not Path(path).is_relative_to(root) for path in paths):
        raise SystemExit("test attempted to write outside its temporary directory")
    # Exercise native directory creation/modes without requiring a privileged test runner.
    subprocess.run(["/usr/bin/mkdir", "-p", "--", *paths], check=True)
elif name == "chmod" and args[0] == "0700":
    for path in args[2:]:
        Path(path).chmod(0o700)
elif name == "bridge":
    print("fake bridge")
"""


class SubstrateImage(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.log = self.root / "calls.jsonl"
        fake = self.bin / "fake"
        fake.write_text(f"#!{sys.executable}\n" + FAKE)
        fake.chmod(0o755)
        for name in ("id", "mkdir", "chown", "chmod", "bridge"):
            (self.bin / name).symlink_to(fake)
        source = SCRIPT.read_text()
        self.assertEqual(source.count(BRIDGE), 2)
        # Remap the image's fixed dependency path; the startup logic is unchanged.
        self.script = self.root / "entrypoint.sh"
        self.script.write_text(source.replace(BRIDGE, shlex.quote(str(self.bin / "bridge"))))
        self.workdir = self.root / "workspace"
        self.home = self.root / "home"
        self.env = dict(
            os.environ,
            PATH=f"{self.bin}:/usr/bin:/bin",
            FAKE_LOG=str(self.log),
            FAKE_ROOT=str(self.root),
            BRANCHYARD_ACTOR_UID="1000",
            BRANCHYARD_ACTOR_GID="1000",
            BRANCHYARD_ACTOR_WORKDIR=str(self.workdir),
            BRANCHYARD_ACTOR_HOME=str(self.home),
        )

    def run_script(self, *arguments):
        return subprocess.run(
            ["/bin/sh", str(self.script), *arguments],
            env=self.env,
            text=True,
            capture_output=True,
            check=False,
            timeout=10,
        )

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def test_bootstrap_repairs_only_root_and_declared_directories_then_execs(self):
        arguments = ("serve", "--run-as", "1000:1000", "--listen", "0.0.0.0:80")
        for _ in range(2):
            result = self.run_script(*arguments)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(self.workdir.stat().st_mode & 0o777, 0o700)
            self.assertEqual(self.home.stat().st_mode & 0o777, 0o700)
        mutations = [(name, args) for name, args in self.calls() if name in ("chmod", "mkdir", "chown")]
        paths = [str(self.workdir), str(self.home)]
        expected = [
            ("chmod", ["0755", "/"]),
            ("mkdir", ["-p", "--", *paths]),
            ("chown", ["0:0", "--", *paths]),
            ("chmod", ["0700", "--", *paths]),
            ("chown", ["1000:1000", "--", *paths]),
        ]
        self.assertEqual(mutations, expected * 2)
        self.assertEqual(self.calls()[-1], ["bridge", list(arguments)])

    def test_configured_identity_must_match_the_template(self):
        for arguments in (
            ("serve",),
            ("serve", "--run-as"),
            ("serve", "--run-as", "0:0"),
            ("serve", "--run-as=1001:1000"),
            ("serve", "--run-as", "1000:1000", "--run-as=1000:1000"),
            ("serve", "--run-as=", "--run-as=1000:1000"),
        ):
            with self.subTest(arguments=arguments):
                self.assertNotEqual(self.run_script(*arguments).returncode, 0)
                self.assertEqual(self.calls(), [])
        self.env["BRANCHYARD_ACTOR_UID"] = "1234"
        self.env["BRANCHYARD_ACTOR_GID"] = "2345"
        self.assertEqual(self.run_script("serve", "--run-as=1234:2345").returncode, 0)
        self.assertIn(["chown", ["1234:2345", "--", str(self.workdir), str(self.home)]], self.calls())

    def test_paths_cannot_redirect_the_privileged_preparation(self):
        outside = self.root / "outside"
        outside.mkdir(mode=0o755)
        (self.root / "link").symlink_to(outside, target_is_directory=True)
        for path in (
            "/",
            "relative",
            "/run/branchyard/identity",
            "/run",
            "/var",
            "/var/lib",
            "/var/lib/branchyard-bridge/attempts",
            str(self.root / "link"),
            str(self.root / "link/child"),
            str(self.home),
            str(self.home / "child"),
            str(self.root) + "/x/../workspace",
        ):
            with self.subTest(path=path):
                self.env["BRANCHYARD_ACTOR_WORKDIR"] = path
                result = self.run_script("serve", "--run-as", "1000:1000")
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(any(name in ("chmod", "mkdir", "chown", "bridge") for name, _ in self.calls()))
        self.assertEqual(outside.stat().st_mode & 0o777, 0o755)

    def test_root_identity_and_coreutils_failures_stop_before_serving(self):
        self.env["FAKE_UID"] = "1000"
        self.assertNotEqual(self.run_script("serve", "--run-as=1000:1000").returncode, 0)
        self.assertFalse(any(name == "bridge" for name, _ in self.calls()))
        self.env["FAKE_UID"] = "0"
        self.env["FAIL_MKDIR"] = "1"
        self.assertNotEqual(self.run_script("serve", "--run-as=1000:1000").returncode, 0)
        self.assertFalse(any(name == "bridge" for name, _ in self.calls()))

    def test_version_and_keygen_do_not_prepare_directories(self):
        for arguments in (("--version",), ("keygen", "--out", "signer")):
            self.assertEqual(self.run_script(*arguments).returncode, 0)
        self.assertEqual([name for name, _ in self.calls()], ["bridge", "bridge"])

    def test_invalid_uids_fail_before_any_mutation(self):
        for uid in ("0", "-1", "01", "nobody", "4294967295", "99999999999999999999999"):
            with self.subTest(uid=uid):
                self.env["BRANCHYARD_ACTOR_UID"] = uid
                self.assertNotEqual(self.run_script("serve", "--run-as", f"{uid}:1000").returncode, 0)
                self.assertEqual(self.calls(), [])

    def test_recipe_probes_both_binaries_as_the_configured_user(self):
        recipe = (ROOT / "deploy/Dockerfile.substrate").read_text()
        probe = recipe.index('RUN /usr/local/libexec/branchyard-bridge --version && "$HARNESS_BINARY" --version')
        self.assertLess(recipe.index("USER ${ACTOR_UID}:${ACTOR_GID}"), probe)
        self.assertGreater(recipe.index("USER 0:0"), probe)
        self.assertIn("COPY --chmod=755 ${BRIDGE_BINARY} /usr/local/libexec/branchyard-bridge", recipe)
        self.assertIn("COPY --chmod=755 substrate-entrypoint.sh /usr/local/bin/branchyard-bridge", recipe)
        self.assertIn("ARG HARNESS_BINARY=agy", recipe)
        self.assertNotIn("gemini", recipe.lower())


if __name__ == "__main__":
    unittest.main()
