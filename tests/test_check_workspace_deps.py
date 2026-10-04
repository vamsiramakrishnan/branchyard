#!/usr/bin/env python3
"""tools/check_workspace_deps.py on small fixture workspaces, plus the real one."""
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
import check_workspace_deps  # noqa: E402


def workspace(root_deps, **members):
    tmp = tempfile.TemporaryDirectory()
    root = Path(tmp.name)
    deps = "\n".join(f"{k} = {v}" for k, v in root_deps.items())
    (root / "Cargo.toml").write_text(f"[workspace]\nmembers = []\n[workspace.dependencies]\n{deps}\n")
    for name, body in members.items():
        (root / "crates" / name).mkdir(parents=True)
        (root / "crates" / name / "Cargo.toml").write_text(f'[package]\nname = "{name}"\n{body}')
    return tmp, root


class CheckWorkspaceDeps(unittest.TestCase):
    def run_check(self, root_deps, **members):
        tmp, root = workspace(root_deps, **members)
        self.addCleanup(tmp.cleanup)
        return check_workspace_deps.check(root)

    def test_the_real_workspace_is_clean(self):
        self.assertEqual(check_workspace_deps.check(ROOT), [])

    def test_inherited_dependencies_pass(self):
        errors = self.run_check(
            {"serde": '"1"'},
            a='[dependencies]\nserde = { workspace = true, features = ["derive"] }\n',
            b="[dependencies]\nserde = { workspace = true }\n",
        )
        self.assertEqual(errors, [])

    def test_a_version_repeated_across_members_fails(self):
        errors = self.run_check(
            {},
            a='[dependencies]\nserde = "1"\n',
            b='[dev-dependencies]\nserde = "1.0.229"\n',
        )
        self.assertEqual(len(errors), 1)
        self.assertIn("serde", errors[0])

    def test_a_version_next_to_a_hoisted_entry_fails(self):
        errors = self.run_check(
            {"serde": '"1"'},
            a='[dependencies]\nserde = "1"\n',
        )
        self.assertEqual(len(errors), 1)
        self.assertIn("workspace = true", errors[0])

    def test_target_tables_count(self):
        errors = self.run_check(
            {},
            a='[target."cfg(unix)".dependencies]\nrustix = "1"\n',
            b='[dependencies]\nrustix = "1"\n',
        )
        self.assertEqual(len(errors), 1)

    def test_a_single_use_and_path_dependencies_are_fine(self):
        errors = self.run_check(
            {},
            a='[dependencies]\naxum = "0.8"\nb = { path = "../b" }\n',
            b='[dependencies]\nb2 = { path = "../b" }\n',
        )
        self.assertEqual(errors, [])

    def test_inheriting_an_undeclared_dependency_fails(self):
        errors = self.run_check({}, a="[dependencies]\nserde = { workspace = true }\n")
        self.assertEqual(len(errors), 1)


if __name__ == "__main__":
    unittest.main()
