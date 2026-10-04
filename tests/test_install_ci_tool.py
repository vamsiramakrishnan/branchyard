#!/usr/bin/env python3
"""tools/install_ci_tool.py: a pinned digest is enforced, and the binary lands executable."""

import hashlib
import io
import re
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
import install_ci_tool


def tarball(member: str, data: bytes) -> bytes:
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode="w:gz") as tar:
        info = tarfile.TarInfo(member)
        info.size = len(data)
        tar.addfile(info, io.BytesIO(data))
    return out.getvalue()


class InstallCiTool(unittest.TestCase):
    def test_every_pin_is_a_well_formed_digest_and_url(self):
        for name, (url, digest, member) in install_ci_tool.TOOLS.items():
            self.assertRegex(digest, r"^[0-9a-f]{64}$", name)
            self.assertTrue(url.startswith("https://"), name)
            self.assertTrue(url.endswith(".tar.gz"), name)
            self.assertTrue(Path(member).name.startswith(name.split("-")[0]), name)
            self.assertIsNotNone(re.search(r"\d+\.\d+\.\d+", url), f"{name}: the URL names its version")

    def test_a_matching_asset_is_installed_executable(self):
        archive = tarball("cargo-deny", b"#!/bin/sh\n")
        pins = {"cargo-deny": ("https://example.com/x.tar.gz", hashlib.sha256(archive).hexdigest(), "cargo-deny")}
        original = install_ci_tool.TOOLS
        install_ci_tool.TOOLS = pins
        try:
            with tempfile.TemporaryDirectory() as dest:
                target = install_ci_tool.install("cargo-deny", Path(dest) / "bin", fetch=lambda _: archive)
                self.assertEqual(target.read_bytes(), b"#!/bin/sh\n")
                self.assertTrue(target.stat().st_mode & 0o100)
        finally:
            install_ci_tool.TOOLS = original

    def test_a_replaced_asset_is_refused_and_nothing_is_written(self):
        pinned = tarball("cargo-deny", b"good")
        evil = tarball("cargo-deny", b"evil")
        pins = {"cargo-deny": ("https://example.com/x.tar.gz", hashlib.sha256(pinned).hexdigest(), "cargo-deny")}
        original = install_ci_tool.TOOLS
        install_ci_tool.TOOLS = pins
        try:
            with tempfile.TemporaryDirectory() as dest:
                with self.assertRaises(SystemExit) as raised:
                    install_ci_tool.install("cargo-deny", Path(dest) / "bin", fetch=lambda _: evil)
                self.assertIn("refusing to install", str(raised.exception))
                self.assertFalse((Path(dest) / "bin").exists())
        finally:
            install_ci_tool.TOOLS = original


if __name__ == "__main__":
    unittest.main()
