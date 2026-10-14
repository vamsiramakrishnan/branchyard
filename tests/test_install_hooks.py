#!/usr/bin/env python3
"""tools/install_hooks.py in scratch repositories: install, idempotence, uninstall, a foreign
core.hooksPath left alone, and the committed hook running check.py's verdict. No cargo."""

import ast
import importlib.util
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("install_hooks", ROOT / "tools" / "install_hooks.py")
assert spec is not None and spec.loader is not None
install_hooks = importlib.util.module_from_spec(spec)
spec.loader.exec_module(install_hooks)


def git(root, *args):
    return subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True)


class Scratch(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root)
        self.assertEqual(git(self.root, "init", "-q").returncode, 0)

    def hooks_path(self):
        return git(self.root, "config", "--local", "--get", "core.hooksPath").stdout.strip()


class Install(Scratch):
    def test_it_writes_the_hook_and_points_git_at_it(self):
        self.assertEqual(install_hooks.install(self.root), "installed")
        hook = self.root / ".githooks" / "pre-push"
        self.assertEqual(hook.read_text(), install_hooks.HOOK)
        self.assertTrue(os.access(hook, os.X_OK))
        self.assertEqual(self.hooks_path(), ".githooks")

    def test_running_it_again_changes_nothing(self):
        install_hooks.install(self.root)
        before = (self.root / ".githooks" / "pre-push").stat().st_mtime_ns
        self.assertEqual(install_hooks.install(self.root), "already installed")
        self.assertEqual((self.root / ".githooks" / "pre-push").stat().st_mtime_ns, before)

    def test_outside_a_repository_it_fails_and_writes_nothing(self):
        plain = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, plain)
        with self.assertRaises(SystemExit):
            install_hooks.install(plain)
        self.assertFalse((plain / ".githooks").exists())

    def test_a_foreign_hooks_path_is_refused_and_left_alone(self):
        git(self.root, "config", "core.hooksPath", "elsewhere")
        with self.assertRaises(SystemExit):
            install_hooks.install(self.root)
        self.assertEqual(self.hooks_path(), "elsewhere")
        self.assertFalse((self.root / ".githooks").exists())

    def test_uninstall_unsets_the_path_and_keeps_the_committed_hook(self):
        install_hooks.install(self.root)
        self.assertEqual(install_hooks.uninstall(self.root), "uninstalled")
        self.assertEqual(self.hooks_path(), "")
        self.assertTrue((self.root / ".githooks" / "pre-push").is_file())
        self.assertEqual(install_hooks.uninstall(self.root), "not installed")


class Hook(Scratch):
    def run_hook(self, verdict):
        (self.root / "tools").mkdir()
        (self.root / "tools" / "check.py").write_text(f"import sys\nsys.exit({verdict})\n")
        (self.root / ".githooks").mkdir()
        (self.root / ".githooks" / "pre-push").write_text(install_hooks.HOOK)
        return subprocess.run(
            ["sh", ".githooks/pre-push"], cwd=self.root, stdin=subprocess.DEVNULL, capture_output=True, text=True
        )

    def test_a_passing_check_lets_the_push_through(self):
        self.assertEqual(self.run_hook(0).returncode, 0)

    def test_a_failing_check_refuses_the_push_and_names_the_bypass(self):
        done = self.run_hook(1)
        self.assertEqual(done.returncode, 1)
        self.assertIn("git push --no-verify", done.stderr)


class RealTree(unittest.TestCase):
    def test_the_committed_hook_is_the_installed_one(self):
        hook = ROOT / ".githooks" / "pre-push"
        self.assertEqual(hook.read_text(), install_hooks.HOOK)
        self.assertTrue(os.access(hook, os.X_OK))

    def test_it_parses_as_python_39(self):
        ast.parse((ROOT / "tools" / "install_hooks.py").read_text(), feature_version=(3, 9))


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
