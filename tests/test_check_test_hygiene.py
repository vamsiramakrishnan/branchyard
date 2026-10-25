#!/usr/bin/env python3
"""tools/check_test_hygiene.py against small fixture trees, and against this
repository: a local wait_until, fake_agent or raw sleep in a test fails, in
the tests directories and in the `#[cfg(test)]` code of a crate's src; so do
a freed port, a process-wide variable, a /tmp path and an unexplained long
settle; the counted ratchets (settle, temp_dir, tcp_listener) can fall but not
grow."""

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
import check_test_hygiene as hygiene


class Hygiene(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        (self.root / "tools").mkdir()
        self.write("crates/a/tests/ok.rs", 'use branchyard_testkit::wait;\nfn t() { wait::until("x", || true); }\n')

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

    def allow(self, *entries):
        data = {
            "format": 1,
            "long_settle": [{"file": file, "why": why, "reason": reason} for file, why, reason in entries],
        }
        (self.root / hygiene.ALLOW).write_text(json.dumps(data))

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
        self.write(
            "crates/a/tests/s.rs",
            'fn t() { wait::settle("w", Duration::from_millis(1)); wait::settle("w", Duration::from_millis(1)); }\n',
        )
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
        self.write("crates/a/tests/s.rs", 'fn t() { wait::settle("w", Duration::from_millis(1)); }\n')
        self.ratchet(settle={"crates/a/tests/s.rs": 3})
        found = self.problems()
        self.assertEqual(len(found), 1, found)
        self.assertIn("still allows 3", found[0])

    def test_lower_rewrites_only_downwards(self):
        self.write("crates/a/tests/s.rs", 'fn t() { wait::settle("w", Duration::from_millis(1)); }\n')
        self.ratchet(
            settle={"crates/a/tests/s.rs": 3, "crates/a/tests/gone.rs": 2},
            temp_dir={},
            tcp_listener={},
        )
        self.assertEqual(self.problems(lower=True), [])
        data = json.loads((self.root / hygiene.RATCHET).read_text())
        self.assertEqual(data["settle"], {"crates/a/tests/s.rs": 1})
        # Growing is never rewritten: it still fails.
        self.write(
            "crates/a/tests/s.rs",
            'fn t() { wait::settle("w", Duration::from_millis(1)); wait::settle("w", Duration::from_millis(1)); }\n',
        )
        self.assertEqual(len(self.problems(lower=True)), 1)

    # The shapes that made tests fail on a slow machine.

    def test_a_bare_sleep_after_a_use_fails_but_a_method_a_definition_and_a_string_do_not(self):
        self.write("crates/a/tests/s.rs", "use std::thread::*;\nfn t() { sleep(D); }\n")
        found = self.problems()
        self.assertEqual(len(found), 1, found)
        self.assertIn("crates/a/tests/s.rs:2", found[0])
        self.write(
            "crates/a/tests/s.rs",
            'fn sleep(d: D) {}\nfn t() { clock.sleep(D); let py = "from time import sleep; sleep(5)"; }\n',
        )
        self.assertEqual(self.problems(), [])

    def test_a_sleep_in_a_cfg_test_module_of_a_src_file_fails(self):
        self.write(
            "crates/a/src/lib.rs",
            "fn product() { std::thread::sleep(D); }\n"
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() { std::thread::sleep(D); }\n}\n",
        )
        found = self.problems()
        self.assertEqual(len(found), 1, found)
        self.assertIn("crates/a/src/lib.rs:5", found[0])

    def test_braces_in_strings_and_comments_do_not_end_a_test_module_early(self):
        self.write(
            "crates/a/src/lib.rs",
            '#[cfg(test)]\nmod tests {\n    const J: &str = r#"{ "a": {"#;\n    // }\n'
            "    fn t() { let c = '}'; let s = \"}}\"; thread::sleep(D); }\n}\n",
        )
        self.assertEqual(len(self.problems()), 1)

    def test_an_out_of_line_cfg_test_module_is_test_code(self):
        self.write("crates/a/src/lib.rs", "#[cfg(test)]\nmod tests;\nfn product() { thread::sleep(D); }\n")
        self.write("crates/a/src/tests.rs", "fn t() { thread::sleep(D); }\n")
        found = self.problems()
        self.assertEqual(len(found), 1, found)
        self.assertIn("crates/a/src/tests.rs:1", found[0])

    def test_a_test_function_outside_a_cfg_test_module_is_test_code(self):
        self.write("crates/a/src/lib.rs", "#[test]\nfn t() { thread::sleep(D); }\nfn product() { thread::sleep(D); }\n")
        found = self.problems()
        self.assertEqual(len(found), 1, found)
        self.assertIn("lib.rs:2", found[0])

    def test_the_ratchets_count_cfg_test_code_in_src(self):
        self.write(
            "crates/a/src/lib.rs",
            "fn p() { TcpListener::bind(A); }\n#[cfg(test)]\nmod t { fn f() { TcpListener::bind(A); } }\n",
        )
        found = self.problems()
        self.assertEqual(len(found), 1, found)
        self.assertIn("1 tcp_listener (the ratchet allows 0)", found[0])

    def test_a_listener_dropped_after_its_port_was_read_fails(self):
        for body in (
            'let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();',
            'let url = format!("http://{}", TcpListener::bind(("127.0.0.1", 0)).expect("bind").local_addr().unwrap());',
            'let l = TcpListener::bind("127.0.0.1:0").unwrap();\n    let a = l.local_addr().unwrap();\n    drop(l);',
            'let mut closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();\n'
            "    let url = closed.local_addr().unwrap();\n    drop(closed);",
        ):
            with self.subTest(body=body):
                self.write("crates/a/tests/p.rs", f"fn t() {{\n    {body}\n}}\n")
                self.ratchet(tcp_listener={"crates/a/tests/p.rs": 1})
                found = self.problems()
                self.assertEqual(len(found), 1, found)
                self.assertIn("a port is read from a listener that is then dropped", found[0])

    def test_a_listener_that_is_kept_or_a_refusal_on_port_1_passes(self):
        self.write(
            "crates/a/tests/p.rs",
            'fn t() {\n    let l = TcpListener::bind("127.0.0.1:0").unwrap();\n'
            "    let port = l.local_addr().unwrap().port();\n    serve(l);\n"
            '    let refused = "http://127.0.0.1:1";\n}\n'
            'fn probe(port: u16) -> bool { TcpListener::bind(("127.0.0.1", port)).is_err() }\n'
            "fn other() { let l = x; drop(l); }\n",
        )
        self.ratchet(tcp_listener={"crates/a/tests/p.rs": 2})
        self.assertEqual(self.problems(), [])

    def test_a_drop_in_another_function_is_not_the_listeners(self):
        self.write(
            "crates/a/tests/p.rs",
            'fn one() {\n    let l = TcpListener::bind("127.0.0.1:0").unwrap();\n    keep(l.local_addr());\n}\n'
            "fn two(l: L) {\n    drop(l);\n}\n",
        )
        self.ratchet(tcp_listener={"crates/a/tests/p.rs": 1})
        self.assertEqual(self.problems(), [])

    def test_set_var_in_a_test_body_fails(self):
        self.write(
            "crates/a/tests/e.rs",
            '#[test]\nfn one() {\n    std::env::set_var("SSL_CERT_FILE", x);\n}\n#[test]\nfn two() {}\n',
        )
        found = self.problems()
        self.assertEqual(len(found), 1, found)
        self.assertIn("crates/a/tests/e.rs:3", found[0])
        self.assertIn("set_var in the test `one`", found[0])
        self.write(
            "crates/a/tests/e.rs",
            '#[test]\nfn one() {\n    env::remove_var("PATH");\n}\n#[test]\nfn two() {}\n',
        )
        self.assertIn("remove_var", self.problems()[0])

    def test_set_var_in_a_fixture_in_a_once_for_a_test_input_or_in_a_binary_of_one_test_passes(self):
        self.write(
            "crates/a/tests/e.rs",
            'fn fixture() {\n    std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");\n}\n'
            "#[test]\nfn once() {\n    static S: Once = Once::new();\n"
            '    S.call_once(|| {\n        std::env::set_var("WEBHOOK_RETRY_MS", "5");\n    });\n}\n'
            '#[test]\nfn lazy() {\n    let v = CELL.get_or_init(|| { std::env::set_var("X", "1"); 1 });\n}\n'
            '#[test]\nfn input() {\n    std::env::set_var("BY_TEST_SECRET", "s");\n}\n',
        )
        self.assertEqual(self.problems(), [])
        self.write(
            "crates/a/tests/alone.rs",
            '#[test]\nfn only() {\n    std::env::set_var("SSL_CERT_FILE", x);\n}\n',
        )
        self.assertEqual(self.problems(), [])

    def test_set_var_outside_the_call_once_closure_fails(self):
        self.write(
            "crates/a/tests/e.rs",
            "#[test]\nfn one() {\n    static S: Once = Once::new();\n    S.call_once(|| {});\n"
            '    std::env::set_var("PATH", "x");\n}\n#[test]\nfn two() {}\n',
        )
        found = self.problems()
        self.assertEqual(len(found), 1, found)
        self.assertIn("e.rs:5", found[0])

    def test_set_var_in_a_src_test_fails_unless_it_is_a_test_input(self):
        self.write(
            "crates/a/src/lib.rs",
            '#[cfg(test)]\nmod tests {\n    #[test]\n    fn a() {\n        std::env::set_var("LANG", "C");\n    }\n'
            '    #[test]\n    fn b() {\n        std::env::set_var("BY_TEST_X", "1");\n    }\n}\n',
        )
        found = self.problems()
        self.assertEqual(len(found), 1, found)
        self.assertIn("lib.rs:5", found[0])

    def test_a_tmp_path_literal_fails_unless_it_says_why(self):
        self.write("crates/a/tests/t.rs", 'fn t() { let p = Path::new("/tmp/by-local"); }\n')
        found = self.problems()
        self.assertEqual(len(found), 1, found)
        self.assertIn("a /tmp path literal", found[0])
        for text in (
            'fn t() { run("touch /tmp/never-run"); }\n',
            'fn t() { let u = "file:///tmp/x"; }\n',
            'fn t() { let p = PathBuf::from("/tmp"); }\n',
        ):
            with self.subTest(text=text):
                self.write("crates/a/tests/t.rs", text)
                self.assertEqual(len(self.problems()), 1)
        self.write(
            "crates/a/tests/t.rs",
            'fn t() {\n    // hygiene-allow: tmp a guest path\n    let p = Path::new("/tmp/by-local");\n'
            '    let q = Path::new("/tmp/x"); // hygiene-allow: tmp the product contract\n}\n',
        )
        self.assertEqual(self.problems(), [])

    def test_a_tmp_that_is_not_a_path_root_or_not_code_passes(self):
        self.write(
            "crates/a/tests/t.rs",
            'fn t() {\n    // /tmp/x in a comment\n    let tmp = root.join(".branchyard/tmp/b");\n'
            '    let s = "target/tmp/repo";\n    run("--root", "/tmp", "--branch");\n}\n'
            "/// Docs name /tmp/branchyard/<branch>.\nfn d() {}\n",
        )
        self.assertEqual(self.problems(), [])

    def test_a_long_settle_needs_an_entry_with_a_reason(self):
        self.write(
            "crates/a/tests/s.rs",
            'fn t() {\n    wait::settle(\n        "a quiet period",\n        Duration::from_millis(500),\n    );\n}\n',
        )
        self.ratchet(settle={"crates/a/tests/s.rs": 1})
        found = self.problems()
        self.assertEqual(len(found), 1, found)
        self.assertIn("crates/a/tests/s.rs:2: wait::settle('a quiet period') is 500 ms", found[0])
        self.allow(("crates/a/tests/s.rs", "a quiet period", "nothing may happen"))
        self.assertEqual(self.problems(), [])

    def test_a_settle_under_half_a_second_needs_no_entry(self):
        self.write(
            "crates/a/tests/s.rs",
            'fn t() { wait::settle("w", Duration::from_millis(499)); '
            'wait::settle("x", std::time::Duration::from_millis(200)); }\n',
        )
        self.ratchet(settle={"crates/a/tests/s.rs": 2})
        self.assertEqual(self.problems(), [])

    def test_seconds_and_durations_it_cannot_evaluate_are_long(self):
        for duration in (
            "Duration::from_secs(1)",
            "Duration::from_secs_f64(0.5)",
            "STALL",
            "Duration::from_nanos(n - m)",
        ):
            with self.subTest(duration=duration):
                self.write("crates/a/tests/s.rs", f'fn t() {{ wait::settle("w", {duration}); }}\n')
                self.ratchet(settle={"crates/a/tests/s.rs": 1})
                self.assertEqual(len(self.problems()), 1)

    def test_an_entry_for_a_settle_that_is_gone_or_short_fails(self):
        self.write("crates/a/tests/s.rs", 'fn t() { wait::settle("w", Duration::from_millis(100)); }\n')
        self.ratchet(settle={"crates/a/tests/s.rs": 1})
        self.allow(("crates/a/tests/s.rs", "w", "r"), ("crates/a/tests/gone.rs", "x", "r"))
        found = self.problems()
        self.assertEqual(len(found), 2, found)
        self.assertTrue(all("remove the entry" in problem for problem in found), found)

    def test_the_entry_names_the_file_and_the_why(self):
        self.write("crates/a/tests/s.rs", 'fn t() { wait::settle("w", Duration::from_secs(2)); }\n')
        self.ratchet(settle={"crates/a/tests/s.rs": 1})
        self.allow(("crates/a/tests/other.rs", "w", "r"))
        self.assertEqual(len(self.problems()), 2)

    def test_the_kit_may_use_every_shape(self):
        self.write(
            "crates/branchyard-testkit/src/lib.rs",
            "#[cfg(test)]\nmod t { fn f() { sleep(D); TcpListener::bind(A).unwrap().local_addr(); }\n"
            '  fn g() { set_var("PATH", "x"); } }\n',
        )
        self.assertEqual(self.problems(), [])

    def test_the_allow_list_of_this_repository_has_a_reason_for_each_entry(self):
        data = json.loads((hygiene.ROOT / hygiene.ALLOW).read_text())
        for entry in data["long_settle"]:
            self.assertGreater(len(entry["reason"].strip()), 40, entry)

    def test_this_repository_passes(self):
        self.assertEqual(hygiene.check(hygiene.ROOT), [])


if __name__ == "__main__":
    unittest.main()
