"""The installed toolkit must be an exact, safe copy of canonical sources."""

from __future__ import annotations

import importlib.util
import json
import os
import shutil
import tempfile
import unittest
from pathlib import Path
from types import ModuleType


def load_generator() -> ModuleType:
    """Load the real generator without requiring tools to be a package."""
    path = Path(__file__).resolve().parents[1] / "tools/artifact_assets.py"
    spec = importlib.util.spec_from_file_location("artifact_assets", path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class ArtifactAssetTests(unittest.TestCase):
    """Validate copied bytes, removal, dependency closure and link refusal."""

    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.module = load_generator()
        self.source = self.root / "sdk/python/branchyard_artifacts"
        self.source.mkdir(parents=True)
        (self.source / "__init__.py").write_text('"""Canonical toolkit."""\n')
        self.plugin = self.root / "plugins/branchyard"
        self.skill = self.plugin / "skills/artifact-tools"
        self.skill.mkdir(parents=True)
        (self.skill / "SKILL.md").write_text("---\nname: stale\ndescription: stale\n---\nKeep this body.\n")
        self.catalog = self.plugin / "capabilities.json"
        self.catalog.write_text(
            json.dumps(
                {"schema_version": 1, "skills": [{"id": "artifact-tools", "description": "Run tools.", "requires": []}]}
            )
        )

    def test_changed_canonical_bytes_reach_mirror_and_check_detects_drift(self) -> None:
        self.assertTrue(self.module.synchronize(self.root, write=True))
        self.assertEqual(self.module.synchronize(self.root, write=False), [])
        source = self.source / "__init__.py"
        source.write_text('"""Updated canonical toolkit."""\n')
        stale = self.module.synchronize(self.root, write=False)
        target = self.skill / "scripts/branchyard_artifacts/__init__.py"
        self.assertIn(target.relative_to(self.root).as_posix(), stale)
        self.assertNotEqual(source.read_bytes(), target.read_bytes())
        self.module.synchronize(self.root, write=True)
        self.assertEqual(source.read_bytes(), target.read_bytes())
        generated = self.root / "crates/branchyard-provision/src/skills.rs"
        self.assertIn("scripts/branchyard_artifacts/__init__.py", generated.read_text())
        self.assertIn("Keep this body.", (self.skill / "SKILL.md").read_text())

    def test_removed_source_is_removed_from_generated_install(self) -> None:
        extra = self.source / "removed.py"
        extra.write_text("VALUE = 1\n")
        self.module.synchronize(self.root, write=True)
        extra.unlink()
        target = self.skill / "scripts/branchyard_artifacts/removed.py"
        self.assertTrue(target.is_file())
        self.module.synchronize(self.root, write=True)
        self.assertFalse(target.exists())

    def test_pinned_runtime_lock_is_in_the_installed_payload(self) -> None:
        lock = self.root / "deploy/artifacts-requirements.lock"
        lock.parent.mkdir()
        lock.write_text("example==1 --hash=sha256:abc\n")
        self.module.synchronize(self.root, write=True)
        installed = self.skill / "scripts/requirements.lock"
        self.assertEqual(lock.read_bytes(), installed.read_bytes())
        self.assertIn(
            "scripts/requirements.lock", (self.root / "crates/branchyard-provision/src/skills.rs").read_text()
        )

    def test_symlink_source_is_never_exported(self) -> None:
        secret = self.root / "not-an-asset.txt"
        secret.write_text("private-data")
        (self.source / "linked.py").symlink_to(secret)
        with self.assertRaisesRegex(ValueError, "symlink"):
            self.module.synchronize(self.root, write=True)

    def test_linked_source_ancestor_is_rejected_before_generated_writes(self) -> None:
        external = self.root / "external"
        shutil.move(str(self.root / "sdk/python"), external)
        (self.root / "sdk/python").symlink_to(external, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "symlink"):
            self.module.synchronize(self.root, write=True)
        self.assertFalse((self.skill / "scripts").exists())
        self.assertFalse((self.root / "crates").exists())

    def test_special_source_files_are_rejected_without_opening_them(self) -> None:
        os.mkfifo(self.source / "fifo.py")
        with self.assertRaisesRegex(ValueError, "not a regular"):
            self.module.synchronize(self.root, write=True)
        self.assertFalse((self.skill / "scripts").exists())

    def test_trusted_outer_repository_link_is_allowed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            linked = Path(temporary) / "repository"
            linked.symlink_to(self.root, target_is_directory=True)
            self.module.synchronize(linked, write=True)
        self.assertEqual(self.module.synchronize(self.root, write=False), [])

    def test_generated_destination_link_is_not_followed(self) -> None:
        target = self.skill / "scripts/branchyard_artifacts"
        target.mkdir(parents=True)
        private = self.root / "private.txt"
        private.write_text("untouched")
        (target / "__init__.py").symlink_to(private)
        with self.assertRaisesRegex(ValueError, "symlink"):
            self.module.synchronize(self.root, write=True)
        self.assertEqual(private.read_text(), "untouched")

    def test_generated_directory_link_is_not_followed(self) -> None:
        outside = self.root / "outside"
        outside.mkdir()
        (self.skill / "scripts").symlink_to(outside, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "symlink"):
            self.module.synchronize(self.root, write=True)
        self.assertEqual(list(outside.iterdir()), [])

    def test_dependency_cycles_and_missing_packages_fail(self) -> None:
        self.catalog.write_text(
            json.dumps(
                {"skills": [{"id": "artifact-tools", "description": "Run tools.", "requires": ["artifact-tools"]}]}
            )
        )
        with self.assertRaisesRegex(ValueError, "cyclic"):
            self.module.synchronize(self.root, write=False)
        (self.source / "__init__.py").unlink()
        with self.assertRaisesRegex(ValueError, "missing"):
            self.module.synchronize(self.root, write=True)


if __name__ == "__main__":
    unittest.main()
