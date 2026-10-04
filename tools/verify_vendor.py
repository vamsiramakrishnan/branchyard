#!/usr/bin/env python3
"""Verify vendor/ against its pins and its recorded local patches. No network.

vendor/ is a pinned reference snapshot of upstream files. vendor.lock.json
pins each one to an upstream commit with its Git blob ID and SHA-256, as
fetched. A file may carry a local patch, but only when vendor.patches.json
lists it with the reason and the upstream commit the patch applies to. So:

- every file under vendor/ is pinned, and every pin names a file there;
- a file not listed as patched must still match both of its pin's digests;
- a file listed as patched must differ from its pin (else the entry is stale
  and must go), name the commit it is pinned at, and give a reason;
- the Warp license boundary holds: AGPL files stay under vendor/warp-agpl/,
  no workspace member lives under vendor/, and no Rust or Cargo file outside
  vendor/ refers to vendor/warp-agpl (no path dependency, `include!` or
  `#[path]` can pull Warp code into an Apache-licensed crate).
"""

import hashlib
import json
import os
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WARP = "vendor/warp-agpl/"
COMMIT = re.compile(r"[0-9a-f]{40}")


def digests(data):
    sha256 = hashlib.sha256(data).hexdigest()
    blob = hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest()
    return sha256, blob


def load_patches(root, problems):
    path = root / "vendor.patches.json"
    if not path.exists():
        problems.append("vendor.patches.json is missing; it lists local patches (it may be empty)")
        return {}
    manifest = json.loads(path.read_text())
    if manifest.get("format") != 1:
        problems.append("vendor.patches.json: format must be 1")
    patches = {}
    for entry in manifest.get("patches", []):
        rel = entry.get("path", "")
        if rel in patches:
            problems.append(f"vendor.patches.json lists {rel} twice")
        patches[rel] = entry
        if not str(entry.get("reason", "")).strip():
            problems.append(f"{rel}: a patched file needs a reason in vendor.patches.json")
        if not COMMIT.fullmatch(str(entry.get("upstream_commit", ""))):
            problems.append(f"{rel}: upstream_commit must be a full 40-character commit")
    return patches


def sources(root):
    """Rust and Cargo files outside vendor/ and build output."""
    for dirpath, dirnames, filenames in os.walk(root):
        # Hidden directories hold VCS data, tool state and other checkouts.
        dirnames[:] = [
            d
            for d in dirnames
            if d not in ("target", "node_modules", "__pycache__")
            and not d.startswith(".")
            and not (Path(dirpath) == root and d == "vendor")
        ]
        for name in filenames:
            if name.endswith((".rs", ".toml")):
                yield Path(dirpath) / name


def verify_boundary(root, lock, problems):
    for entry in lock["files"]:
        if "AGPL" in entry.get("license", "") and not entry["path"].startswith(WARP):
            problems.append(f"{entry['path']}: AGPL files belong under {WARP}")
    cargo = (root / "Cargo.toml").read_text()
    members = re.search(r"members\s*=\s*\[(.*?)\]", cargo, re.S)
    if members and "vendor/" in members.group(1):
        problems.append("Cargo.toml: no workspace member may live under vendor/")
    for path in sorted(sources(root)):
        rel = path.relative_to(root).as_posix()
        if "vendor/warp-agpl" in path.read_text(errors="replace"):
            problems.append(
                f"{rel} refers to vendor/warp-agpl: Warp's AGPL code must not reach an Apache-licensed file"
            )


def verify(root=ROOT):
    """Problems found under `root`, and counts of (pinned, patched) files."""
    problems = []
    lock = json.loads((root / "vendor.lock.json").read_text())
    patches = load_patches(root, problems)
    seen = set()
    patched = 0
    for entry in lock["files"]:
        rel = entry["path"]
        if rel in seen:
            problems.append(f"duplicate lock entry: {rel}")
        seen.add(rel)
        path = root / rel
        if path.is_symlink() or not path.resolve().is_relative_to(root / "vendor"):
            problems.append(f"unexpected vendor path: {rel}")
            continue
        if not COMMIT.fullmatch(entry["commit"]) or entry["modified"]:
            # The pin describes upstream as fetched; a local change is a patch.
            problems.append(f"invalid upstream pin: {rel}")
        if not path.is_file():
            continue
        matches = digests(path.read_bytes()) == (entry["sha256"], entry["git_blob"])
        patch = patches.get(rel)
        if patch is None and not matches:
            problems.append(
                f"modified upstream file: {rel}; record the patch in "
                "vendor.patches.json with its reason and upstream commit, or restore it"
            )
        elif patch is not None:
            patched += 1
            if matches:
                problems.append(f"{rel} is listed as patched but matches its pin; remove its vendor.patches.json entry")
            if patch.get("upstream_commit") != entry["commit"]:
                problems.append(
                    f"{rel}: the patch names upstream commit "
                    f"{patch.get('upstream_commit')}, but the file is pinned at "
                    f"{entry['commit']}"
                )
    for rel in patches:
        if rel not in seen:
            problems.append(f"vendor.patches.json lists {rel}, which vendor.lock.json does not pin")
    # Developer notes are kept outside vendor/ so every file here is pinned.
    actual = {
        p.relative_to(root).as_posix()
        for p in (root / "vendor").rglob("*")
        if p.is_file() and "__pycache__" not in p.parts
    }
    if actual != seen:
        problems.append(f"untracked or missing vendor files: {sorted(actual ^ seen)}")
    verify_boundary(root, lock, problems)
    return problems, len(seen), patched


def main():
    problems, pinned, patched = verify()
    if problems:
        sys.exit("vendor/ does not match its pins and patches:\n  " + "\n  ".join(problems))
    print(
        f"Verified {pinned} vendored files: {pinned - patched} match their upstream pins, "
        f"{patched} carry recorded patches; the Warp license boundary holds."
    )


if __name__ == "__main__":
    main()
