"""The installed toolkit must be an exact, safe copy of canonical sources."""

from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import re
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

    def test_navigation_is_relative_compact_and_contains_only_relevant_resources(self) -> None:
        value = json.loads(self.catalog.read_text())
        value["topics"] = [
            {"id": "runtime", "title": "Runtime", "description": "Find installed tools."},
            {"id": "knowledge", "title": "Knowledge", "description": "Work with evidence."},
        ]
        value["skills"][0]["topic"] = "runtime"
        value["skills"].append(
            {
                "id": "claims",
                "description": "Check evidence anchors.",
                "requires": ["artifact-tools"],
                "topic": "knowledge",
            }
        )
        self.catalog.write_text(json.dumps(value))
        claims = self.plugin / "skills/claims"
        (claims / "references").mkdir(parents=True)
        (claims / "SKILL.md").write_text("---\nname: claims\ndescription: Check anchors.\n---\n" + "LONG BODY " * 2000)
        (claims / "references/anchors.md").write_text("# Anchor reference\nPrivate instruction detail.\n")
        (self.skill / "scripts").mkdir()
        (self.skill / "scripts/run.py").write_text('print("tool")\n')
        examples = self.source / "examples"
        examples.mkdir()
        (examples / "claims.json").write_text('{"schema_version": 1}\n')
        self.module.synchronize(self.root, write=True)
        self.assertEqual(self.module.synchronize(self.root, write=False), [])
        index = (self.plugin / "skills/INDEX.md").read_text()
        self.assertIn("topics/knowledge/INDEX.md", index)
        self.assertNotIn("claims/", index)
        self.assertNotIn("LONG BODY", index)
        self.assertIn("../../claims/INDEX.md", (self.plugin / "skills/topics/knowledge/INDEX.md").read_text())
        self.assertIn("../artifact-tools/INDEX.md", (claims / "INDEX.md").read_text())
        self.assertIn("references/anchors.md", (claims / "INDEX.md").read_text())
        self.assertNotIn("Private instruction detail", (claims / "INDEX.md").read_text())
        toolkit = (self.skill / "INDEX.md").read_text()
        self.assertIn("scripts/run.py", toolkit)
        self.assertIn("scripts/branchyard_artifacts/examples/claims.json", toolkit)
        self.assertNotIn("__init__.py", toolkit)
        self.assertNotIn("SKILL.md\n", toolkit)
        root = self.plugin / "skills"
        for path in root.rglob("INDEX.md"):
            body = path.read_text()
            self.assertLessEqual(len(body.encode()), 4096 if path.parent == root else 8192)
            for link in re.findall(r"\]\(([^)]+)\)", body):
                with self.subTest(index=path, link=link):
                    self.assertFalse(Path(link).is_absolute())
                    target = (path.parent / link).resolve(strict=True)
                    self.assertTrue(target.is_relative_to(root))
                    self.assertTrue(target.is_file())
        generated = (self.root / "crates/branchyard-provision/src/skills.rs").read_text()
        self.assertIn("pub const INDEX:", generated)
        self.assertIn("pub const TOPIC_INDEXES:", generated)
        self.assertIn('("INDEX.md", include_str!', generated)

    def test_legacy_catalog_gets_a_documents_topic_and_skill_body_changes_do_not_expand_indexes(self) -> None:
        self.module.synchronize(self.root, write=True)
        index = self.plugin / "skills/INDEX.md"
        skill_index = self.skill / "INDEX.md"
        before = (index.read_bytes(), skill_index.read_bytes())
        self.assertIn("topics/documents/INDEX.md", index.read_text())
        path = self.skill / "SKILL.md"
        path.write_text(path.read_text() + "Unrelated implementation detail.\n" * 10000)
        self.module.synchronize(self.root, write=True)
        self.assertEqual((index.read_bytes(), skill_index.read_bytes()), before)

    def test_navigation_refuses_unknown_topics_duplicate_topics_and_oversized_metadata(self) -> None:
        original = json.loads(self.catalog.read_text())
        value = json.loads(self.catalog.read_text())
        value["skills"][0]["topic"] = "missing"
        self.catalog.write_text(json.dumps(value))
        with self.assertRaisesRegex(ValueError, "unknown topic"):
            self.module.synchronize(self.root, write=False)
        original["topics"] = [
            {"id": "documents", "title": "Documents", "description": "Choose."},
            {"id": "documents", "title": "Repeated", "description": "Choose."},
        ]
        self.catalog.write_text(json.dumps(original))
        with self.assertRaisesRegex(ValueError, "repeated topic"):
            self.module.synchronize(self.root, write=False)
        original["topics"] = [{"id": "../escape", "title": "Invalid", "description": "Choose."}]
        self.catalog.write_text(json.dumps(original))
        with self.assertRaisesRegex(ValueError, "invalid topic"):
            self.module.synchronize(self.root, write=False)
        original["topics"] = [{"id": "documents", "title": "Documents", "description": "Large " * 2000}]
        self.catalog.write_text(json.dumps(original))
        with self.assertRaisesRegex(ValueError, "byte budget"):
            self.module.synchronize(self.root, write=False)

    def test_stale_topic_indexes_are_removed_without_removing_unrelated_files(self) -> None:
        self.module.synchronize(self.root, write=True)
        stale = self.plugin / "skills/topics/retired/INDEX.md"
        stale.parent.mkdir()
        stale.write_text("# Generated old topic\n")
        retained = stale.parent / "notes.txt"
        retained.write_text("Unrelated content.\n")
        self.assertIn(stale.relative_to(self.root).as_posix(), self.module.synchronize(self.root, write=False))
        self.module.synchronize(self.root, write=True)
        self.assertFalse(stale.exists())
        self.assertTrue(retained.is_file())

    def test_optional_resources_are_installed_relative_files_and_links_are_not_followed(self) -> None:
        reference = self.skill / "references/guide.md"
        reference.parent.mkdir()
        reference.write_text("# Guide\n")
        value = json.loads(self.catalog.read_text())
        value["skills"][0]["resources"] = [{"path": "references/guide.md", "description": "Guide"}]
        self.catalog.write_text(json.dumps(value))
        self.module.synchronize(self.root, write=True)
        self.assertIn("[Guide](references/guide.md)", (self.skill / "INDEX.md").read_text())
        outside = self.root / "outside.md"
        outside.write_text("Not installed.\n")
        value["skills"][0]["resources"] = [{"path": os.path.relpath(outside, self.skill)}]
        self.catalog.write_text(json.dumps(value))
        with self.assertRaisesRegex(ValueError, "not an installed file"):
            self.module.synchronize(self.root, write=False)
        value["skills"][0]["resources"] = [{"path": str(reference)}]
        self.catalog.write_text(json.dumps(value))
        with self.assertRaisesRegex(ValueError, "must be a relative"):
            self.module.synchronize(self.root, write=False)
        reference.unlink()
        reference.symlink_to(outside)
        with self.assertRaisesRegex(ValueError, "symlink"):
            self.module.synchronize(self.root, write=False)

    def test_upstream_records_bind_reused_files_and_license_bytes(self) -> None:
        reference = self.skill / "references/reused.md"
        reference.parent.mkdir()
        reference.write_text("# Reused guide\n")
        license_file = reference.parent / "LICENSE.txt"
        license_file.write_text("MIT License\n")
        source = {
            "id": "reference-guide",
            "repository": "example/skills",
            "revision": "a" * 40,
            "path": "skills/example/guide.md",
            "source_sha256": "b" * 64,
            "license": "MIT",
            "license_path": "LICENSE.txt",
            "installed_path": "artifact-tools/references/reused.md",
            "installed_sha256": hashlib.sha256(reference.read_bytes()).hexdigest(),
            "installed_license_path": "artifact-tools/references/LICENSE.txt",
            "license_sha256": hashlib.sha256(license_file.read_bytes()).hexdigest(),
            "modifications": "Adapted tool invocation paths.",
        }
        lock = self.plugin / "skills/upstream.lock.json"
        lock.write_text(json.dumps({"schema_version": 1, "reviewed_at": "2026-10-29", "sources": [source]}))
        self.module.synchronize(self.root, write=True)
        self.assertIn(
            "[Upstream sources and licenses](upstream.lock.json)", (self.plugin / "skills/INDEX.md").read_text()
        )
        self.assertIn(
            "pub const UPSTREAM_LOCK: &str = include_str!",
            (self.root / "crates/branchyard-provision/src/skills.rs").read_text(),
        )
        reference.write_text("Changed guide\n")
        with self.assertRaisesRegex(ValueError, "installed_sha256 mismatch"):
            self.module.synchronize(self.root, write=False)
        reference.write_text("# Reused guide\n")
        license_file.write_text("Changed license\n")
        with self.assertRaisesRegex(ValueError, "license_sha256 mismatch"):
            self.module.synchronize(self.root, write=False)
        license_file.write_text("MIT License\n")
        for field, value in [
            ("revision", "main"),
            ("source_sha256", "unverified"),
            ("installed_sha256", "short"),
            ("license_sha256", "short"),
            ("installed_path", "../outside.md"),
            ("installed_license_path", "/tmp/license"),
            ("path", "../upstream-guide"),
            ("license", "source-available"),
        ]:
            with self.subTest(field=field):
                changed = {**source, field: value}
                lock.write_text(json.dumps({"schema_version": 1, "sources": [changed]}))
                with self.assertRaises(ValueError):
                    self.module.synchronize(self.root, write=False)
        lock.write_text(json.dumps({"schema_version": 1, "sources": [source]}))
        reference.unlink()
        reference.symlink_to(license_file)
        with self.assertRaisesRegex(ValueError, "symlink"):
            self.module.synchronize(self.root, write=True)
        reference.unlink()
        reference.write_text("# Reused guide\n")
        os.mkfifo(reference.parent / "fifo.md")
        changed = {**source, "installed_path": "artifact-tools/references/fifo.md"}
        lock.write_text(json.dumps({"schema_version": 1, "sources": [changed]}))
        with self.assertRaisesRegex(ValueError, "regular installed file"):
            self.module.synchronize(self.root, write=False)
        lock.write_text(json.dumps({"schema_version": 1, "sources": [source, source]}))
        with self.assertRaisesRegex(ValueError, "repeated upstream source"):
            self.module.synchronize(self.root, write=False)
        lock.write_text(json.dumps({"schema_version": True, "sources": [source]}))
        with self.assertRaisesRegex(ValueError, "schema_version"):
            self.module.synchronize(self.root, write=False)

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
