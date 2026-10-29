"""Explicit runtime setup uses hashed pip installs; doctor never bootstraps."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import tempfile
import unittest
from pathlib import Path
from types import ModuleType
from unittest import mock


def launcher() -> ModuleType:
    """Load the shipped launcher without executing it."""
    path = Path(__file__).resolve().parents[1] / "plugins/branchyard/skills/artifact-tools/scripts/run.py"
    spec = importlib.util.spec_from_file_location("artifact_launcher", path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class BootstrapTests(unittest.TestCase):
    """Exercise successful, interrupted and ordinary invocations without networking."""

    def test_setup_pins_hashes_and_marks_ready_only_after_pip_check(self) -> None:
        module = launcher()
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "runtime"

            def create(path: Path) -> None:
                path.mkdir()

            with (
                mock.patch.object(module.venv, "EnvBuilder") as builder,
                mock.patch.object(module.subprocess, "run") as run,
            ):
                builder.return_value.create.side_effect = create
                run.return_value.returncode = 0
                with contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(module.setup(target), 0)
                self.assertTrue((target / ".ready").is_file())
                self.assertEqual(json.loads((target / ".ready").read_text()), module.receipt())
                builder.assert_called_once_with(with_pip=True, clear=True)
                self.assertIn("--require-hashes", run.call_args_list[0].args[0])
                self.assertEqual(run.call_args_list[1].args[0][-2:], ["pip", "check"])

    def test_failed_install_never_selects_partial_runtime(self) -> None:
        module = launcher()
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory)
            (target / ".ready").touch()
            with mock.patch.object(module.venv, "EnvBuilder"), mock.patch.object(module.subprocess, "run") as run:
                run.return_value.returncode = 7
                self.assertEqual(module.setup(target), 7)
            self.assertFalse((target / ".ready").exists())

    def test_failed_pip_check_never_publishes_a_ready_receipt(self) -> None:
        module = launcher()
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory)
            with mock.patch.object(module.venv, "EnvBuilder"), mock.patch.object(module.subprocess, "run") as run:
                run.side_effect = [mock.Mock(returncode=0), mock.Mock(returncode=8)]
                self.assertEqual(module.setup(target), 8)
            self.assertFalse((target / ".ready").exists())

    def test_receipt_requires_current_lock_and_python_but_allows_venv_interpreter_links(self) -> None:
        module = launcher()
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory)
            (target / "bin").mkdir()
            (target / "bin/python").symlink_to(module.sys.executable)
            marker = target / ".ready"
            for value in ["", "pinned Python dependencies installed", "{}"]:
                marker.write_text(value)
                self.assertFalse(module.ready(target))
            for key, replacement in [("requirements_sha256", "stale"), ("python_version", [0, 0])]:
                record = module.receipt()
                record[key] = replacement
                marker.write_text(json.dumps(record))
                self.assertFalse(module.ready(target))
            marker.write_text(json.dumps(module.receipt()))
            self.assertTrue(module.ready(target))

    def test_setup_rejects_linked_cache_receipt_and_lock_without_touching_their_targets(self) -> None:
        module = launcher()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            outside = root / "outside"
            outside.mkdir()
            with mock.patch.object(module.venv, "EnvBuilder") as builder:
                (root / "cache").symlink_to(outside, target_is_directory=True)
                with self.assertRaisesRegex(ValueError, "symbolic links"):
                    module.setup(root / "cache/runtime")
                target = root / "runtime"
                target.mkdir()
                victim = outside / "victim"
                victim.write_text("unchanged")
                (target / ".ready").symlink_to(victim)
                with self.assertRaisesRegex(ValueError, "symbolic link"):
                    module.setup(target)
                (target / ".ready").unlink()
                (root / "runtime.lock").unlink()
                (root / "runtime.lock").symlink_to(victim)
                with self.assertRaisesRegex(ValueError, "symbolic link"):
                    module.setup(target)
                builder.assert_not_called()
                self.assertEqual(victim.read_text(), "unchanged")

    def test_runtime_accepts_a_linked_trusted_home_prefix(self) -> None:
        module = launcher()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            home = root / "home"
            home.mkdir()
            linked = root / "linked"
            linked.symlink_to(home, target_is_directory=True)
            with mock.patch.object(module.Path, "home", return_value=linked):
                target = module.runtime()
            self.assertTrue(target.is_relative_to(home))
            module.plain_directory(target)

    def test_doctor_does_not_install_dependencies(self) -> None:
        module = launcher()
        with (
            tempfile.TemporaryDirectory() as directory,
            mock.patch.object(module, "runtime", return_value=Path(directory)),
            mock.patch.object(module, "setup") as setup,
            mock.patch.object(module.sys, "argv", ["run.py", "doctor"]),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            self.assertEqual(module.main(), 0)
            setup.assert_not_called()

    def test_ready_runtime_reexecutes_with_the_original_arguments(self) -> None:
        module = launcher()
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory)
            (target / "bin").mkdir()
            (target / "bin/python").touch()
            (target / ".ready").write_text(json.dumps(module.receipt()))
            with (
                mock.patch.object(module, "runtime", return_value=target),
                mock.patch.object(module.os, "execv", side_effect=SystemExit(9)) as execute,
                mock.patch.object(module.sys, "argv", ["run.py", "deck", "schema", "deck"]),
            ):
                with self.assertRaises(SystemExit):
                    module.main()
                self.assertEqual(execute.call_args.args[1][-3:], ["deck", "schema", "deck"])


if __name__ == "__main__":
    unittest.main()
