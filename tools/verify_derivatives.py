#!/usr/bin/env python3
"""Require every derived source to match the upstream it records.

The emdash and Orca ports (patches/ports.json) are recorded like Scion's:
each derived file, Rust or generated catalog data, lists the vendored
sources it follows by upstream with their Git blob IDs, and its header must
name each upstream's repository, revision, license and copyright, and say
it was modified for Branchyard (Apache-2.0 section 4(b) for emdash).

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
import fnmatch
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


def _comment_header(path, lines=30):
    """The leading comment block of a Rust (`//`) or TOML (`#`) file, joined."""
    text = []
    for line in path.read_text().splitlines()[:lines]:
        stripped = line.lstrip()
        for marker in ("//", "#"):
            if stripped.startswith(marker):
                text.append(stripped[len(marker):].strip())
                break
    return " ".join(text)


def verify_ports(root=ROOT):
    manifest = json.loads((root / "patches/ports.json").read_text())
    lock = {e["path"]: e for e in json.loads((root / "vendor.lock.json").read_text())["files"]}
    upstreams = manifest["upstreams"]
    problems = []
    count = 0
    for derived in manifest["derivatives"]:
        path = root / derived["path"]
        if not path.is_file():
            problems.append(f"{derived['path']} is recorded but missing")
            continue
        header = _comment_header(path)
        named = derived.get("named_as", [])
        if "Modified for Branchyard" not in header:
            problems.append(f"{derived['path']}: its header does not say 'Modified for Branchyard'")
        for name, sources in derived["from"].items():
            upstream = upstreams[name]
            for needed in (upstream["repository"], upstream["commit"], upstream["license"],
                           upstream["copyright"]):
                if needed not in header:
                    problems.append(f"{derived['path']}: its header does not name {needed!r}")
            for source, recorded in sources.items():
                count += 1
                if source not in header and not any(fnmatch.fnmatch(source, g) for g in named):
                    problems.append(f"{derived['path']}: its header does not name {source}")
                entry = lock.get(f"{upstream['vendor']}/{source}")
                if entry is None or entry["commit"] != upstream["commit"]:
                    problems.append(f"{source} is not vendored at {upstream['commit']}")
                elif entry["git_blob"] != recorded:
                    problems.append(f"{source} changed upstream since {derived['path']} was "
                                    "derived: review the change, port it, and record the new blob")
    if problems:
        raise SystemExit("emdash and Orca ports out of date:\n  " + "\n  ".join(problems))
    print(f"Verified {len(manifest['derivatives'])} emdash and Orca derivatives against "
          f"{count} vendored sources.")


def main():
    verify_herdr()
    verify_scion()
    verify_ports()


if __name__ == "__main__":
    main()
