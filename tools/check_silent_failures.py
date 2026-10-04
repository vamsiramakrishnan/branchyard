#!/usr/bin/env python3
"""Keep silent failures from coming back, and let the existing ones only fall.

Two rules over the non-test Rust source of every crate (crates/*/src):

1. The poison-recovery idiom `.lock().unwrap_or_else(|e| e.into_inner())`
   (and `PoisonError::into_inner`) is banned outright. Use
   `branchyard_support::LockExt::lock_recovering("name")`, which logs the
   poisoning. The count is zero and stays zero.

2. A bare `let _ = <expr>;` discards a result with no trace. Where the failure
   is acceptable, use `branchyard_support::best_effort("what", expr)` (or
   `cleanup_dir`, `cleanup_file`, `kill_group`, `join_reporting`), which logs
   it. The `let _ =` sites that remain are counted per crate in
   tools/silent_failures.json. A crate's count may not rise, and when it
   falls the baseline must be lowered in the same change (run with --update),
   so the gain is kept.

Code from the first `#[cfg(test)]` line to the end of a file is test code and
is not counted. See CONTRIBUTING.md, "Failures that may be ignored".
"""

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BASELINE = ROOT / "tools" / "silent_failures.json"

LET_UNDERSCORE = re.compile(r"^\s*let _ = ", re.M)
# The idiom in any spelling, found on text with whitespace removed.
CLOSURE_IDIOM = re.compile(r"unwrap_or_else\(\|(\w+)\|\1\.into_inner\(\)\)")
PATH_IDIOM = ("unwrap_or_else(PoisonError::into_inner)",
              "unwrap_or_else(std::sync::PoisonError::into_inner)")
# branchyard-support implements the helpers and documents the idiom.
EXEMPT = {"crates/branchyard-support/src/locks.rs"}


def non_test(source):
    cut = source.find("\n#[cfg(test)]")
    return source if cut < 0 else source[:cut]


def has_idiom(source):
    squeezed = re.sub(r"\s+", "", source)
    return bool(CLOSURE_IDIOM.search(squeezed)) or any(p in squeezed for p in PATH_IDIOM)


def scan(root):
    """(let-underscore count per crate, files that use the poison idiom)."""
    counts = {}
    idiom_files = []
    for path in sorted((root / "crates").glob("*/src/**/*.rs")):
        rel = path.relative_to(root).as_posix()
        crate = rel.split("/")[1]
        text = non_test(path.read_text(encoding="utf-8"))
        n = len(LET_UNDERSCORE.findall(text))
        if n:
            counts[crate] = counts.get(crate, 0) + n
        if rel not in EXEMPT and has_idiom(text):
            idiom_files.append(rel)
    return counts, idiom_files


def check(counts, idiom_files, baseline):
    problems = []
    for rel in idiom_files:
        problems.append(
            f"{rel}: retypes the poison-recovery idiom; use "
            "branchyard_support::LockExt::lock_recovering(\"name\")"
        )
    for crate in sorted(set(counts) | set(baseline)):
        now, allowed = counts.get(crate, 0), baseline.get(crate, 0)
        if now > allowed:
            problems.append(
                f"{crate}: {now} `let _ =` (baseline {allowed}); use "
                "branchyard_support::best_effort(\"what\", expr) so the failure is logged"
            )
        elif now < allowed:
            problems.append(
                f"{crate}: {now} `let _ =`, baseline {allowed}; lower it with "
                "`python3 tools/check_silent_failures.py --update` to keep the gain"
            )
    return problems


def main(argv):
    counts, idiom_files = scan(ROOT)
    if "--update" in argv:
        if idiom_files:
            print("refusing to update while the poison idiom is in use:", *idiom_files, sep="\n  ")
            return 1
        BASELINE.write_text(json.dumps(dict(sorted(counts.items())), indent=2) + "\n")
        print(f"wrote {BASELINE.relative_to(ROOT)}: {sum(counts.values())} `let _ =` remain")
        return 0
    baseline = json.loads(BASELINE.read_text())
    problems = check(counts, idiom_files, baseline)
    for problem in problems:
        print(problem)
    if problems:
        return 1
    print(f"silent failures ok: {sum(counts.values())} `let _ =` (baseline), no poison idiom")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
