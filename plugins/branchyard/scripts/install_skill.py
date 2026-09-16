#!/usr/bin/env python3
"""Install the canonical standalone skill into an explicit host skill directory."""
import argparse
import json
from pathlib import Path
import shutil
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--destination", type=Path, required=True, help="Host's skill directory; branchyard is created beneath it")
    parser.add_argument("--apply", action="store_true", help="Copy; otherwise print a preview")
    args = parser.parse_args()
    source = Path(__file__).resolve().parents[1] / "skills" / "branchyard"
    target = args.destination.expanduser().resolve() / "branchyard"
    if target.exists() or target.is_symlink():
        print(json.dumps({"error": "destination_exists", "message": "Refusing to replace an existing skill."}), file=sys.stderr)
        return 2
    if args.apply:
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copytree(source, target)
    print(json.dumps({"applied": args.apply, "destination": str(target)}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
