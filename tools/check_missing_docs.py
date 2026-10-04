#!/usr/bin/env python3
"""A counted ratchet on undocumented public items.

`missing_docs` cannot be denied across the workspace yet: it still finds
undocumented public items in most library crates. This runs `cargo check` over
every library with the lint on and compares, per crate, with the counts in
tools/missing_docs_ratchet.json:

- a crate over its count fails (new public API needs rustdoc);
- a crate under its count fails too, until you lower the file with --write, so
  the counts can only fall and the improvement is kept;
- a crate not listed has a count of zero.

A crate that reaches zero should also carry `#![warn(missing_docs)]` in its
lib.rs, so editors and Clippy report the item where it is written. The build
uses its own target directory (target/missing-docs) so it does not disturb the
main one.

    python3 tools/check_missing_docs.py           check against the ratchet
    python3 tools/check_missing_docs.py --write   record the current counts
"""
import json
import os
import subprocess
import sys
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
RATCHET = ROOT / "tools" / "missing_docs_ratchet.json"


def count():
    env = dict(os.environ)
    env["RUSTFLAGS"] = (env.get("RUSTFLAGS", "") + " -W missing_docs").strip()
    env["CARGO_TARGET_DIR"] = str(Path(env.get("CARGO_TARGET_DIR", ROOT / "target")) / "missing-docs")
    command = ["cargo", "check", "--workspace", "--lib", "--locked", "--offline",
               "--message-format=json"]
    done = subprocess.run(command, cwd=ROOT, env=env, capture_output=True, text=True)
    if done.returncode != 0:
        sys.exit(f"cargo check failed:\n{done.stderr[-2000:]}")
    seen, counts = set(), Counter()
    for line in done.stdout.splitlines():
        try:
            message = json.loads(line)
        except ValueError:
            continue
        if message.get("reason") != "compiler-message":
            continue
        diagnostic = message["message"]
        if (diagnostic.get("code") or {}).get("code") != "missing_docs":
            continue
        package = message["package_id"]
        name = package.rsplit("/", 1)[-1].split("#")[0] if "#" in package else package.split()[0]
        span = next((s for s in diagnostic["spans"] if s["is_primary"]), diagnostic["spans"][0])
        key = (name, span["file_name"], span["line_start"], span["column_start"])
        if key not in seen:
            seen.add(key)
            counts[name] += 1
    return dict(sorted(counts.items()))


def main():
    write = "--write" in sys.argv[1:]
    counts = count()
    if write:
        RATCHET.write_text(json.dumps(counts, indent=2) + "\n")
        print(f"missing_docs: recorded {sum(counts.values())} undocumented public items in {len(counts)} crates.")
        return
    recorded = json.loads(RATCHET.read_text())
    problems = []
    for name in sorted(set(counts) | set(recorded)):
        now, before = counts.get(name, 0), recorded.get(name, 0)
        if now > before:
            problems.append(f"{name}: {now} undocumented public items, up from {before}; document the new ones")
        elif now < before:
            problems.append(f"{name}: down to {now} from {before}; lower it with "
                            "python3 tools/check_missing_docs.py --write")
    if problems:
        sys.exit("missing_docs ratchet:\n" + "\n".join(f"  {p}" for p in problems))
    print(f"missing_docs: {sum(counts.values())} undocumented public items, none more than recorded.")


if __name__ == "__main__":
    main()
