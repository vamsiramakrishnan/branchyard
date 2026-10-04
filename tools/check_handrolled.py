#!/usr/bin/env python3
"""Fail when first-party Rust re-implements a codec a vetted crate already provides.

The banned signatures and the ratchet live in tools/handrolled_banlist.toml:

  [[ban]]    id, pattern (a regex matched per code line), use (what to call instead)
  [[allow]]  ban, path, count, reason

A [[ban]] matches a line of non-comment Rust under crates/, tests/, sdk/ and
examples/ (never vendor/, patches/ or target/). Every match must be covered by
an [[allow]] for that ban and file. `count` is a ratchet: more matches than
`count` is a new hand-rolled codec, fewer means the allowlist is stale and
must be lowered (or the row deleted) in the same change, so the number can
only fall. Rows for other mechanisms are appended to the same file.

Usage: tools/check_handrolled.py [--root DIR] [--banlist FILE]
"""

import argparse
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCAN = ("crates", "tests", "sdk", "examples")
SKIP_DIRS = {"vendor", "patches", "target", ".git", "node_modules"}


def rust_files(root):
    for top in SCAN:
        base = root / top
        if not base.is_dir():
            continue
        for path in sorted(base.rglob("*.rs")):
            rel = path.relative_to(root)
            if SKIP_DIRS.intersection(rel.parts):
                continue
            yield rel, path


def load(banlist):
    data = tomllib.loads(Path(banlist).read_text())
    bans = {}
    for ban in data.get("ban", []):
        for key in ("id", "pattern", "use"):
            if not ban.get(key):
                raise SystemExit(f"{banlist}: a [[ban]] needs {key}: {ban}")
        if ban["id"] in bans:
            raise SystemExit(f"{banlist}: duplicate ban id {ban['id']}")
        bans[ban["id"]] = {**ban, "regex": re.compile(ban["pattern"])}
    allow = {}
    for row in data.get("allow", []):
        for key in ("ban", "path", "count", "reason"):
            if row.get(key) in (None, ""):
                raise SystemExit(f"{banlist}: an [[allow]] needs {key}: {row}")
        if row["ban"] not in bans:
            raise SystemExit(f"{banlist}: [[allow]] names unknown ban {row['ban']!r}")
        if not isinstance(row["count"], int) or row["count"] < 1:
            raise SystemExit(f"{banlist}: [[allow]] count must be a positive integer: {row}")
        key = (row["ban"], row["path"])
        if key in allow:
            raise SystemExit(f"{banlist}: duplicate [[allow]] for {key}")
        allow[key] = row
    return bans, allow


def code_lines(text):
    """(line number, line) for every line that is not a `//` comment."""
    for number, line in enumerate(text.splitlines(), 1):
        if line.lstrip().startswith("//"):
            continue
        yield number, line


def scan(root, bans):
    found = {}
    for rel, path in rust_files(root):
        text = path.read_text(errors="replace")
        for number, line in code_lines(text):
            for ban in bans.values():
                if ban["regex"].search(line):
                    found.setdefault((ban["id"], rel.as_posix()), []).append(number)
    return found


def check(root, banlist):
    bans, allow = load(banlist)
    found = scan(root, bans)
    errors = []
    for (ban_id, path), lines in sorted(found.items()):
        row = allow.get((ban_id, path))
        where = ", ".join(f"{path}:{n}" for n in lines)
        if row is None:
            errors.append(f"[{ban_id}] hand-rolled codec at {where}\n    use {bans[ban_id]['use']}")
        elif len(lines) > row["count"]:
            errors.append(
                f"[{ban_id}] {path} has {len(lines)} matches, the allowlist permits "
                f"{row['count']} ({where})\n    use {bans[ban_id]['use']}"
            )
        elif len(lines) < row["count"]:
            errors.append(
                f"[{ban_id}] {path} has {len(lines)} matches but the allowlist says "
                f"{row['count']}: lower the count (or delete the row) so it only falls"
            )
    for ban_id, path in sorted(allow):
        if (ban_id, path) not in found:
            errors.append(f"[{ban_id}] allowlist row for {path} matches nothing: delete it")
    return errors, found, allow


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--banlist", type=Path)
    args = parser.parse_args(argv)
    banlist = args.banlist or args.root / "tools" / "handrolled_banlist.toml"
    errors, _found, allow = check(args.root, banlist)
    if errors:
        print('Hand-rolled codecs (see CONTRIBUTING.md, "Use a vetted crate"):', file=sys.stderr)
        for error in errors:
            print(f"  {error}", file=sys.stderr)
        return 1
    remaining = sum(row["count"] for row in allow.values())
    print(
        f"No hand-rolled codecs outside the allowlist ({remaining} allowlisted "
        f"match{'es' if remaining != 1 else ''} left)."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
