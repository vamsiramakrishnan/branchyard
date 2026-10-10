#!/usr/bin/env python3
"""tools/vendor_pin.py against a temporary upstream git repository: files
land under vendor/<dir>/<upstream path> with the digests the lock records,
the result passes tools/verify_vendor.py, a re-pin reports the blobs that
changed, and bad input is refused before anything is written."""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
import vendor_pin
import verify_vendor


def git(repo: Path, *args: str) -> str:
    done = subprocess.run(
        ["git", "-C", str(repo), *args],
        capture_output=True,
        check=True,
        text=True,
        env={
            "GIT_AUTHOR_NAME": "t",
            "GIT_AUTHOR_EMAIL": "t@x",
            "GIT_COMMITTER_NAME": "t",
            "GIT_COMMITTER_EMAIL": "t@x",
            "PATH": "/usr/bin:/bin",
        },
    )
    return done.stdout.strip()


class VendorPin(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        base = Path(self.tmp.name)
        self.upstream = base / "upstream"
        self.upstream.mkdir()
        git(self.upstream, "init", "-q", "-b", "main")
        (self.upstream / "LICENSE").write_text("MIT License\n")
        (self.upstream / "src").mkdir()
        (self.upstream / "src/a.txt").write_text("one\n")
        (self.upstream / "src/b.txt").write_text("same\n")
        git(self.upstream, "add", ".")
        git(self.upstream, "commit", "-q", "-m", "first")
        self.first = git(self.upstream, "rev-parse", "HEAD")
        (self.upstream / "src/a.txt").write_text("two\n")
        git(self.upstream, "add", ".")
        git(self.upstream, "commit", "-q", "-m", "second")
        self.second = git(self.upstream, "rev-parse", "HEAD")
        self.root = base / "root"
        (self.root / "vendor").mkdir(parents=True)
        (self.root / "vendor.lock.json").write_text(
            json.dumps({"format": 1, "retrieved_at": "2026-10-10", "files": []})
        )
        (self.root / "vendor.patches.json").write_text(json.dumps({"format": 1, "patches": []}))
        # The verifier reads the workspace manifest for the Warp boundary check.
        (self.root / "Cargo.toml").write_text('[workspace]\nmembers = ["crates/c"]\n')
        self.source = vendor_pin.GitSource(self.upstream)

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def lock(self) -> dict:
        return json.loads((self.root / "vendor.lock.json").read_text())

    def test_pinned_files_are_written_and_recorded_so_the_verifier_accepts_them(self) -> None:
        report = vendor_pin.pin(
            self.root,
            "vendor/up",
            "https://github.com/o/r",
            self.first,
            ["LICENSE", "src/a.txt"],
            "MIT",
            "reference-only",
            self.source,
        )
        self.assertEqual(report.added, ["vendor/up/LICENSE", "vendor/up/src/a.txt"])
        self.assertEqual((self.root / "vendor/up/src/a.txt").read_text(), "one\n")
        lock = self.lock()
        self.assertEqual(lock["retrieved_at"], "2026-10-10")
        entry = next(e for e in lock["files"] if e["path"] == "vendor/up/src/a.txt")
        self.assertEqual(list(entry), list(vendor_pin.FIELDS))
        self.assertEqual(entry["commit"], self.first)
        self.assertEqual(entry["upstream_path"], "src/a.txt")
        self.assertEqual(entry["git_blob"], git(self.upstream, "rev-parse", f"{self.first}:src/a.txt"))
        self.assertEqual(entry["sha256"], vendor_pin.digests(b"one\n")[0])
        self.assertEqual((entry["license"], entry["use"], entry["modified"]), ("MIT", "reference-only", False))
        problems = verify_vendor.verify(self.root)[0]
        self.assertEqual(problems, [])

    def test_a_repin_fetches_the_new_blobs_and_reports_what_changed(self) -> None:
        vendor_pin.pin(
            self.root,
            "vendor/up",
            "https://github.com/o/r",
            self.first,
            ["LICENSE", "src/a.txt", "src/b.txt"],
            "MIT",
            "upstream-data",
            self.source,
        )
        report = vendor_pin.repin(self.root, "vendor/up", self.second, self.source)
        self.assertEqual(report.changed, ["vendor/up/src/a.txt"])
        self.assertEqual(sorted(report.unchanged), ["vendor/up/LICENSE", "vendor/up/src/b.txt"])
        self.assertEqual((self.root / "vendor/up/src/a.txt").read_text(), "two\n")
        for entry in self.lock()["files"]:
            self.assertEqual(entry["commit"], self.second)
            self.assertEqual(entry["use"], "upstream-data")
        self.assertEqual(verify_vendor.verify(self.root)[0], [])
        # Pinning again at the same commit changes nothing and says so.
        again = vendor_pin.pin(
            self.root,
            "vendor/up",
            "https://github.com/o/r",
            self.second,
            ["src/a.txt"],
            "MIT",
            "upstream-data",
            self.source,
        )
        self.assertEqual((again.added, again.changed, again.unchanged), ([], [], ["vendor/up/src/a.txt"]))

    def test_bad_input_is_refused_before_anything_is_written(self) -> None:
        with self.assertRaises(ValueError):
            vendor_pin.pin(self.root, "vendor/up", "u", "abc", ["LICENSE"], "MIT", "x", self.source)
        with self.assertRaises(ValueError):
            vendor_pin.pin(self.root, "lib/up", "u", self.first, ["LICENSE"], "MIT", "x", self.source)
        with self.assertRaises(ValueError):
            vendor_pin.pin(self.root, "vendor/up", "u", self.first, ["../etc/passwd"], "MIT", "x", self.source)
        with self.assertRaises(LookupError):
            vendor_pin.pin(self.root, "vendor/up", "u", self.first, ["LICENSE", "missing.txt"], "MIT", "x", self.source)
        with self.assertRaises(ValueError):
            vendor_pin.repin(self.root, "vendor/none", self.first, self.source)
        self.assertEqual(self.lock()["files"], [])
        self.assertFalse((self.root / "vendor/up/LICENSE").exists() and (self.root / "vendor/up/missing.txt").exists())

    def test_the_raw_source_names_github_files_at_the_commit(self) -> None:
        raw = vendor_pin.RawSource("https://github.com/coder/httpjail.git")
        self.assertEqual(
            raw.url("a" * 40, "src/rules.rs"),
            "https://raw.githubusercontent.com/coder/httpjail/" + "a" * 40 + "/src/rules.rs",
        )
        with self.assertRaises(ValueError):
            vendor_pin.RawSource("https://example.com/x")

    def test_the_command_line_pins_and_repins(self) -> None:
        argv = [
            "--root", str(self.root), "--dir", "vendor/up", "--upstream", "https://github.com/o/r",
            "--commit", self.first, "--license", "MIT", "--use", "reference-only", "--git", str(self.upstream),
            "LICENSE", "src/a.txt",
        ]  # fmt: skip
        self.assertEqual(vendor_pin.main(argv), 0)
        self.assertEqual(
            vendor_pin.main(
                [
                    "--root",
                    str(self.root),
                    "--dir",
                    "vendor/up",
                    "--commit",
                    self.second,
                    "--git",
                    str(self.upstream),
                    "--repin",
                ]
            ),
            0,
        )
        self.assertEqual((self.root / "vendor/up/src/a.txt").read_text(), "two\n")
        self.assertEqual(
            vendor_pin.main(
                [
                    "--root",
                    str(self.root),
                    "--dir",
                    "vendor/up",
                    "--commit",
                    "nope",
                    "--git",
                    str(self.upstream),
                    "--repin",
                ]
            ),
            1,
        )


if __name__ == "__main__":
    unittest.main()
