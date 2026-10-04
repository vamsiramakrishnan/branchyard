#!/usr/bin/env python3
"""tools/check_test_hygiene.py against small fixture trees, and against this
repository: a local wait_until, fake_agent or raw sleep in a test fails; the
counted ratchets (settle, temp_dir, tcp_listener) can fall but not grow."""
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
import check_test_hygiene as hygiene  # noqa: E402


class Hygiene(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        (self.root / "tools").mkdir()
        self.write("crates/a/tests/ok.rs", "use branchyard_testkit::wait;\nfn t() { wait::until(\"x\", || true); }\n")

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, rel, text):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def ratchet(self, **counts):
        data = {"format": 1}
        data.update(counts)
        (self.root / hygiene.RATCHET).write_text(json.dumps(data))

    def problems(self, **kwargs):
        return hygiene.check(self.root, **kwargs)

    def test_a_clean_tree_passes(self):
        self.assertEqual(self.problems(), [])

    def test_a_new_wait_until_in_a_test_fails(self):
        self.write("crates/a/tests/new.rs", "fn wait_until(what: &str, f: impl FnMut() -> bool) {}\n")
        found = self.problems()
        self.assertEqual(len(found), 1, found)
        self.assertIn("crates/a/tests/new.rs:1", found[0])
        self.assertIn("wait_until", found[0])

    def test_each_banned_name_fails(self):
        for name in hygiene.BANNED_FNS:
            with self.subTest(name=name):
                self.write("crates/a/tests/new.rs", f"pub fn {name}() {{}}\n")
                self.assertEqual(len(self.problems()), 1)

    def test_names_that_only_contain_a_banned_name_pass(self):
        self.write("crates/a/tests/new.rs", "fn wait_for_own_prompt() {}\nfn until_ready() {}\n")
        self.assertEqual(self.problems(), [])

    def test_a_raw_sleep_in_a_test_fails(self):
        self.write("crates/a/tests/new.rs", "fn t() { std::thread::sleep(D); }\n")
        found = self.problems()
        self.assertEqual(len(found), 1, found)
        self.assertIn("raw sleep", found[0])

    def test_a_commented_out_sleep_or_fn_is_not_code(self):
        self.write("crates/a/tests/new.rs", "// fn wait_until() { thread::sleep(D) }\n")
        self.assertEqual(self.problems(), [])

    def test_the_kit_itself_may_poll_and_sleep(self):
        self.write(
            "crates/branchyard-testkit/src/wait.rs",
            "pub fn until() { std::thread::sleep(D); }\nfn fake_agent() {}\n",
        )
        self.assertEqual(self.problems(), [])

    def test_the_conformance_suite_is_covered(self):
        self.write("crates/branchyard/src/conformance.rs", "fn t() { thread::sleep(D); }\n")
        self.assertEqual(len(self.problems()), 1)

    def test_a_ratchet_count_may_not_grow(self):
        self.write("crates/a/tests/s.rs", "fn t() { wait::settle(\"w\", D); wait::settle(\"w\", D); }\n")
        self.ratchet(settle={"crates/a/tests/s.rs": 1})
        found = self.problems()
        self.assertEqual(len(found), 1, found)
        self.assertIn("2 settle (the ratchet allows 1)", found[0])

    def test_a_file_not_in_the_ratchet_may_have_none(self):
        self.write("crates/a/tests/s.rs", "fn t() { let d = std::env::temp_dir(); }\n")
        found = self.problems()
        self.assertEqual(len(found), 1, found)
        self.assertIn("1 temp_dir (the ratchet allows 0)", found[0])
        self.write("crates/a/tests/m.rs", "fn t() { TcpListener::bind(A); }\n")
        self.assertEqual(len(self.problems()), 2)

    def test_a_ratchet_that_is_too_high_must_be_lowered(self):
        self.write("crates/a/tests/s.rs", "fn t() { wait::settle(\"w\", D); }\n")
        self.ratchet(settle={"crates/a/tests/s.rs": 3})
        found = self.problems()
        self.assertEqual(len(found), 1, found)
        self.assertIn("still allows 3", found[0])

    def test_lower_rewrites_only_downwards(self):
        self.write("crates/a/tests/s.rs", "fn t() { wait::settle(\"w\", D); }\n")
        self.ratchet(
            settle={"crates/a/tests/s.rs": 3, "crates/a/tests/gone.rs": 2},
            temp_dir={},
            tcp_listener={},
        )
        self.assertEqual(self.problems(lower=True), [])
        data = json.loads((self.root / hygiene.RATCHET).read_text())
        self.assertEqual(data["settle"], {"crates/a/tests/s.rs": 1})
        # Growing is never rewritten: it still fails.
        self.write("crates/a/tests/s.rs", "fn t() { wait::settle(\"w\", D); wait::settle(\"w\", D); }\n")
        self.assertEqual(len(self.problems(lower=True)), 1)

    def test_this_repository_passes(self):
        self.assertEqual(hygiene.check(hygiene.ROOT), [])


if __name__ == "__main__":
    unittest.main()
