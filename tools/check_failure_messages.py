#!/usr/bin/env python3
"""Keep `by`'s failures in one shape: a Report, not a bare message.

A failure of the CLI (crates/branchyard-cli) is a `Report` built with
`Report::new(kind, what).next(..)` and returned as `Failure::Report`: it has a
kind, so `--json` names it and the process exits with that kind's status
(docs/cli.md, "Exit status"), and it ends with the command to run next.
`Failure::Message(String)` is the older shape: a sentence with the fix folded
into its prose, reported with the kind `unclassified` (exit 1) whatever went
wrong.

Each place that still builds one (`Failure::Message(...)`, or
`map_err(Failure::Message)`) is counted per file of crates/branchyard-cli/src,
outside `#[cfg(test)]` code, comments and strings; a match arm that reads one
(`Failure::Message(m) => ...`) is not a place that builds one. The counts are in
tools/failure_message_ratchet.json, and they only fall: a file over its count,
or not listed, fails; a file under its count fails too until the count is
lowered, so the list shrinks as the code does and never grows (AGENTS.md,
"Ratchets only go down").

    python3 tools/check_failure_messages.py            check
    python3 tools/check_failure_messages.py --list     the sites, per file
    python3 tools/check_failure_messages.py --update   lower the counts to what is left
"""

import importlib.util
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RATCHET = "tools/failure_message_ratchet.json"
SCOPE = "crates/branchyard-cli/src"
SITE = re.compile(r"\bFailure\s*::\s*Message\b")

# Comments, strings and #[cfg(test)] items are blanked as the silent-failures
# check does it: one implementation of "the code that is left".
_spec = importlib.util.spec_from_file_location("_silent", Path(__file__).resolve().parent / "check_silent_failures.py")
assert _spec is not None and _spec.loader is not None
_silent = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_silent)


def _closing(code, start):
    """The index after the parenthesis that closes the one at `start`."""
    depth = 0
    for j in range(start, len(code)):
        if code[j] == "(":
            depth += 1
        elif code[j] == ")":
            depth -= 1
            if depth == 0:
                return j + 1
    return len(code)


def sites(source):
    """The lines of `source` that build a `Failure::Message`, in non-test code."""
    code = _silent.strip_cfg_test(_silent.blank(source))
    found = []
    for m in SITE.finditer(code):
        end = m.end()
        rest = code[end:].lstrip()
        if rest.startswith("("):
            after = code[_closing(code, code.index("(", end)) :].lstrip()
            # A pattern that reads one: `Failure::Message(m) =>` or `... | ...`.
            if after.startswith("=>") or (after.startswith("|") and not after.startswith("||")):
                continue
            if after.startswith("if ") and "=>" in after.split("\n", 1)[0]:
                continue
        found.append(code.count("\n", 0, m.start()) + 1)
    return found


def scan(root=ROOT):
    """{file: [line, ...]} for every file of the CLI that builds one."""
    out = {}
    for path in sorted((root / SCOPE).rglob("*.rs")):
        lines = sites(path.read_text(encoding="utf-8"))
        if lines:
            out[path.relative_to(root).as_posix()] = lines
    return out


def load(root=ROOT):
    path = root / RATCHET
    if not path.exists():
        return {"files": {}}
    return json.loads(path.read_text(encoding="utf-8"))


def check(found, ratchet):
    """What is wrong between `found` and the ratchet's counts."""
    problems = []
    listed = ratchet.get("files", {})
    for rel, lines in sorted(found.items()):
        allowed = listed.get(rel, 0)
        if len(lines) > allowed:
            where = ", ".join(str(n) for n in lines)
            problems.append(
                f"{rel}: {len(lines)} Failure::Message site(s) (lines {where}), {RATCHET} allows {allowed}; "
                'return a Report instead: Report::new(kind, what).next("the command to run").into() '
                "(crates/branchyard-cli/src/errors.rs)"
            )
        elif len(lines) < allowed:
            problems.append(
                f"{rel}: {len(lines)} left, {RATCHET} allows {allowed}; "
                "lower it: python3 tools/check_failure_messages.py --update"
            )
    for rel in sorted(listed):
        if rel not in found:
            problems.append(f"{RATCHET}: {rel} has none left; remove it (--update)")
    return problems


def update(found, ratchet):
    """The ratchet lowered to `found`; a count never rises and no file joins."""
    listed = ratchet.setdefault("files", {})
    for rel in list(listed):
        have = len(found.get(rel, []))
        if have == 0:
            del listed[rel]
        elif have < listed[rel]:
            listed[rel] = have
    return ratchet


def main(argv=None, root=ROOT):
    argv = sys.argv[1:] if argv is None else argv
    found = scan(root)
    total = sum(len(lines) for lines in found.values())
    if "--list" in argv:
        for rel, lines in found.items():
            print(f"{len(lines):3d} {rel}: {', '.join(str(n) for n in lines)}")
        print(f"{total} in {len(found)} files")
        return 0
    if "--update" in argv:
        ratchet = update(found, load(root))
        (root / RATCHET).write_text(json.dumps(ratchet, indent=2) + "\n", encoding="utf-8")
        kept = sum(ratchet["files"].values())
        print(f"{RATCHET} lowered to what is left ({kept}); it never rises")
        return 0
    problems = check(found, load(root))
    for problem in problems:
        print(problem)
    if problems:
        return 1
    print(f"failure messages ok: {total} Failure::Message site(s) left in {len(found)} files, all counted in {RATCHET}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
