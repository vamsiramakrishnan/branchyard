#!/usr/bin/env python3
"""tools/check_handrolled.py passes on this tree and fails when a banned codec
is re-added, when the ratchet is not lowered, and on a stale allowlist row."""
import importlib.util
import shutil
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("check_handrolled", ROOT / "tools/check_handrolled.py")
check_handrolled = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check_handrolled)

BANLIST = ROOT / "tools/handrolled_banlist.toml"

ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
BANNED = {
    "percent-codec": "fn percent_decode(text: &str) -> String { String::new() }\n",
    "percent-escape": 'fn f(b: u8) -> String { format!("%{b:02X}") }\n',
    "hex-codec": "fn hex(bytes: &[u8]) -> String { String::new() }\n",
    "base64-alphabet": f'const T: &[u8; 64] = b"{ALPHABET}";\n',
    "base64-codec": "fn base64_encode(bytes: &[u8]) -> String { String::new() }\n",
    "shell-quote": "fn shell_quote(word: &str) -> String { word.into() }\n",
    "shell-quote-idiom": "fn q(w: &str) -> String { w.replace('\\'', \"'\\\\''\") }\n",
    "host-port": "fn host_port(url: &str) -> Option<(String, u16)> { None }\n",
    "tar-format": 'fn h(h: &mut [u8]) { h[257..263].copy_from_slice(b"ustar\\0"); }\n',
}


class Tree(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.dir)
        (self.dir / "crates/demo/src").mkdir(parents=True)
        (self.dir / "tools").mkdir()
        shutil.copy(BANLIST, self.dir / "tools/handrolled_banlist.toml")

    def write(self, text, name="crates/demo/src/lib.rs"):
        path = self.dir / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def errors(self, stale=False):
        """The gate's errors; the real allowlist's rows name files this tree lacks."""
        errors, _, _ = check_handrolled.check(self.dir, self.dir / "tools/handrolled_banlist.toml")
        return [e for e in errors if stale or "matches nothing" not in e]


class RealTree(unittest.TestCase):
    def test_head_is_clean(self):
        errors, _, _ = check_handrolled.check(ROOT, BANLIST)
        self.assertEqual(errors, [])


class Gate(Tree):
    def test_every_ban_fires_outside_the_allowlist(self):
        for name, source in BANNED.items():
            with self.subTest(name):
                self.write(source)
                self.assertTrue(self.errors(), f"{name} was not caught")

    def test_vendor_comments_and_target_are_not_scanned(self):
        self.write("// fn percent_decode(text: &str) -> String\n")
        self.write(BANNED["percent-codec"], "vendor/x/src/lib.rs")
        self.write(BANNED["percent-codec"], "crates/demo/target/x.rs")
        self.assertEqual(self.errors(), [])

    def test_clean_code_passes(self):
        self.write("fn f() { let _ = hex::encode([1u8]); }\n")
        self.assertEqual(self.errors(), [])

    def test_ratchet_rejects_more_matches_than_allowed(self):
        path = "crates/branchyard-cli/src/adf.rs"
        self.write('fn a(b: u8) -> String { format!("%{b:02X}") }\n' * 2, path)
        self.assertTrue(any("permits 1" in e for e in self.errors()))

    def test_ratchet_demands_the_count_fall(self):
        path = "crates/branchyard-cli/src/adf.rs"
        self.write("fn a() {}\n", path)
        self.assertTrue(any("adf.rs matches nothing" in e for e in self.errors(stale=True)))

    def test_a_banlist_row_needs_a_reason(self):
        banlist = self.dir / "tools/handrolled_banlist.toml"
        banlist.write_text(
            banlist.read_text()
            + '\n[[allow]]\nban = "tar-format"\npath = "crates/demo/src/lib.rs"\ncount = 1\nreason = ""\n'
        )
        with self.assertRaises(SystemExit):
            check_handrolled.check(self.dir, banlist)


if __name__ == "__main__":
    unittest.main()
