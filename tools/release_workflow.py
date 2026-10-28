#!/usr/bin/env python3
"""Regenerate dist's workflow and require package smoke tests before publication."""

import argparse
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def require_smoke(text: str) -> str:
    """Gate dist's host job on the custom smoke job, including its result."""
    start = text.index("\n  host:\n")
    end = text.index("\n  custom-release-smoke:\n", start)
    host = text[start:end]
    dependency = "      - custom-release-extras\n"
    if "      - custom-release-smoke\n" not in host:
        host = host.replace(dependency, dependency + "      - custom-release-smoke\n", 1)
    condition = "needs.plan.result == 'success'"
    guarded = condition + " && needs.custom-release-smoke.result == 'success'"
    if guarded not in host:
        host = host.replace(condition, guarded, 1)
    if "      - custom-release-smoke\n" not in host or guarded not in host:
        raise ValueError("dist host job changed; review the release smoke dependency")
    return text[:start] + host + text[end:]


def main() -> None:
    """Generate pinned dist configuration and apply the publication guard."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dist", default="dist", help="pinned dist executable")
    parser.add_argument("--check", action="store_true", help="check the publication guard without generating")
    args = parser.parse_args()
    path = ROOT / ".github/workflows/release.yml"
    if not args.check:
        config = ROOT / "dist-workspace.toml"
        original = config.read_text()
        try:
            config.write_text(original.replace('allow-dirty = ["ci"]', "allow-dirty = []"))
            subprocess.run([args.dist, "generate", "--mode", "ci"], cwd=ROOT, check=True)
        finally:
            config.write_text(original)
    text = path.read_text()
    guarded = require_smoke(text)
    if args.check:
        if guarded != text:
            raise SystemExit("release workflow must require smoke success before publication")
    else:
        path.write_text(guarded)


if __name__ == "__main__":
    main()
