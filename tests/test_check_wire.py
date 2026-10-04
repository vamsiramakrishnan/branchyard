#!/usr/bin/env python3
"""tools/check_wire.py against small fixture trees: the hard rules (a chunk
size parsed by hand, httparse outside the wire crate), the ratchet (a count
may fall, never rise, and the file list never grows), and the real tree."""

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
import check_wire

CHUNK_PARSER = """
fn read_chunk(line: &str) -> usize {
    let size = line.trim().split(';').next().unwrap_or("");
    usize::from_str_radix(size, 16).unwrap_or(0)
}
"""


class CheckWire(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.write("crates/branchyard-wire/src/lib.rs", CHUNK_PARSER + "use httparse;\n")
        self.write("crates/branchyard-wire/Cargo.toml", 'httparse = "1"\n')
        self.write("crates/a/src/lib.rs", "fn clean() {}\n")
        self.write("crates/a/Cargo.toml", '[dependencies]\nbranchyard-wire = { path = "../branchyard-wire" }\n')
        self.ratchet({})

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, rel, text):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def ratchet(self, allowed):
        self.write(check_wire.RATCHET, json.dumps(allowed))

    def problems(self):
        return check_wire.check(self.root)[0]

    def assert_problem(self, needle):
        problems = self.problems()
        self.assertTrue(any(needle in p for p in problems), problems)

    def test_a_clean_tree_passes_and_the_wire_crate_is_exempt(self):
        self.assertEqual(self.problems(), [])

    def test_a_chunk_size_parsed_by_hand_fails(self):
        self.write("crates/a/src/lib.rs", CHUNK_PARSER)
        self.assert_problem("parses a chunk size by hand")

    def test_a_chunk_size_parsed_by_hand_in_a_test_fails_too(self):
        self.write("crates/a/tests/t.rs", CHUNK_PARSER)
        self.assert_problem("parses a chunk size by hand")

    def test_radix_parsing_that_is_not_a_chunk_is_fine(self):
        self.write("crates/a/src/lib.rs", "fn mode(m: &str) -> u32 { u32::from_str_radix(m, 8).unwrap() }\n")
        self.assertEqual(self.problems(), [])

    def test_httparse_outside_the_wire_crate_fails(self):
        self.write("crates/a/src/lib.rs", "use httparse::Request;\n")
        self.assert_problem("uses httparse")
        self.write("crates/a/src/lib.rs", "fn clean() {}\n")
        self.write("crates/a/Cargo.toml", 'httparse = "1"\n')
        self.assert_problem("depends on httparse")

    def test_a_new_framing_literal_fails_until_it_is_ratcheted(self):
        self.write("crates/a/src/lib.rs", 'fn f(n: &str) -> bool { n == "content-length" }\n')
        self.assert_problem("1 framing_literals (ratchet allows 0)")
        self.ratchet({"framing_literals": {"crates/a/src/lib.rs": 1}})
        self.assertEqual(self.problems(), [])

    def test_a_count_over_its_ratchet_fails(self):
        self.ratchet({"framing_literals": {"crates/a/src/lib.rs": 1}})
        self.write(
            "crates/a/src/lib.rs",
            'fn f(n: &str) -> bool { n == "content-length" || n == "transfer-encoding" }\n',
        )
        self.assert_problem("2 framing_literals (ratchet allows 1)")

    def test_a_count_under_its_ratchet_fails_so_it_can_only_fall(self):
        self.ratchet({"framing_literals": {"crates/a/src/lib.rs": 2}})
        self.write("crates/a/src/lib.rs", 'fn f(n: &str) -> bool { n == "content-length" }\n')
        self.assert_problem("fell to 1 (ratchet says 2)")
        self.write("crates/a/src/lib.rs", "fn clean() {}\n")
        self.assert_problem("fell to 0 (ratchet says 2)")

    def test_literals_in_test_modules_do_not_count(self):
        self.write(
            "crates/a/src/lib.rs",
            'fn clean() {}\n#[cfg(test)]\nmod tests {\n    const H: &str = "content-length";\n}\n',
        )
        self.assertEqual(self.problems(), [])

    def test_a_hand_written_head_reader_is_counted(self):
        self.write(
            "crates/a/src/lib.rs",
            "fn head(r: &mut impl std::io::BufRead) { let _ = \"HTTP/1.1\"; r.read_until(b'\\n', &mut Vec::new()); }\n",
        )
        self.assert_problem("1 hand_written_heads (ratchet allows 0)")

    def test_the_real_tree_passes(self):
        problems, _ = check_wire.check()
        self.assertEqual(problems, [])


if __name__ == "__main__":
    unittest.main()
