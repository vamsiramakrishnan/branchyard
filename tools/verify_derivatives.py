#!/usr/bin/env python3
"""Require the recorded adaptation patch to describe the exact derived source."""
import difflib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
UPSTREAM = "vendor/herdr/src/agent_resume.rs"
DERIVED = "crates/branchyard-controls/src/resume.rs"


def main():
    expected = "".join(difflib.unified_diff(
        (ROOT / UPSTREAM).read_text().splitlines(True),
        (ROOT / DERIVED).read_text().splitlines(True),
        fromfile=UPSTREAM, tofile=DERIVED,
    ))
    actual = (ROOT / "patches/herdr-resume.patch").read_text()
    if actual != expected:
        raise SystemExit("Herdr adaptation changed: regenerate and review patches/herdr-resume.patch")
    print("Verified Herdr extraction provenance patch.")


if __name__ == "__main__":
    main()
