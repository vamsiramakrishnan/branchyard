#!/usr/bin/env python3
"""Keep error discipline machine-enforced: the lint table, and a ratchet on its exceptions.

The workspace denies, through Clippy and rustc (Cargo.toml `[workspace.lints]`,
shared by every crate with `[lints] workspace = true`): `let _ =` on a
must-use value, `unwrap`, `expect`, `panic!`, `unwrap_in_result`,
`map_unwrap_or`, `too_many_arguments`, `todo!`, `unimplemented!`, `dbg!` and
`unused_must_use`. Code that predates the table carries a targeted
`#[allow(clippy::..)]` (an item, or a whole file) with a `// ratchet: <crate>`
marker after the attribute, naming the crate the file is in. This script, no
build needed:

1. counts the markers per crate against tools/lint_ratchet.json. A count may
   not rise; when it falls the file must be lowered (`--update`) so the gain
   is kept. A crate not listed has a count of zero.
2. keeps the set of files with a file-wide (`#![allow]`) marker. A file may
   not join it, so a file cannot absorb new violations at no cost; a file that
   leaves it must be removed with `--update`.
3. requires every crates/*/Cargo.toml to say `[lints]` `workspace = true`
   and to inherit `rust-version`, and `branchyard-testkit` to be a
   dev-dependency only.
4. requires the root Cargo.toml to deny every lint above, and its
   `rust-version` to equal rust-toolchain.toml's channel (major.minor).
5. finds every way to switch a denied lint off: `allow` and `expect`, inside
   `cfg_attr` too, naming the lint or a group that contains it (`clippy::all`,
   `pedantic`, `restriction`, `unused`, `warnings`, ...), however the path is
   spaced. Matches in comments and string literals are ignored. Each needs a
   marker: `// ratchet: <crate>` (counted) or `// tests: ...` (test code only:
   a file under tests/, examples/ or benches/, the testkit crate, or the
   `#[cfg(test)] mod` it is written above). The one exception is the generated
   protobuf module of branchyard-substrate (`GENERATED`).

`let _ =` used to be counted by tools/check_silent_failures.py as well; Clippy
now owns that rule (it sees types, which a regex cannot), so that script keeps
only the poison-recovery idiom. See CONTRIBUTING.md, "Lints and the ratchet".

    python3 tools/check_lint_ratchet.py            check
    python3 tools/check_lint_ratchet.py --update   record the current state
"""

import json
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BASELINE = ROOT / "tools" / "lint_ratchet.json"

CLIPPY_DENIED = (
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
RUST_DENIED = ("unused_must_use",)
# Groups whose members include a denied lint: allowing one allows it.
GROUPS = {
    "clippy::restriction",
    "clippy::pedantic",
    "clippy::complexity",
    "clippy::suspicious",
    "clippy::all",
    "unused",
    "warnings",
}
DENIED = {f"clippy::{lint}" for lint in CLIPPY_DENIED} | set(RUST_DENIED)
COVERED = DENIED | GROUPS
# Generated code, where `clippy::all` is allowed because we do not write it.
GENERATED = {"crates/branchyard-substrate/src/lib.rs": {"clippy::all"}}
TEST_DIRS = ("tests", "examples", "benches")
ATTRIBUTE = re.compile(r"#(?P<inner>!?)\[\s*(?P<name>allow|expect|cfg_attr)\b")
SWITCH = re.compile(r"\b(allow|expect)\s*\(")


def rust_files(root):
    for path in sorted((root / "crates").glob("*/**/*.rs")):
        rel = path.relative_to(root)
        if "target" not in rel.parts:
            yield path, rel


def is_test_file(rel):
    return rel.parts[2] in TEST_DIRS or rel.parts[1] == "branchyard-testkit"


def mask(text):
    """`text` with comments and string and char literals blanked, newlines and offsets kept."""
    out = list(text)
    i, n = 0, len(text)

    def blank(a, b):
        for k in range(a, b):
            if out[k] != "\n":
                out[k] = " "

    while i < n:
        two = text[i : i + 2]
        if two == "//":
            j = text.find("\n", i)
            j = n if j < 0 else j
            blank(i, j)
            i = j
        elif two == "/*":
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            blank(i, j)
            i = j
        elif (
            text[i] == "r"
            and re.match(r'r#*"', text[i:])
            and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_"))
        ):
            hashes = len(re.match(r"r(#*)", text[i:]).group(1))
            end = text.find('"' + "#" * hashes, i + 2 + hashes)
            end = n if end < 0 else end + 1 + hashes
            blank(i + 2 + hashes, end - 1 - hashes)
            i = end
        elif text[i] == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            blank(i + 1, j)
            i = j + 1
        elif text[i] == "'":
            lit = re.match(r"'(?:\\.[^']*|[^\\'])'", text[i:])
            if lit:
                blank(i + 1, i + len(lit.group(0)) - 1)
                i += len(lit.group(0))
            else:
                i += 1
        else:
            i += 1
    return "".join(out)


def attribute_end(masked, start):
    """The offset just past the `]` closing the attribute whose `[` is the first one at or after `start`."""
    depth, i = 0, masked.index("[", start)
    while i < len(masked):
        if masked[i] == "[":
            depth += 1
        elif masked[i] == "]":
            depth -= 1
            if depth == 0:
                return i + 1
        i += 1
    return len(masked)


def switched_off(body):
    """The lints (`clippy::` kept, spaces removed) that `allow(..)` and `expect(..)` in `body` name."""
    lints = set()
    for match in SWITCH.finditer(body):
        depth, j = 1, match.end()
        while j < len(body) and depth:
            depth += {"(": 1, ")": -1}.get(body[j], 0)
            j += 1
        for item in body[match.end() : j - 1].split(","):
            item = re.sub(r"\s+", "", item.split("=")[0])
            if item:
                lints.add(item)
    return lints


def scan(root):
    """(marker count per crate, files with a file-wide marker, problems with the allows themselves)."""
    counts, wide, problems = {}, [], []
    for path, rel in rust_files(root):
        text = path.read_text(encoding="utf-8")
        masked = mask(text)
        crate, relname = rel.parts[1], rel.as_posix()
        for match in ATTRIBUTE.finditer(masked):
            end = attribute_end(masked, match.start())
            lints = switched_off(masked[match.start() : end]) & COVERED
            lints -= GENERATED.get(relname, set())
            if not lints:
                continue
            line = text.count("\n", 0, match.start()) + 1
            where = f"{relname}:{line}"
            inner = bool(match["inner"])
            line_end = text.find("\n", end)
            tail = text[end : len(text) if line_end < 0 else line_end].strip()
            if tail.startswith("// ratchet:"):
                named = tail[len("// ratchet:") :].split()
                if not named or named[0] != crate:
                    problems.append(f"{where}: the marker must name this file's crate: `// ratchet: {crate}`")
                counts[crate] = counts.get(crate, 0) + 1
                if inner:
                    wide.append(relname)
            elif tail.startswith("// tests:"):
                before_cfg_test = re.match(r"\s*#\[cfg\(test\)\]\s*(pub(\([^)]*\))? )?mod \w+", masked[end:])
                if not (is_test_file(rel) or (not inner and before_cfg_test)):
                    problems.append(f"{where}: `// tests:` is for test code; use `// ratchet: {crate}` instead")
            else:
                problems.append(
                    f"{where}: allows a denied lint ({', '.join(sorted(lints))}) "
                    f"without a `// ratchet: {crate}` marker; fix the code instead"
                )
    return counts, sorted(set(wide)), problems


def manifest_problems(root):
    problems = []
    for toml in sorted((root / "crates").glob("*/Cargo.toml")):
        rel = toml.relative_to(root).as_posix()
        text = toml.read_text(encoding="utf-8")
        if not re.search(r"^\[lints\]\s*\nworkspace\s*=\s*true\s*$", text, re.M):
            problems.append(f"{rel}: missing `[lints]` `workspace = true`")
        if not re.search(r"^rust-version\.workspace\s*=\s*true\s*$", text, re.M):
            problems.append(f"{rel}: missing `rust-version.workspace = true`")
        manifest = tomllib.loads(text)
        tables = [manifest.get("dependencies", {}), manifest.get("build-dependencies", {})]
        tables += [
            t.get(kind, {})
            for t in manifest.get("target", {}).values()
            for kind in ("dependencies", "build-dependencies")
        ]
        if any("branchyard-testkit" in table for table in tables):
            problems.append(f"{rel}: branchyard-testkit may only be a dev-dependency")
    cargo = (root / "Cargo.toml").read_text(encoding="utf-8")
    for table, lints in (("clippy", CLIPPY_DENIED), ("rust", RUST_DENIED)):
        found = re.search(rf"^\[workspace\.lints\.{table}\]\n(.*?)(?:\n\[|\Z)", cargo, re.M | re.S)
        body = found[1] if found else ""
        for lint in lints:
            if not re.search(rf'^{lint}\s*=\s*"deny"', body, re.M):
                problems.append(f'Cargo.toml: [workspace.lints.{table}] must set {lint} = "deny"')
    channel = re.search(r'channel\s*=\s*"(\d+\.\d+)', (root / "rust-toolchain.toml").read_text(encoding="utf-8"))
    declared = re.search(r'^rust-version\s*=\s*"([^"]+)"', cargo, re.M)
    if not channel or not declared or declared[1] != channel[1]:
        problems.append(
            f"Cargo.toml: [workspace.package] rust-version {declared and declared[1]!r} "
            f"must equal rust-toolchain.toml's channel {channel and channel[1]!r}"
        )
    return problems


def check(counts, wide, baseline):
    problems = []
    allowed_counts = baseline.get("allows", {})
    for crate in sorted(set(counts) | set(allowed_counts)):
        now, allowed = counts.get(crate, 0), allowed_counts.get(crate, 0)
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
    listed = set(baseline.get("file_wide", []))
    for rel in sorted(set(wide) - listed):
        problems.append(
            f"{rel}: a new file-wide `#![allow]` marker; allow the one function (`#[allow(..)] // ratchet:`) "
            "or fix the code: a file-wide allow lets the whole file absorb new violations"
        )
    for rel in sorted(listed - set(wide)):
        problems.append(
            f"{rel}: no longer has a file-wide marker; drop it with `python3 tools/check_lint_ratchet.py --update`"
        )
    return problems


def main(argv):
    counts, wide, problems = scan(ROOT)
    problems += manifest_problems(ROOT)
    if "--update" in argv:
        if problems:
            print("refusing to update while there are problems:", *problems, sep="\n  ")
            return 1
        old = json.loads(BASELINE.read_text()) if BASELINE.exists() else {}
        added = sorted(set(wide) - set(old.get("file_wide", [])))
        if old and (added or any(counts.get(c, 0) > n for c, n in old.get("allows", {}).items())):
            print("refusing to raise the ratchet:", *added, sep="\n  ")
            return 1
        state = {"allows": dict(sorted(counts.items())), "file_wide": wide}
        BASELINE.write_text(json.dumps(state, indent=2) + "\n")
        print(
            f"wrote {BASELINE.relative_to(ROOT)}: {sum(counts.values())} ratchet allows remain, {len(wide)} file-wide"
        )
        return 0
    problems += check(counts, wide, json.loads(BASELINE.read_text()))
    for problem in problems:
        print(problem)
    if problems:
        return 1
    print(
        f"lint ratchet ok: {sum(counts.values())} allows ({len(wide)} file-wide), "
        "every crate inherits the workspace lints"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
