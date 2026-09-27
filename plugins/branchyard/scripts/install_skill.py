#!/usr/bin/env python3
"""Install the canonical delegate skill into a harness's skill directory.

Both the Claude Code and Codex plugin manifests point at
`plugins/branchyard/skills/delegate`; this installs an exact copy of it
for a skill-capable host that has no plugin loader of its own. See
`docs/distribution.md`.
"""
import argparse
import filecmp
import json
from pathlib import Path
import shutil
import sys

SKILL_NAME = "delegate"


def _tree_matches(a: Path, b: Path) -> bool:
    """Whether `a` and `b` hold the same files with the same bytes."""
    comparison = filecmp.dircmp(a, b)
    if comparison.left_only or comparison.right_only or comparison.diff_files or comparison.funny_files:
        return False
    return all(_tree_matches(a / name, b / name) for name in comparison.common_dirs)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--destination",
        type=Path,
        required=True,
        help="Host's skill directory; 'delegate' is created beneath it",
    )
    parser.add_argument("--apply", action="store_true", help="Copy; otherwise print a preview")
    parser.add_argument(
        "--force",
        action="store_true",
        help="Replace an existing install whose contents differ from the canonical skill",
    )
    args = parser.parse_args()
    source = Path(__file__).resolve().parents[1] / "skills" / SKILL_NAME
    target = args.destination.expanduser().resolve() / SKILL_NAME

    replacing = False
    if target.is_symlink() or (target.exists() and not target.is_dir()):
        print(
            json.dumps(
                {
                    "error": "destination_exists",
                    "message": f"{target} exists and is not a plain directory; refusing to replace it.",
                }
            ),
            file=sys.stderr,
        )
        return 2
    if target.exists():
        if _tree_matches(source, target):
            print(json.dumps({"applied": False, "destination": str(target), "unchanged": True}))
            return 0
        if not args.force:
            print(
                json.dumps(
                    {
                        "error": "destination_exists",
                        "message": (
                            f"{target} already holds a different skill install; "
                            "pass --force to replace it."
                        ),
                    }
                ),
                file=sys.stderr,
            )
            return 2
        replacing = True

    if args.apply:
        if replacing:
            shutil.rmtree(target)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copytree(source, target)
    print(json.dumps({"applied": args.apply, "destination": str(target), "replaced": replacing}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
