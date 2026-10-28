"""The workspace's patch of zstd 0.13 (`patches/zstd-0.13`, docs/ci-gates.md
"Supply chain"): Microsandbox's msb_krun_vmm asks for zstd 0.13, whose
zstd-safe 7 has RustSec advisories and no fixed 0.13 release, so
`[patch.crates-io]` puts in its place a zstd 0.13 that re-exports zstd 0.14.
These tests keep the lockfile on the patch and off the vulnerable releases,
and the patch a faithful stand-in: every zstd 0.13 feature, outside the
workspace.
"""

import tomllib
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PATCH = "patches/zstd-0.13"

# zstd 0.13.3's own features (its Cargo.toml on crates.io), each of which a
# crate depending on zstd 0.13 may turn on.
ZSTD_0_13_FEATURES = {
    "arrays",
    "bindgen",
    "debug",
    "default",
    "doc-cfg",
    "experimental",
    "fat-lto",
    "legacy",
    "no_asm",
    "pkg-config",
    "thin",
    "thin-lto",
    "wasm",
    "zdict_builder",
    "zstdmt",
}


def version(text):
    """`8.2.0` as (8, 2, 0); a `+build` suffix is not part of the order."""
    return tuple(int(part) for part in text.split("+")[0].split("."))


def load(path):
    return tomllib.loads((ROOT / path).read_text())


class ZstdPatch(unittest.TestCase):
    def setUp(self):
        self.lock = load("Cargo.lock")["package"]
        self.root = load("Cargo.toml")
        self.patch = load(f"{PATCH}/Cargo.toml")

    def packages(self, name):
        return [p for p in self.lock if p["name"] == name]

    def test_zstd_0_13_resolves_to_the_patch_and_nothing_else(self):
        self.assertEqual(self.root["patch"]["crates-io"]["zstd"], {"path": PATCH})
        old = [p for p in self.packages("zstd") if version(p["version"]) < (0, 14)]
        self.assertEqual(len(old), 1, old)
        # A path package has no source in the lockfile; crates.io's would.
        self.assertNotIn("source", old[0])
        self.assertEqual(old[0]["version"], self.patch["package"]["version"])

    def test_no_release_the_advisories_cover_is_in_the_lockfile(self):
        safe = self.packages("zstd-safe")
        self.assertTrue(safe)
        for package in safe:
            self.assertGreaterEqual(version(package["version"]), (8, 2, 0), package)
        for package in self.packages("zstd"):
            if "source" in package:
                self.assertGreaterEqual(version(package["version"]), (0, 14, 3), package)

    def test_the_patch_serves_zstd_0_13_from_zstd_0_14(self):
        upstream = self.patch["dependencies"]["upstream"]
        self.assertEqual(upstream["package"], "zstd")
        self.assertGreaterEqual(version(upstream["version"]), (0, 14, 3))
        self.assertIs(upstream["default-features"], False)
        lib = (ROOT / PATCH / "src/lib.rs").read_text()
        self.assertIn("pub use upstream::*;", lib)

    def test_every_zstd_0_13_feature_turns_on_its_zstd_0_14_namesake(self):
        features = self.patch["features"]
        self.assertEqual(set(features), ZSTD_0_13_FEATURES)
        self.assertEqual(features["default"], ["legacy", "arrays", "zdict_builder"])
        for name, enables in features.items():
            if name != "default":
                self.assertEqual(enables, [f"upstream/{name}"], name)

    def test_the_patch_is_not_a_workspace_member(self):
        workspace = self.root["workspace"]
        self.assertIn(PATCH, workspace["exclude"])
        self.assertNotIn(PATCH, workspace["members"])
        self.assertIs(self.patch["package"]["publish"], False)


if __name__ == "__main__":
    unittest.main()
