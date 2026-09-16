#!/usr/bin/env python3
"""Verify immutable upstream files against SHA-256 and Git blob IDs. No network."""
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def main():
    lock = json.loads((ROOT / "vendor.lock.json").read_text())
    seen = set()
    for entry in lock["files"]:
        rel = entry["path"]
        if rel in seen:
            raise ValueError(f"duplicate lock entry: {rel}")
        seen.add(rel)
        path = ROOT / rel
        if path.is_symlink() or not path.resolve().is_relative_to(ROOT / "vendor"):
            raise ValueError(f"unexpected vendor path: {rel}")
        data = path.read_bytes()
        actual = hashlib.sha256(data).hexdigest()
        blob = hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest()
        if actual != entry["sha256"] or blob != entry["git_blob"]:
            raise ValueError(f"modified upstream file: {rel}")
        if len(entry["commit"]) != 40 or entry["modified"]:
            raise ValueError(f"invalid upstream pin: {rel}")
    # Developer notes are kept outside vendor/ so every file here is pinned.
    actual_files = {str(p.relative_to(ROOT)) for p in (ROOT / "vendor").rglob("*")
                    if p.is_file() and "__pycache__" not in p.parts}
    if actual_files != seen:
        raise ValueError(f"untracked or missing vendor files: {actual_files ^ seen}")
    print(f"Verified {len(seen)} upstream files against both digests.")


if __name__ == "__main__":
    main()
