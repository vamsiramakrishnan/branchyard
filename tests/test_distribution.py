"""Exercise the distribution archives (`tools/package.py`) as delivered:
extracted outside the checkout, installed with the shipped installer, and
run with the shipped launcher / imported as the shipped module. See
`docs/distribution.md`.
"""

import importlib.util
import json
import os
import re
import stat
import subprocess
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

spec = importlib.util.spec_from_file_location("package", ROOT / "tools/package.py")
assert spec is not None and spec.loader is not None
package = importlib.util.module_from_spec(spec)
spec.loader.exec_module(package)


class ArchiveTests(unittest.TestCase):
    """`tools/package.py`'s archives, without touching the checkout."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def test_archives_are_reproducible_and_complete(self):
        first = package.build(self.root / "one")
        second = package.build(self.root / "two")
        self.assertEqual(len(first), 4)
        for a, b in zip(first, second, strict=True):
            self.assertEqual(a.name, b.name)
            self.assertEqual(a.read_bytes(), b.read_bytes(), f"{a.name} is not reproducible")
            with zipfile.ZipFile(a) as archive:
                root_name = archive.namelist()[0].split("/", 1)[0]
                manifest = json.loads(archive.read(f"{root_name}/MANIFEST.sha256.json"))
                self.assertEqual(
                    set(archive.namelist()),
                    {f"{root_name}/{name}" for name in manifest} | {f"{root_name}/MANIFEST.sha256.json"},
                )
                for name, digest in manifest.items():
                    import hashlib

                    self.assertEqual(hashlib.sha256(archive.read(f"{root_name}/{name}")).hexdigest(), digest)

    def test_plugin_archive_bundles_the_same_skill_bytes_as_the_standalone_one(self):
        plugin, skill, setup_skill, _sdk = package.build(self.root / "dist")
        with zipfile.ZipFile(plugin) as plugin_zip:
            for archive, source, name in [
                (skill, package.SKILL, "delegate"),
                (setup_skill, package.SETUP_SKILL, "setup"),
            ]:
                with zipfile.ZipFile(archive) as skill_zip:
                    inputs = package.inputs(source)
                    self.assertIn("SKILL.md", inputs)
                    for path, data in inputs.items():
                        self.assertEqual(plugin_zip.read(f"branchyard/skills/{name}/{path}"), data)
                        self.assertEqual(skill_zip.read(f"{name}/{path}"), data)
            # The /branchyard:setup command ships in the plugin.
            self.assertIn("branchyard/commands/setup.md", plugin_zip.namelist())

    def test_sdk_archive_matches_its_source(self):
        *_archives, sdk = package.build(self.root / "dist")
        with zipfile.ZipFile(sdk) as sdk_zip:
            for path, data in package.inputs(package.SDK).items():
                self.assertEqual(sdk_zip.read(f"branchyard-sdk/{path}"), data)

    def test_manifests_must_match_the_workspace_version(self):
        original = (package.PLUGIN / ".codex-plugin/plugin.json").read_text()
        try:
            broken = json.loads(original)
            broken["version"] = "9.9.9"
            (package.PLUGIN / ".codex-plugin/plugin.json").write_text(json.dumps(broken))
            with self.assertRaises(ValueError):
                package.build(self.root / "broken")
        finally:
            (package.PLUGIN / ".codex-plugin/plugin.json").write_text(original)


class InstallerTests(unittest.TestCase):
    """The installer, run on a plugin archive extracted outside the checkout."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        plugin, _skill, _setup_skill, _sdk = package.build(self.root / "dist")
        with zipfile.ZipFile(plugin) as archive:
            archive.extractall(self.root / "extracted")
        self.plugin = self.root / "extracted/branchyard"
        self.installer = self.plugin / "scripts/install_skill.py"
        self.host = self.root / "host"

    def run_installer(self, *extra):
        return subprocess.run(
            [sys.executable, str(self.installer), "--destination", str(self.host), *extra],
            capture_output=True,
            text=True,
        )

    def test_preview_by_default_then_applies_then_refuses_a_different_install(self):
        preview = self.run_installer()
        self.assertEqual(preview.returncode, 0, preview.stderr)
        self.assertFalse(json.loads(preview.stdout)["applied"])
        self.assertFalse(self.host.exists())

        applied = self.run_installer("--apply")
        self.assertEqual(applied.returncode, 0, applied.stderr)
        installed = self.host / "delegate"
        self.assertEqual(
            (installed / "SKILL.md").read_bytes(),
            (package.SKILL / "SKILL.md").read_bytes(),
        )

        # An identical re-install is a no-op, not a refusal.
        again = self.run_installer("--apply")
        self.assertEqual(again.returncode, 0, again.stderr)
        self.assertTrue(json.loads(again.stdout)["unchanged"])

        # A changed destination is refused without --force ...
        (installed / "local-notes.md").write_text("do not delete me")
        refused = self.run_installer("--apply")
        self.assertEqual(refused.returncode, 2)
        self.assertEqual(json.loads(refused.stderr)["error"], "destination_exists")
        self.assertTrue((installed / "local-notes.md").exists())

        # ... and replaced with it.
        forced = self.run_installer("--apply", "--force")
        self.assertEqual(forced.returncode, 0, forced.stderr)
        self.assertTrue(json.loads(forced.stdout)["replaced"])
        self.assertFalse((installed / "local-notes.md").exists())
        self.assertEqual(
            (installed / "SKILL.md").read_bytes(),
            (package.SKILL / "SKILL.md").read_bytes(),
        )

    def test_installs_the_setup_skill_by_name(self):
        applied = self.run_installer("--skill", "setup", "--apply")
        self.assertEqual(applied.returncode, 0, applied.stderr)
        installed = self.host / "setup"
        for path, data in package.inputs(package.SETUP_SKILL).items():
            self.assertEqual((installed / path).read_bytes(), data)
        self.assertFalse((self.host / "delegate").exists())
        unknown = self.run_installer("--skill", "nope")
        self.assertEqual(unknown.returncode, 2)

    def test_refuses_a_non_directory_destination(self):
        self.host.mkdir(parents=True)
        (self.host / "delegate").write_text("not a skill")
        refused = self.run_installer("--apply")
        self.assertEqual(refused.returncode, 2)
        self.assertEqual(json.loads(refused.stderr)["error"], "destination_exists")


class SdkArchiveRuntimeTests(unittest.TestCase):
    """The Python SDK archive, extracted outside the checkout and run
    against a stub `by`."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        *_archives, sdk = package.build(self.root / "dist")
        with zipfile.ZipFile(sdk) as archive:
            archive.extractall(self.root / "extracted")
        self.module_path = self.root / "extracted/branchyard-sdk/branchyard.py"

    def _stub_by(self, reply: dict) -> Path:
        stub = self.root / "by"
        stub.write_text(f"#!/usr/bin/env python3\nimport json, sys\nprint(json.dumps({reply!r}))\n")
        stub.chmod(stub.stat().st_mode | stat.S_IEXEC)
        return stub

    def test_extracted_module_is_importable_and_matches_its_source(self):
        self.assertEqual(self.module_path.read_bytes(), (package.SDK / "branchyard.py").read_bytes())
        spec = importlib.util.spec_from_file_location("branchyard_extracted", self.module_path)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        stub = self._stub_by(
            {
                "name": "demo",
                "status": {"state": "running"},
                "harness": "codex",
                "profile": "codex-app-server",
                "parent": None,
                "children": [],
                "depth": 0,
                "turns": 1,
                "candidate": None,
                "cost_usd": None,
                "subtree_cost_usd": 0.0,
                "max_usd": None,
                "remaining_usd": None,
                "envelope": None,
                "last_message": "",
            }
        )
        env = dict(os.environ)
        env["BRANCHYARD_BY"] = str(stub)
        code = (
            "import os, sys\n"
            f"sys.path.insert(0, {str(self.module_path.parent)!r})\n"
            "import branchyard\n"
            "result = branchyard.inspect()\n"
            "print(result.name, result.running)\n"
        )
        result = subprocess.run([sys.executable, "-c", code], env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "demo True")


class ReleaseWorkflowTests(unittest.TestCase):
    """`.github/workflows/release.yml`: prebuilt binaries, checksums and
    provenance, only for a v* tag. Never run here; checked as text (and as
    YAML when PyYAML is installed)."""

    def setUp(self):
        self.text = (ROOT / ".github/workflows/release.yml").read_text()

    def test_it_runs_only_for_a_v_tag_push(self):
        head = self.text.split("\npermissions:", 1)[0]
        self.assertTrue(head.endswith("\non:\n  push:\n    tags: ['v*']"), head)
        for trigger in ("pull_request", "workflow_dispatch", "schedule", "branches"):
            self.assertNotIn(trigger, head)
        try:
            import yaml
        except ImportError:
            return
        parsed = yaml.safe_load(self.text)
        # PyYAML reads the key `on` as True.
        self.assertEqual(parsed[True], {"push": {"tags": ["v*"]}})
        self.assertEqual(set(parsed["jobs"]), {"build", "release", "image"})

    def test_every_action_is_pinned_to_a_commit(self):
        uses = re.findall(r"uses:\s*(\S+)", self.text)
        self.assertTrue(uses)
        for use in uses:
            self.assertRegex(use, r"^[\w.-]+/[\w.-]+@[0-9a-f]{40}$")

    def test_it_builds_four_targets_with_checksums_and_attestations(self):
        for target in (
            "x86_64-unknown-linux-musl",
            "aarch64-unknown-linux-musl",
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
        ):
            self.assertIn(f"target: {target}", self.text)
        self.assertIn("-p branchyard-cli -p branchyard-server", self.text)
        self.assertIn("statically linked", self.text)
        self.assertIn("shasum -a 256", self.text)
        self.assertIn("SHA256SUMS", self.text)
        self.assertEqual(self.text.count("actions/attest-build-provenance@"), 2)
        self.assertIn("id-token: write", self.text)
        self.assertIn("attestations: write", self.text)
        self.assertIn("--draft --verify-tag", self.text)
        self.assertIn("docker build -f deploy/Dockerfile.harnesses", self.text)
        self.assertIn("if: vars.PUBLISH_IMAGES == 'true'", self.text)


def _fake_release(directory, version, target, by_text="#!/bin/sh\necho 'by 9.9.9'\n"):
    name = f"branchyard-{version}-{target}"
    stage = directory / "stage" / name
    (stage / "licenses").mkdir(parents=True)
    for binary, text in (("by", by_text), ("branchyard-server", "#!/bin/sh\nexit 0\n")):
        (stage / binary).write_text(text)
        (stage / binary).chmod(0o755)
    for extra in ("LICENSE", "THIRD_PARTY.md", "README.md"):
        (stage / extra).write_text(extra)
    (stage / "licenses" / "orca-LICENSE").write_text("MIT")
    release = directory / "release"
    release.mkdir(exist_ok=True)
    archive = release / f"{name}.tar.gz"
    subprocess.run(["tar", "-C", str(stage.parent), "-czf", str(archive), name], check=True)
    import hashlib

    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    (release / "SHA256SUMS").write_text(f"{digest}  {archive.name}\n")
    return release


class InstallScriptTests(unittest.TestCase):
    """`install.sh` against a local file:// "release", in a temporary prefix."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.target = "x86_64-unknown-linux-musl"
        self.release = _fake_release(self.root, "1.2.3", self.target)
        self.prefix = self.root / "prefix"

    def run_install(self, *args):
        return subprocess.run(
            ["sh", str(ROOT / "install.sh"), "--target", self.target, *args],
            capture_output=True,
            text=True,
            env={"PATH": os.environ["PATH"], "HOME": str(self.root)},
        )

    def test_it_is_posix_sh(self):
        self.assertEqual(subprocess.run(["sh", "-n", str(ROOT / "install.sh")]).returncode, 0)
        text = (ROOT / "install.sh").read_text()
        self.assertTrue(text.startswith("#!/bin/sh\n"))
        self.assertNotIn("sudo ", text.replace("never calls sudo", ""))
        self.assertNotIn("[[", text)

    def test_installs_after_verifying_the_checksum(self):
        out = self.run_install(
            "--version", "v1.2.3", "--prefix", str(self.prefix), "--base-url", f"file://{self.release}"
        )
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertIn("installed by 9.9.9 into", out.stdout)
        for binary in ("by", "branchyard-server"):
            path = self.prefix / "bin" / binary
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o755)
        self.assertTrue((self.prefix / "share/branchyard/licenses/orca-LICENSE").is_file())
        # A plain directory works too, and installing again replaces.
        out = self.run_install("--version", "1.2.3", "--prefix", str(self.prefix), "--base-url", str(self.release))
        self.assertEqual(out.returncode, 0, out.stderr)

    def test_a_tampered_archive_installs_nothing(self):
        sums = self.release / "SHA256SUMS"
        sums.write_text("0" * 64 + sums.read_text()[64:])
        out = self.run_install(
            "--version", "1.2.3", "--prefix", str(self.prefix), "--base-url", f"file://{self.release}"
        )
        self.assertNotEqual(out.returncode, 0)
        self.assertIn("checksum mismatch", out.stderr)
        self.assertFalse((self.prefix / "bin" / "by").exists())

    def test_it_refuses_what_it_cannot_do_safely(self):
        out = self.run_install("--prefix", str(self.prefix))
        self.assertIn("--version", out.stderr)
        out = self.run_install("--version", "1.2.3", "--base-url", "http://example.invalid/r")
        self.assertIn("refusing plain http", out.stderr)
        out = self.run_install(
            "--version", "1.2.3", "--prefix", str(self.prefix), "--base-url", f"file://{self.root}/nowhere"
        )
        self.assertNotEqual(out.returncode, 0)
        self.assertFalse(self.prefix.exists())
        out = self.run_install("--version", "1.2.3", "--dry-run")
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertIn("would install branchyard-1.2.3-x86_64-unknown-linux-musl.tar.gz", out.stdout)


class HomebrewFormulaTests(unittest.TestCase):
    def setUp(self):
        spec = importlib.util.spec_from_file_location("homebrew_formula", ROOT / "tools/homebrew_formula.py")
        self.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.module)

    def test_the_template_fills_from_sha256sums(self):
        sums = "".join(f"{str(i) * 64}  branchyard-1.2.3-{t}.tar.gz\n" for i, t in enumerate(self.module.TARGETS))
        formula = self.module.render("1.2.3", sums)
        self.assertNotRegex(formula, r"@[A-Z0-9_]+@")
        self.assertIn('version "1.2.3"', formula)
        self.assertIn("releases/download/v1.2.3", formula)
        self.assertIn('sha256 "' + "0" * 64 + '"', formula)
        with self.assertRaisesRegex(ValueError, "lists no"):
            self.module.render("1.2.3", sums.splitlines()[0])


class HarnessImageTests(unittest.TestCase):
    """`deploy/Dockerfile.harnesses`, statically: no container runtime here."""

    def setUp(self):
        self.text = (ROOT / "deploy/Dockerfile.harnesses").read_text()

    def test_the_build_stage_is_deploy_dockerfiles(self):
        def stage(text):
            start = text.index("FROM rust:")
            return text[start : text.index("\nFROM ", start + 1)]

        self.assertEqual(stage(self.text), stage((ROOT / "deploy/Dockerfile").read_text()))

    def test_runtime_images_are_pinned_by_digest_and_run_as_a_user(self):
        froms = re.findall(r"^FROM (\S+)", self.text, re.M)
        self.assertEqual(len(froms), 3)
        for image in froms[1:]:
            self.assertRegex(image, r"^node:22[\w.-]*@sha256:[0-9a-f]{64}$")
        self.assertRegex(self.text, r"\nUSER branchyard\n")
        self.assertIn("openssh-client", self.text)
        self.assertIn(" git ", self.text)
        self.assertIn("npm ci --omit=dev", self.text)
        self.assertNotIn("npm install", self.text)
        self.assertNotRegex(self.text, r"(?i)ENV [^\n]*(KEY|TOKEN|SECRET)")

    def test_harnesses_are_the_qualified_pins_with_integrity(self):
        manifest = json.loads((ROOT / "deploy/harnesses/package.json").read_text())
        lock = json.loads((ROOT / "deploy/harnesses/package-lock.json").read_text())
        qualify = (ROOT / ".github/workflows/qualify.yml").read_text()
        for package, version in manifest["dependencies"].items():
            self.assertRegex(version, r"^\d+\.\d+\.\d+$", "exact pins only")
            self.assertIn(f"{package}@{version}", qualify)
            self.assertEqual(lock["packages"][f"node_modules/{package}"]["version"], version)
        self.assertEqual(lock["packages"][""]["dependencies"], manifest["dependencies"])
        for path, entry in lock["packages"].items():
            if not path:
                continue
            self.assertRegex(entry.get("integrity", ""), r"^sha512-", path)
            self.assertTrue(entry["resolved"].startswith("https://registry.npmjs.org/"), path)


if __name__ == "__main__":
    unittest.main()
