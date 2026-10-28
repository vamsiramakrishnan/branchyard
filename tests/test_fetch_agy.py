"""Verify the release image's agy download boundary without a network call."""

import hashlib
import importlib.util
import io
import json
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("fetch_agy", ROOT / "tools/fetch_agy.py")
assert spec and spec.loader
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class DownloadTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.archive = self.root / "release.tar.gz"
        self.manifest = self.root / "manifest.json"
        self.output = self.root / "agy"

    def payload(self, names=("antigravity",)):
        with tarfile.open(self.archive, "w:gz") as archive:
            for name in names:
                member = tarfile.TarInfo(name)
                member.size = 6
                archive.addfile(member, io.BytesIO(b"binary"))
        self.manifest.write_text(
            json.dumps(
                {
                    "url": "https://storage.googleapis.com/antigravity-public/fixture.tar.gz",
                    "sha512": hashlib.sha512(self.archive.read_bytes()).hexdigest(),
                }
            )
        )

    def fetch(self):
        def download(_url, destination):
            Path(destination).write_bytes(self.archive.read_bytes())

        with mock.patch.object(module.urllib.request, "urlretrieve", side_effect=download):
            module.fetch(self.manifest, self.output)

    def test_verified_binary_is_extracted_with_executable_mode(self):
        self.payload()
        self.fetch()
        self.assertEqual(self.output.read_bytes(), b"binary")
        self.assertEqual(self.output.stat().st_mode & 0o777, 0o755)

    def test_checksum_failure_writes_no_executable(self):
        self.payload()
        self.archive.write_bytes(self.archive.read_bytes() + b"changed")
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            self.fetch()
        self.assertFalse(self.output.exists())

    def test_missing_or_ambiguous_payload_is_refused(self):
        for names in [("README",), ("agy", "antigravity")]:
            with self.subTest(names=names):
                self.payload(names)
                with self.assertRaisesRegex(ValueError, "exactly one"):
                    self.fetch()
                self.assertFalse(self.output.exists())

    def test_arbitrary_distribution_origin_is_refused(self):
        self.manifest.write_text(json.dumps({"url": "http://example.invalid/archive"}))
        with self.assertRaisesRegex(ValueError, "official HTTPS"):
            self.fetch()

    def test_help_needs_no_download(self):
        result = subprocess.run([sys.executable, str(ROOT / "tools/fetch_agy.py"), "--help"], capture_output=True)
        self.assertEqual(result.returncode, 0)
        self.assertIn(b"--manifest", result.stdout)


if __name__ == "__main__":
    unittest.main()
