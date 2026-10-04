#!/usr/bin/env python3
"""Keep error discipline machine-enforced: the lint table, and a ratchet on its exceptions.

The workspace denies, through Clippy (Cargo.toml `[workspace.lints]`, shared by
every crate with `[lints] workspace = true`): `let _ =` on a must-use value,
`unwrap`, `expect`, `panic!`, `unwrap_in_result`, `map_unwrap_or`,
`too_many_arguments`, `todo!`, `unimplemented!` and `dbg!`. Code that predates
the table carries a targeted `#[allow(clippy::..)]` (an item, or a whole file)
with a `// ratchet: <crate>` marker on the same line as the attribute's end.
This script, no build needed:

1. counts the markers per crate against tools/lint_ratchet.json. A count may
   not rise; when it falls the file must be lowered (`--update`) so the gain
   is kept. A crate not listed has a count of zero.
2. requires every crates/*/Cargo.toml to say `[lints]` `workspace = true`
   and to inherit `rust-version`.
3. requires the root Cargo.toml to deny every lint above, and its
   `rust-version` to equal rust-toolchain.toml's channel (major.minor).
4. requires every allow of one of those lints to carry a marker: `// ratchet:`
   (counted) or `// tests:` (test code only: a file under tests/, examples/ or
   benches/, the testkit crate, or the `#[cfg(test)] mod` it is written above).

`let _ =` used to be counted by tools/check_silent_failures.py as well; Clippy
now owns that rule (it sees types, which a regex cannot), so that script keeps
only the poison-recovery idiom. See CONTRIBUTING.md, "Lints and the ratchet".

    python3 tools/check_lint_ratchet.py            check
    python3 tools/check_lint_ratchet.py --update   record the current counts
"""

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BASELINE = ROOT / "tools" / "lint_ratchet.json"

DENIED = (
    "let_underscore_must_use",
    "let_underscore_future",
    "unwrap_used",
    "expect_used",
    "panic",
    "unwrap_in_result",
    "map_unwrap_or",
    "too_many_arguments",
    "todo",
    "unimplemented",
    "dbg_macro",
)
# An `#[allow(..)]` or `#![allow(..)]`, possibly over several lines, and what follows it on its last line.
ALLOW = re.compile(r"#!?\[allow\((?P<lints>[^\]]*?)\)\][ \t]*(?P<tail>[^\n]*)")
TEST_DIRS = ("tests", "examples", "benches")


def rust_files(root):
    for path in sorted((root / "crates").glob("*/**/*.rs")):
        rel = path.relative_to(root)
        if "target" not in rel.parts:
            yield path, rel


def is_test_file(rel):
    return rel.parts[2] in TEST_DIRS or rel.parts[1] == "branchyard-testkit"


def scan(root):
    """(marker count per crate, problems with the allows themselves)."""
    counts, problems = {}, []
    for path, rel in rust_files(root):
        text = path.read_text(encoding="utf-8")
        crate = rel.parts[1]
        for match in ALLOW.finditer(text):
            lints = {name.strip().removeprefix("clippy::") for name in match["lints"].split(",")}
            if not lints & set(DENIED):
                continue
            tail = match["tail"].strip()
            line = text.count("\n", 0, match.start()) + 1
            where = f"{rel.as_posix()}:{line}"
            if tail.startswith("// ratchet:"):
                counts[crate] = counts.get(crate, 0) + 1
            elif tail.startswith("// tests:"):
                following = text[match.end() :].lstrip()
                after_cfg = re.match(r"#\[cfg\(test\)\]\s*(pub(\([^)]*\))? )?mod \w+", following)
                if not (is_test_file(rel) or after_cfg):
                    problems.append(f"{where}: `// tests:` is for test code; use `// ratchet: {crate}` instead")
            else:
                problems.append(
                    f"{where}: allow of a denied lint without a `// ratchet: {crate}` marker; "
                    "fix the code instead, or (existing code only) lower the count later"
                )
    return counts, problems


def manifest_problems(root):
    problems = []
    for toml in sorted((root / "crates").glob("*/Cargo.toml")):
        rel = toml.relative_to(root).as_posix()
        text = toml.read_text(encoding="utf-8")
        if not re.search(r"^\[lints\]\s*\nworkspace\s*=\s*true\s*$", text, re.M):
            problems.append(f"{rel}: missing `[lints]` `workspace = true`")
        if not re.search(r"^rust-version\.workspace\s*=\s*true\s*$", text, re.M):
            problems.append(f"{rel}: missing `rust-version.workspace = true`")
    cargo = (root / "Cargo.toml").read_text(encoding="utf-8")
    table = re.search(r"^\[workspace\.lints\.clippy\]\n(.*?)(?:\n\[|\Z)", cargo, re.M | re.S)
    body = table[1] if table else ""
    for lint in DENIED:
        if not re.search(rf'^{lint}\s*=\s*"deny"', body, re.M):
            problems.append(f'Cargo.toml: [workspace.lints.clippy] must set {lint} = "deny"')
    channel = re.search(r'channel\s*=\s*"(\d+\.\d+)', (root / "rust-toolchain.toml").read_text(encoding="utf-8"))
    declared = re.search(r'^rust-version\s*=\s*"([^"]+)"', cargo, re.M)
    if not channel or not declared or declared[1] != channel[1]:
        problems.append(
            f"Cargo.toml: [workspace.package] rust-version {declared and declared[1]!r} "
            f"must equal rust-toolchain.toml's channel {channel and channel[1]!r}"
        )
    return problems


def check(counts, baseline):
    problems = []
    for crate in sorted(set(counts) | set(baseline)):
        now, allowed = counts.get(crate, 0), baseline.get(crate, 0)
        if now > allowed:
            problems.append(
                f"{crate}: {now} `// ratchet:` allows (baseline {allowed}); "
                "fix the new code (`?`, `best_effort`, a typed error) instead of allowing it"
            )
        elif now < allowed:
            problems.append(
                f"{crate}: {now} `// ratchet:` allows, baseline {allowed}; lower it with "
                "`python3 tools/check_lint_ratchet.py --update` to keep the gain"
            )
    return problems


def main(argv):
    counts, problems = scan(ROOT)
    problems += manifest_problems(ROOT)
    if "--update" in argv:
        if problems:
            print("refusing to update while there are problems:", *problems, sep="\n  ")
            return 1
        BASELINE.write_text(json.dumps(dict(sorted(counts.items())), indent=2) + "\n")
        print(f"wrote {BASELINE.relative_to(ROOT)}: {sum(counts.values())} ratchet allows remain")
        return 0
    problems += check(counts, json.loads(BASELINE.read_text()))
    for problem in problems:
        print(problem)
    if problems:
        return 1
    print(f"lint ratchet ok: {sum(counts.values())} allows (baseline), every crate inherits the workspace lints")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
