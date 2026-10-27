#!/usr/bin/env python3
"""check_failure_messages.py counts what it should, and fails on each regression it exists to stop."""

import contextlib
import importlib.util
import io
import json
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("check", ROOT / "tools" / "check_failure_messages.py")
assert spec is not None and spec.loader is not None
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)

CLI = "crates/branchyard-cli/src"


def tree(files, ratchet=None):
    root = Path(tempfile.mkdtemp())
    for rel, text in files.items():
        path = root / CLI / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    if ratchet is not None:
        (root / "tools").mkdir(exist_ok=True)
        (root / check.RATCHET).write_text(json.dumps({"files": ratchet}))
    return root


def body(code):
    return f"fn f() -> Outcome {{\n{code}\n}}\n"


class Sites(unittest.TestCase):
    def test_each_way_of_building_one_is_a_site(self):
        for code in (
            'return Err(Failure::Message("no".into()));',
            "x.map_err(Failure::Message)?;",
            'x.ok_or_else(|| Failure::Message(format!("{a}")))?;',
            'Err(Failure :: Message("spaced".into()))',
        ):
            self.assertEqual(len(check.sites(body(code))), 1, code)

    def test_a_pattern_that_reads_one_is_not_a_site(self):
        for code in (
            "match f { Failure::Message(m) => m, _ => String::new() }",
            "match f { Failure::Message(m) | Failure::Manual(m) => m, _ => String::new() }",
            "match f { Failure::Message(m) if m.is_empty() => m, _ => String::new() }",
        ):
            self.assertEqual(check.sites(body(code)), [], code)

    def test_comments_strings_and_tests_are_not_code(self):
        source = (
            '// Failure::Message("old")\n'
            "/// a `Failure::Message` is the old shape\n"
            'const S: &str = "Failure::Message(x)";\n'
            "fn f() {}\n"
            "#[cfg(test)]\nmod tests {\n    fn t() { Failure::Message(String::new()); }\n}\n"
        )
        self.assertEqual(check.sites(source), [])

    def test_the_line_of_each_site_is_reported(self):
        source = 'fn f() {\n    a();\n    Err(Failure::Message("x".into()))\n}\n'
        self.assertEqual(check.sites(source), [3])


class Ratchet(unittest.TestCase):
    SITE = body('Err(Failure::Message("x".into()))')
    TWO = body('Err(Failure::Message("x".into())); Err(Failure::Message("y".into()))')

    def problems(self, files, ratchet):
        root = tree(files, ratchet)
        return check.check(check.scan(root), check.load(root))

    def test_counts_as_listed_pass(self):
        self.assertEqual(self.problems({"a.rs": self.SITE}, {f"{CLI}/a.rs": 1}), [])
        self.assertEqual(self.problems({"a.rs": "fn f() {}\n"}, {}), [])

    def test_a_count_that_rises_fails_and_says_what_to_build(self):
        found = self.problems({"a.rs": self.TWO}, {f"{CLI}/a.rs": 1})
        self.assertEqual(len(found), 1)
        self.assertIn("Report::new(kind, what)", found[0])

    def test_a_file_that_joins_fails(self):
        found = self.problems({"new.rs": self.SITE}, {})
        self.assertEqual(len(found), 1)
        self.assertIn("allows 0", found[0])

    def test_a_count_that_falls_must_be_lowered(self):
        found = self.problems({"a.rs": self.SITE}, {f"{CLI}/a.rs": 2})
        self.assertEqual(len(found), 1)
        self.assertIn("--update", found[0])
        found = self.problems({"a.rs": "fn f() {}\n"}, {f"{CLI}/a.rs": 1})
        self.assertIn("has none left", found[0])

    def test_update_lowers_and_removes_and_never_raises_or_adds(self):
        root = tree(
            {"a.rs": self.SITE, "b.rs": "fn f() {}\n", "c.rs": self.TWO, "d.rs": self.SITE},
            {f"{CLI}/a.rs": 3, f"{CLI}/b.rs": 1, f"{CLI}/c.rs": 1},
        )
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(check.main(["--update"], root=root), 0)
        files = check.load(root)["files"]
        self.assertEqual(files, {f"{CLI}/a.rs": 1, f"{CLI}/c.rs": 1})
        # c.rs over its count and d.rs unlisted still fail.
        said = io.StringIO()
        with contextlib.redirect_stdout(said):
            self.assertEqual(check.main([], root=root), 1)
        self.assertIn("c.rs: 2 Failure::Message site(s)", said.getvalue())
        self.assertIn("d.rs: 1 Failure::Message site(s)", said.getvalue())

    def test_the_repository_is_within_its_ratchet(self):
        self.assertEqual(check.check(check.scan(), check.load()), [])


if __name__ == "__main__":
    unittest.main()
