#!/usr/bin/env python3
"""check_lint_ratchet.py fails on each regression it exists to stop."""

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("check", ROOT / "tools" / "check_lint_ratchet.py")
assert spec is not None and spec.loader is not None
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)

TABLE = '[workspace.package]\nrust-version = "1.94"\n\n[workspace.lints.clippy]\n' + "".join(
    f'{lint} = "deny"\n' for lint in check.DENIED
)
MANIFEST = '[package]\nname = "a"\nrust-version.workspace = true\n\n[lints]\nworkspace = true\n'


def tree(files):
    root = Path(tempfile.mkdtemp())
    base = {
        "Cargo.toml": TABLE,
        "rust-toolchain.toml": '[toolchain]\nchannel = "1.94.0"\n',
        "crates/a/Cargo.toml": MANIFEST,
    }
    for rel, text in {**base, **files}.items():
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    return root


def problems(files, baseline):
    root = tree(files)
    counts, allow_problems = check.scan(root)
    return allow_problems + check.manifest_problems(root) + check.check(counts, baseline)


MARKED = "#[allow(clippy::unwrap_used)] // ratchet: a\nfn f() {}\n"


class Ratchet(unittest.TestCase):
    def test_at_baseline_passes(self):
        self.assertEqual(problems({"crates/a/src/lib.rs": MARKED}, {"a": 1}), [])

    def test_a_new_marker_fails(self):
        self.assertTrue(problems({"crates/a/src/lib.rs": MARKED + MARKED}, {"a": 1}))

    def test_a_fall_must_lower_the_baseline(self):
        found = problems({"crates/a/src/lib.rs": "fn f() {}\n"}, {"a": 1})
        self.assertTrue(found and "lower it" in found[0])

    def test_an_allow_without_a_marker_fails(self):
        files = {"crates/a/src/lib.rs": "#![allow(clippy::panic)]\n"}
        self.assertTrue(problems(files, {}))

    def test_a_multi_line_allow_is_found(self):
        files = {"crates/a/src/lib.rs": "#![allow(\n    clippy::expect_used,\n    clippy::panic\n)] // ratchet: a\n"}
        self.assertEqual(problems(files, {"a": 1}), [])

    def test_tests_markers_are_for_test_code(self):
        line = "#![allow(clippy::unwrap_used)] // tests: a panic is the failure report\n"
        self.assertEqual(problems({"crates/a/tests/t.rs": line}, {}), [])
        self.assertTrue(problems({"crates/a/src/lib.rs": line}, {}))

    def test_an_unrelated_allow_is_left_alone(self):
        self.assertEqual(problems({"crates/a/src/lib.rs": "#![allow(dead_code)]\n"}, {}), [])

    def test_a_crate_without_the_lints_table_fails(self):
        files = {"crates/a/Cargo.toml": '[package]\nname = "a"\nrust-version.workspace = true\n'}
        self.assertTrue(problems(files, {}))

    def test_a_crate_without_the_rust_version_fails(self):
        files = {"crates/a/Cargo.toml": '[package]\nname = "a"\n\n[lints]\nworkspace = true\n'}
        self.assertTrue(problems(files, {}))

    def test_a_lint_left_out_of_the_table_fails(self):
        files = {"Cargo.toml": TABLE.replace('unwrap_used = "deny"\n', "")}
        self.assertTrue(problems(files, {}))

    def test_a_rust_version_that_differs_from_the_toolchain_fails(self):
        files = {"Cargo.toml": TABLE.replace("1.94", "1.80")}
        self.assertTrue(problems(files, {}))

    def test_the_repository_is_at_baseline(self):
        counts, allow_problems = check.scan(ROOT)
        baseline = check.json.loads(check.BASELINE.read_text())
        self.assertEqual(allow_problems + check.manifest_problems(ROOT) + check.check(counts, baseline), [])


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
