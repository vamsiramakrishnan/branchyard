#!/usr/bin/env python3
"""Require every derived source to match the upstream it records.

Herdr's resume recipes are extracted Rust: the checked-in patch must be the
exact diff from the vendored file. Scion's provisioners are translated from
Python to Rust, where a line diff between languages would record nothing
reviewable; patches/scion-provision.json instead records, for each derived
file, the vendored upstream files it follows and their Git blob IDs. This
fails when a derived file's header does not name its origin, revision,
sources, license and modification, or when a vendored source changed since
the translation was reviewed.
"""
import difflib
import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
UPSTREAM = "vendor/herdr/src/agent_resume.rs"
DERIVED = "crates/branchyard-controls/src/resume.rs"


def verify_herdr():
    expected = "".join(difflib.unified_diff(
        (ROOT / UPSTREAM).read_text().splitlines(True),
        (ROOT / DERIVED).read_text().splitlines(True),
        fromfile=UPSTREAM, tofile=DERIVED,
    ))
    actual = (ROOT / "patches/herdr-resume.patch").read_text()
    if actual != expected:
        raise SystemExit("Herdr adaptation changed: regenerate and review patches/herdr-resume.patch")
    print("Verified Herdr extraction provenance patch.")


def _header(path):
    header = "".join(line for line in path.read_text().splitlines(True)[:20]
                     if line.startswith("//"))
    return re.sub(r"\s*\n//\s*", " ", header)


def verify_scion():
    manifest = json.loads((ROOT / "patches/scion-provision.json").read_text())
    commit, prefix = manifest["commit"], manifest["vendor"]
    lock = {e["path"]: e for e in json.loads((ROOT / "vendor.lock.json").read_text())["files"]}
    problems = []
    for derived in manifest["derivatives"]:
        path = ROOT / derived["path"]
        header = _header(path)
        for needed in ("GoogleCloudPlatform/scion", commit, "Apache License, Version 2.0",
                       "Modified for Branchyard"):
            if needed not in header:
                problems.append(f"{derived['path']}: its header does not name {needed!r}")
        for source, recorded in derived["from"].items():
            if source not in header:
                problems.append(f"{derived['path']}: its header does not name {source}")
            entry = lock.get(f"{prefix}/{source}")
            if entry is None or entry["commit"] != commit:
                problems.append(f"{source} is not vendored at {commit}")
            # The upstream pin, not the file: a local patch to the vendored
            # copy (vendor.patches.json) does not change what the
            # translation follows, and verify_vendor.py checks an
            # unpatched file against its pin.
            elif entry["git_blob"] != recorded:
                problems.append(f"{source} changed upstream since {derived['path']} was "
                                "translated: review the change, port it, and record the new blob")
    # A `rewritten` file was once translated line for line from these
    # sources, like a `derivatives` entry, but has since been rebuilt on a
    # real library instead of following them: its header must still credit
    # Scion, but a vendored source changing no longer means it needs
    # review, so (unlike `derivatives`) its blob is not tracked.
    for rewritten in manifest.get("rewritten", []):
        path = ROOT / rewritten["path"]
        header = _header(path)
        for needed in ("GoogleCloudPlatform/scion", commit, "Apache License, Version 2.0",
                       "Rewritten for Branchyard"):
            if needed not in header:
                problems.append(f"{rewritten['path']}: its header does not name {needed!r}")
        for source in rewritten.get("originally_from", []):
            if source not in header:
                problems.append(f"{rewritten['path']}: its header does not name {source}")
    if problems:
        raise SystemExit("Scion derivatives out of date:\n  " + "\n  ".join(problems))
    count = sum(len(d["from"]) for d in manifest["derivatives"])
    rewritten = len(manifest.get("rewritten", []))
    print(f"Verified {len(manifest['derivatives'])} Scion derivatives against {count} "
          f"vendored sources at {commit[:7]}, and {rewritten} rewritten file(s)' attribution.")


def main():
    verify_herdr()
    verify_scion()


if __name__ == "__main__":
    main()
