#!/usr/bin/env python3
"""Keep the poison-recovery idiom out of the code.

Over the non-test Rust source of every crate (crates/*/src), the idiom
`.lock().unwrap_or_else(|e| e.into_inner())` (and `PoisonError::into_inner`)
is banned outright. Use `branchyard_support::LockExt::lock_recovering("name")`,
which logs the poisoning. The count is zero and stays zero.

A bare `let _ = <fallible>;` used to be counted here too. Clippy owns that now
(`let_underscore_must_use` at deny in `[workspace.lints]`, which sees types
where a regex cannot), and its exceptions are counted by
tools/check_lint_ratchet.py. Where the failure is acceptable, use
`branchyard_support::best_effort("what", expr)` (or `cleanup_dir`,
`cleanup_file`, `kill_group`, `join_reporting`), which logs it.

Code from the first `#[cfg(test)]` line to the end of a file is test code and
is not scanned. See CONTRIBUTING.md, "Failures that may be ignored".
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# The idiom in any spelling, found on text with whitespace removed.
CLOSURE_IDIOM = re.compile(r"unwrap_or_else\(\|(\w+)\|\1\.into_inner\(\)\)")
PATH_IDIOM = ("unwrap_or_else(PoisonError::into_inner)", "unwrap_or_else(std::sync::PoisonError::into_inner)")
# branchyard-support implements the helpers and documents the idiom.
EXEMPT = {"crates/branchyard-support/src/locks.rs"}


def non_test(source):
    cut = source.find("\n#[cfg(test)]")
    return source if cut < 0 else source[:cut]


def has_idiom(source):
    squeezed = re.sub(r"\s+", "", source)
    return bool(CLOSURE_IDIOM.search(squeezed)) or any(p in squeezed for p in PATH_IDIOM)


def scan(root):
    """The files that use the poison idiom."""
    idiom_files = []
    for path in sorted((root / "crates").glob("*/src/**/*.rs")):
        rel = path.relative_to(root).as_posix()
        if rel not in EXEMPT and has_idiom(non_test(path.read_text(encoding="utf-8"))):
            idiom_files.append(rel)
    return idiom_files


def check(idiom_files):
    return [
        f'{rel}: retypes the poison-recovery idiom; use branchyard_support::LockExt::lock_recovering("name")'
        for rel in idiom_files
    ]


def main():
    problems = check(scan(ROOT))
    for problem in problems:
        print(problem)
    if problems:
        return 1
    print("silent failures ok: no poison idiom")
    return 0


if __name__ == "__main__":
    sys.exit(main())
