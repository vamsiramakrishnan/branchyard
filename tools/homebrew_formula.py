#!/usr/bin/env python3
"""Fill packaging/homebrew/branchyard.rb.in from a release's SHA256SUMS.

    python3 tools/homebrew_formula.py --version 0.1.0 --sums SHA256SUMS > branchyard.rb

Fails when SHA256SUMS lacks an archive the formula names. Publishes
nothing. See docs/distribution.md.
"""
import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TEMPLATE = ROOT / "packaging/homebrew/branchyard.rb.in"
TARGETS = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-musl",
    "x86_64-unknown-linux-musl",
]


def render(version, sums_text, template=None):
    if not re.fullmatch(r"[0-9A-Za-z.+-]+", version):
        raise ValueError(f"unusable version {version!r}")
    sums = {}
    for line in sums_text.splitlines():
        parts = line.split()
        if len(parts) == 2 and re.fullmatch(r"[0-9a-f]{64}", parts[0]):
            sums[parts[1].lstrip("*")] = parts[0]
    text = template if template is not None else TEMPLATE.read_text()
    for target in TARGETS:
        archive = f"branchyard-{version}-{target}.tar.gz"
        if archive not in sums:
            raise ValueError(f"SHA256SUMS lists no {archive}")
        text = text.replace("@SHA256_" + target.upper().replace("-", "_") + "@", sums[archive])
    text = text.replace("@VERSION@", version)
    left = re.findall(r"@[A-Z0-9_]+@", text)
    if left:
        raise ValueError(f"unfilled placeholders: {sorted(set(left))}")
    return text


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--version", required=True)
    parser.add_argument("--sums", required=True, type=Path)
    args = parser.parse_args()
    try:
        sys.stdout.write(render(args.version.lstrip("v"), args.sums.read_text()))
    except ValueError as error:
        raise SystemExit(f"homebrew_formula.py: {error}")


if __name__ == "__main__":
    main()
