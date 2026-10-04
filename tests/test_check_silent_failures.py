#!/usr/bin/env python3
"""check_silent_failures.py fails on each regression it exists to stop."""

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("check", ROOT / "tools" / "check_silent_failures.py")
assert spec is not None and spec.loader is not None
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)


def tree(files):
    root = Path(tempfile.mkdtemp())
    for rel, text in files.items():
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    return root


class Ratchet(unittest.TestCase):
    def run_check(self, files, baseline):
        counts, idiom = check.scan(tree(files))
        return check.check(counts, idiom, baseline)

    def test_clean_tree_at_baseline_passes(self):
        files = {"crates/a/src/lib.rs": "fn f() {\n    let _ = g();\n}\n"}
        self.assertEqual(self.run_check(files, {"a": 1}), [])

    def test_a_new_let_underscore_fails(self):
        files = {"crates/a/src/lib.rs": "fn f() {\n    let _ = g();\n    let _ = h();\n}\n"}
        self.assertTrue(self.run_check(files, {"a": 1}))

    def test_a_crate_missing_from_the_baseline_fails(self):
        files = {"crates/b/src/lib.rs": "fn f() {\n    let _ = g();\n}\n"}
        self.assertTrue(self.run_check(files, {}))

    def test_a_fall_must_lower_the_baseline(self):
        files = {"crates/a/src/lib.rs": "fn f() {}\n"}
        problems = self.run_check(files, {"a": 1})
        self.assertTrue(problems and "lower it" in problems[0])

    def test_test_code_is_not_counted(self):
        files = {"crates/a/src/lib.rs": "fn f() {}\n#[cfg(test)]\nmod t { fn x() { let _ = y(); } }\n"}
        self.assertEqual(self.run_check(files, {}), [])

    def test_the_poison_idiom_fails_in_every_spelling(self):
        for spelling in (
            "m.lock().unwrap_or_else(|e| e.into_inner())",
            "m.lock()\n  .unwrap_or_else(|poisoned| poisoned.into_inner())",
            "m.lock().unwrap_or_else(PoisonError::into_inner)",
        ):
            files = {"crates/a/src/lib.rs": f"fn f() {{ {spelling}; }}\n"}
            self.assertTrue(self.run_check(files, {}), spelling)

    def test_the_repository_is_at_baseline(self):
        counts, idiom = check.scan(ROOT)
        baseline = check.json.loads(check.BASELINE.read_text())
        self.assertEqual(check.check(counts, idiom, baseline), [])


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
