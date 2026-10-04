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


class PoisonIdiom(unittest.TestCase):
    def run_check(self, files):
        return check.check(check.scan(tree(files)))

    def test_clean_tree_passes(self):
        self.assertEqual(self.run_check({"crates/a/src/lib.rs": "fn f() {\n    let _ = g();\n}\n"}), [])

    def test_test_code_is_not_scanned(self):
        test_mod = "mod t { fn x() { m.lock().unwrap_or_else(|e| e.into_inner()); } }"
        files = {"crates/a/src/lib.rs": f"fn f() {{}}\n#[cfg(test)]\n{test_mod}\n"}
        self.assertEqual(self.run_check(files), [])

    def test_the_poison_idiom_fails_in_every_spelling(self):
        for spelling in (
            "m.lock().unwrap_or_else(|e| e.into_inner())",
            "m.lock()\n  .unwrap_or_else(|poisoned| poisoned.into_inner())",
            "m.lock().unwrap_or_else(PoisonError::into_inner)",
        ):
            files = {"crates/a/src/lib.rs": f"fn f() {{ {spelling}; }}\n"}
            self.assertTrue(self.run_check(files), spelling)

    def test_the_repository_is_clean(self):
        self.assertEqual(check.check(check.scan(ROOT)), [])


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
