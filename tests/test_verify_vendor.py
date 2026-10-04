#!/usr/bin/env python3
"""tools/verify_vendor.py against small fixture trees: pins, recorded
patches, stale or incomplete patch entries, and the Warp license boundary."""

import hashlib
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
import verify_vendor

COMMIT = "a" * 40


def pin(path, data, license="Apache-2.0"):
    return {
        "path": path,
        "upstream": "https://example.com/up",
        "commit": COMMIT,
        "upstream_path": path.split("/", 2)[2],
        "git_blob": hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest(),
        "sha256": hashlib.sha256(data).hexdigest(),
        "license": license,
        "use": "reference",
        "modified": False,
    }


class VerifyVendor(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.files = {
            "vendor/up/a.txt": (b"upstream\n", "Apache-2.0"),
            "vendor/warp-agpl/w.rs": (b"fn warp() {}\n", "AGPL-3.0-only"),
        }
        for rel, (data, _) in self.files.items():
            (self.root / rel).parent.mkdir(parents=True, exist_ok=True)
            (self.root / rel).write_bytes(data)
        self.write_lock()
        self.write_patches([])
        (self.root / "Cargo.toml").write_text('[workspace]\nmembers = ["crates/c"]\n')
        (self.root / "crates/c/src").mkdir(parents=True)
        (self.root / "crates/c/src/lib.rs").write_text("// Apache-2.0\n")

    def tearDown(self):
        self.tmp.cleanup()

    def write_lock(self):
        lock = {"format": 1, "files": [pin(rel, data, lic) for rel, (data, lic) in self.files.items()]}
        (self.root / "vendor.lock.json").write_text(json.dumps(lock))

    def write_patches(self, patches):
        (self.root / "vendor.patches.json").write_text(json.dumps({"format": 1, "patches": patches}))

    def problems(self):
        return verify_vendor.verify(self.root)[0]

    def assert_problem(self, needle):
        problems = self.problems()
        self.assertTrue(any(needle in p for p in problems), problems)

    def test_a_clean_snapshot_passes(self):
        self.assertEqual(verify_vendor.verify(self.root), ([], 2, 0))

    def test_an_unrecorded_change_fails(self):
        (self.root / "vendor/up/a.txt").write_bytes(b"patched\n")
        self.assert_problem("modified upstream file: vendor/up/a.txt")

    def test_a_recorded_patch_passes(self):
        (self.root / "vendor/up/a.txt").write_bytes(b"patched\n")
        self.write_patches([{"path": "vendor/up/a.txt", "reason": "fix a typo", "upstream_commit": COMMIT}])
        self.assertEqual(verify_vendor.verify(self.root), ([], 2, 1))

    def test_patch_entries_must_be_complete_current_and_known(self):
        self.write_patches([{"path": "vendor/up/a.txt", "reason": "x", "upstream_commit": COMMIT}])
        self.assert_problem("listed as patched but matches its pin")
        (self.root / "vendor/up/a.txt").write_bytes(b"patched\n")
        self.write_patches([{"path": "vendor/up/a.txt", "reason": " ", "upstream_commit": "b" * 40}])
        self.assert_problem("needs a reason")
        self.assert_problem("is pinned at " + COMMIT)
        self.write_patches([{"path": "vendor/up/a.txt", "reason": "x", "upstream_commit": "abc"}])
        self.assert_problem("full 40-character commit")
        self.write_patches([{"path": "vendor/up/nope.txt", "reason": "x", "upstream_commit": COMMIT}])
        self.assert_problem("which vendor.lock.json does not pin")
        (self.root / "vendor.patches.json").unlink()
        self.assert_problem("vendor.patches.json is missing")

    def test_every_file_is_pinned(self):
        (self.root / "vendor/up/new.txt").write_text("mine\n")
        self.assert_problem("untracked or missing vendor files")

    def test_the_warp_license_boundary(self):
        (self.root / "crates/c/Cargo.toml").write_text('[dependencies]\nwarp = { path = "../../vendor/warp-agpl" }\n')
        self.assert_problem("crates/c/Cargo.toml refers to vendor/warp-agpl")
        (self.root / "crates/c/Cargo.toml").unlink()
        (self.root / "crates/c/src/lib.rs").write_text('include!("../../../vendor/warp-agpl/w.rs");\n')
        self.assert_problem("crates/c/src/lib.rs refers to vendor/warp-agpl")
        (self.root / "crates/c/src/lib.rs").write_text("\n")
        (self.root / "Cargo.toml").write_text('[workspace]\nmembers = ["crates/c", "vendor/warp-agpl"]\n')
        self.assert_problem("no workspace member may live under vendor/")
        (self.root / "Cargo.toml").write_text('[workspace]\nmembers = ["crates/c"]\n')
        self.files["vendor/up/a.txt"] = (b"upstream\n", "AGPL-3.0-only")
        self.write_lock()
        self.assert_problem("vendor/up/a.txt: AGPL files belong under vendor/warp-agpl/")

    def test_the_repository_itself_passes(self):
        problems, pinned, _ = verify_vendor.verify()
        self.assertEqual(problems, [])
        self.assertGreater(pinned, 0)


if __name__ == "__main__":
    unittest.main()
