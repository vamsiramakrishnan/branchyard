"""Exercise the distribution archives (`tools/package.py`) as delivered:
extracted outside the checkout, installed with the shipped installer, and
run with the shipped launcher / imported as the shipped module. See
`docs/distribution.md`.
"""
import importlib.util
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[1]

spec = importlib.util.spec_from_file_location("package", ROOT / "tools/package.py")
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
        self.assertEqual(len(first), 3)
        for a, b in zip(first, second):
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
        plugin, skill, _sdk = package.build(self.root / "dist")
        with zipfile.ZipFile(plugin) as plugin_zip, zipfile.ZipFile(skill) as skill_zip:
            for path, data in package.inputs(package.SKILL).items():
                self.assertEqual(plugin_zip.read(f"branchyard/skills/delegate/{path}"), data)
                self.assertEqual(skill_zip.read(f"delegate/{path}"), data)

    def test_sdk_archive_matches_its_source(self):
        _plugin, _skill, sdk = package.build(self.root / "dist")
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
        plugin, _skill, _sdk = package.build(self.root / "dist")
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
        _plugin, _skill, sdk = package.build(self.root / "dist")
        with zipfile.ZipFile(sdk) as archive:
            archive.extractall(self.root / "extracted")
        self.module_path = self.root / "extracted/branchyard-sdk/branchyard.py"

    def _stub_by(self, reply: dict) -> Path:
        stub = self.root / "by"
        stub.write_text(
            "#!/usr/bin/env python3\n"
            "import json, sys\n"
            f"print(json.dumps({reply!r}))\n"
        )
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


if __name__ == "__main__":
    unittest.main()
