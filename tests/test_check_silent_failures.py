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


def reads(body, prelude=""):
    """The read hits of a function whose body is `body`."""
    return check.read_hits(f"{prelude}\nfn f() {{\n{body}\n}}\n")


class ReadErrors(unittest.TestCase):
    """`.ok()` and `.unwrap_or_default()` on a read turn an error into 'nothing'."""

    def test_each_kind_of_read_is_flagged(self):
        for body in (
            "let r = store.read(name).ok();",
            "let all = yard.store().list().unwrap_or_default();",
            "let r = ledger.read(&plan.branch).ok();",
            "let events = record::read(&store, name).unwrap_or_default();",
            "let text = std::fs::read_to_string(path).unwrap_or_default();",
            "let text = fs::read(path).ok();",
            "let entries = fs::read_dir(dir).ok()?;",
            "let r = self.store().backend().read(child).ok().flatten();",
        ):
            hits = reads(body)
            self.assertEqual(len(hits), 1, body)
            self.assertEqual(hits[0][0], 3, "the line of the call in: " + body)

    def test_a_chain_across_lines_is_one_hit_on_the_line_of_the_call(self):
        hits = reads("let r = store\n    .read(name)\n    .ok()\n    .and_then(|r| r.parent);")
        self.assertEqual([(line, call) for line, call, _ in hits], [(5, "ok")])

    def test_two_swallows_of_one_read_on_a_line_are_one_hit(self):
        hits = reads("let v = serde_json::from_str(&fs::read_to_string(&path).ok()?).ok()?;")
        self.assertEqual(len(hits), 1, hits)

    def test_what_is_not_a_read_of_the_store_or_a_file_is_not_flagged(self):
        for body in (
            'let n = "3".parse::<u32>().ok();',
            "let v = map.get(key).cloned().unwrap_or_default();",
            "let v = env::var(name).ok();",
            "let r = result.ok();",
        ):
            self.assertEqual(reads(body), [], body)

    def test_a_read_whose_failure_is_logged_first_is_the_sanctioned_fallback(self):
        body = (
            'let events = branchyard_support::best_effort("read the log", record::read(&store, n)).unwrap_or_default();'
        )
        self.assertEqual(reads(body), [])
        once = 'let rows = best_effort_once("read rows", store.list()).unwrap_or_default();'
        self.assertEqual(reads(once), [])

    def test_a_read_whose_error_was_returned_is_not_flagged(self):
        # The `?` took the error; what is defaulted is the `Option` inside.
        self.assertEqual(reads("let b = store.backend().read(parent)?.map(|p| p.name).unwrap_or_default();"), [])
        # A `?` in the read's own arguments does not.
        self.assertEqual(len(reads("let t = fs::read_to_string(self.file(name)?).ok()?;")), 1)

    def test_comments_and_strings_are_not_code(self):
        body = '// let r = store.read(x).ok();\nlet s = "store.read(x).ok()";\n/* fs::read(p).ok() */'
        self.assertEqual(reads(body), [])

    def test_test_code_is_not_scanned_but_what_follows_it_is(self):
        source = (
            "fn a() { store.read(x).ok(); }\n"
            "#[cfg(test)]\nfn only_in_tests() { store.read(x).ok(); }\n"
            "fn b() { store.read(y).ok(); }\n"
            "#[cfg(test)]\nmod tests { fn t() { fs::read(p).ok(); } }\n"
        )
        self.assertEqual([line for line, _, _ in check.read_hits(source)], [1, 4])

    def run_reads(self, files, ratchet):
        return check.check_reads(check.scan_reads(tree(files)), ratchet)

    def test_a_new_read_in_an_unlisted_file_fails_with_where_and_what_to_do(self):
        files = {"crates/a/src/lib.rs": "fn f() {\n    let r = store.read(n).ok();\n}\n"}
        problems = self.run_reads(files, {"reads": {}})
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("crates/a/src/lib.rs:2:", problems[0])
        self.assertIn("Error::could_not", problems[0])
        self.assertIn("best_effort", problems[0])

    def test_the_ratchet_allows_its_count_and_no_more_and_no_fewer(self):
        two = "fn f() {\n    store.read(a).ok();\n    store.read(b).ok();\n}\n"
        files = {"crates/a/src/lib.rs": two}
        listed = lambda n: {"reads": {"crates/a/src/lib.rs": {"count": n, "why": "tolerated: absent means none"}}}  # noqa: E731
        self.assertEqual(self.run_reads(files, listed(2)), [])
        over = self.run_reads(files, listed(1))
        self.assertTrue(over and "crates/a/src/lib.rs:2:" in over[0], over)
        under = self.run_reads(files, listed(3))
        self.assertEqual(len(under), 1, under)
        self.assertIn("lower its count to 2", under[0])

    def test_an_entry_with_nothing_left_or_no_reason_fails(self):
        clean = {"crates/a/src/lib.rs": "fn f() {}\n"}
        gone = self.run_reads(clean, {"reads": {"crates/a/src/lib.rs": {"count": 1, "why": "x"}}})
        self.assertTrue(any("remove its entry" in p for p in gone), gone)
        files = {"crates/a/src/lib.rs": "fn f() { store.read(a).ok(); }\n"}
        unexplained = self.run_reads(files, {"reads": {"crates/a/src/lib.rs": {"count": 1, "why": " "}}})
        self.assertTrue(any("needs a `why`" in p for p in unexplained), unexplained)

    def test_test_support_is_exempt(self):
        body = "fn f() { fs::read_to_string(p).unwrap_or_default(); }\n"
        self.assertEqual(self.run_reads({"crates/branchyard-testkit/src/wait.rs": body}, {"reads": {}}), [])

    def test_the_repository_is_within_its_ratchet(self):
        self.assertEqual(check.check_reads(check.scan_reads(ROOT), check.load_ratchet(ROOT)), [])


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
