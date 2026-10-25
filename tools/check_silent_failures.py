#!/usr/bin/env python3
"""Keep failures from vanishing quietly.

Two rules, over the non-test Rust source of every crate (crates/*/src).

1. The poison-recovery idiom `.lock().unwrap_or_else(|e| e.into_inner())` (and
   `PoisonError::into_inner`) is banned outright. Use
   `branchyard_support::LockExt::lock_recovering("name")`, which logs the
   poisoning. The count is zero and stays zero.

2. A read turned into an answer. `.ok()` or `.unwrap_or_default()` on a read
   of the store (`store()`, `store.`), the effect ledger (`ledger.`), a
   branch's event log (`record::read(`) or a file (`fs::read*(`) makes an
   error look like "nothing there": a locked or damaged store reads as "that
   branch is gone", an unreadable log as an empty one. Return the error
   (`Error::could_not`), or, where a fallback is really safe, say so with
   `branchyard_support::best_effort("what", read)` and a comment. Existing
   uses are counted per file in tools/silent_failures_ratchet.json with the
   reason each is tolerated; a count may only fall. A file over its count, or
   not listed, fails; a file under its count also fails until the count is
   lowered, so the list shrinks as the code does and never grows.

A bare `let _ = <fallible>;` used to be counted here too. Clippy owns that now
(`let_underscore_must_use` at deny in `[workspace.lints]`, which sees types
where a regex cannot), and its exceptions are counted by
tools/check_lint_ratchet.py.

Code under `#[cfg(test)]` is test code and is not scanned, nor is the test kit
(crates/branchyard-testkit) or the fake agent. See CONTRIBUTING.md, "Failures
that may be ignored".

`python3 tools/check_silent_failures.py --list` prints the current counts per
file; `--update` rewrites the ratchet's counts downward (it never raises one).
"""

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RATCHET = "tools/silent_failures_ratchet.json"

# The idiom in any spelling, found on text with whitespace removed.
CLOSURE_IDIOM = re.compile(r"unwrap_or_else\(\|(\w+)\|\1\.into_inner\(\)\)")
PATH_IDIOM = ("unwrap_or_else(PoisonError::into_inner)", "unwrap_or_else(std::sync::PoisonError::into_inner)")
# branchyard-support implements the helpers and documents the idiom.
EXEMPT = {"crates/branchyard-support/src/locks.rs"}

# Test support, not product code.
READS_EXEMPT_PREFIXES = ("crates/branchyard-testkit/", "crates/branchyard-runtime/src/bin/fake-acp-agent.rs")

# The calls that turn an error into "nothing", and the reads they hide.
SWALLOW = re.compile(r"\.\s*(ok|unwrap_or_default)\s*\(\s*\)")
READS = re.compile(
    r"""\bstore\s*\(\s*\)        # yard.store()
      | \bledger\b\s*\.         # ledger.effects()
      | \brecord::read\s*\(     # a branch's event log
      | \bfs::read\w*\s*\(      # std::fs::read, read_to_string, read_dir, read_link
      | \bstore\s*\.\s*\w+      # a local `store`
    """,
    re.X,
)


BEST_EFFORT = re.compile(r"\bbest_effort(?:_once)?\s*\(")


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


def _blank_run(text):
    return re.sub(r"[^\n]", " ", text)


def _starts_a_word(source, i):
    """Whether `source[i]` begins an identifier or literal prefix, not the
    tail of one (`r` in `expr#"` is not a raw string)."""
    return i == 0 or not (source[i - 1].isalnum() or source[i - 1] == "_")


def blank(source):
    """`source` with comments and the insides of string and char literals
    replaced by spaces (newlines kept), so that what is left is code."""
    out = []
    i, n = 0, len(source)
    while i < n:
        c = source[i]
        nxt = source[i + 1] if i + 1 < n else ""
        if c == "/" and nxt == "/":
            j = source.find("\n", i)
            j = n if j < 0 else j
            out.append(" " * (j - i))
            i = j
        elif c == "/" and nxt == "*":
            j = source.find("*/", i + 2)
            j = n if j < 0 else j + 2
            out.append(_blank_run(source[i:j]))
            i = j
        elif c == "r" and (opening := re.match(r'r(#*)"', source[i : i + 80])) and _starts_a_word(source, i):
            hashes = opening.group(1)
            end = source.find('"' + hashes, i + len(opening.group(0)))
            end = n if end < 0 else end + 1 + len(hashes)
            out.append(_blank_run(source[i:end]))
            i = end
        elif c == '"':
            j = i + 1
            while j < n and source[j] != '"':
                j += 2 if source[j] == "\\" else 1
            j = min(j + 1, n)
            out.append('"' + _blank_run(source[i + 1 : j - 1]) + '"')
            i = j
        elif c == "'":
            literal = re.match(r"'(\\.[^']*|[^\\'])'", source[i:])
            if literal:
                out.append(" " * len(literal.group(0)))
                i += len(literal.group(0))
            else:
                out.append(c)  # a lifetime
                i += 1
        else:
            out.append(c)
            i += 1
    return "".join(out)


def _matching_brace(text, start):
    depth = 0
    for j in range(start, len(text)):
        if text[j] == "{":
            depth += 1
        elif text[j] == "}":
            depth -= 1
            if depth == 0:
                return j + 1
    return len(text)


def strip_cfg_test(text):
    """`text` (already `blank`ed) without its `#[cfg(test)]` items: a module
    or function by its braces, a `use` or `mod x;` by its semicolon."""
    out, pos = [], 0
    for m in re.finditer(r"#\[cfg\(test\)\]", text):
        if m.start() < pos:
            continue
        out.append(text[pos : m.start()])
        brace, semi = text.find("{", m.end()), text.find(";", m.end())
        if brace >= 0 and (semi < 0 or brace < semi):
            end = _matching_brace(text, brace)
        else:
            end = semi + 1 if semi >= 0 else len(text)
        out.append(_blank_run(text[m.start() : end]))
        pos = end
    out.append(text[pos:])
    return "".join(out)


def _is_assignment(text, j):
    """Whether `text[j]` is the `=` of an assignment or `let`, not part of
    `==`, `!=`, `<=`, `>=` or `=>`."""
    return text[j] == "=" and text[j - 1 : j] not in ("=", "!", "<", ">") and text[j + 1 : j + 2] != "="


def _receiver(text, end):
    """The expression a call ending at `end` is made on: back to the nearest
    delimiter that is not inside brackets (`;`, `{`, `,`, `=`, `|`, an
    unmatched `(`)."""
    depth, j = 0, end - 1
    while j >= 0:
        c = text[j]
        if c in ")]":
            depth += 1
        elif c in "([":
            if depth == 0:
                break
            depth -= 1
        elif depth == 0 and (c in ";{},|" or _is_assignment(text, j)):
            break
        j -= 1
    return text[j + 1 : end]


def _after_call(receiver, read):
    """What follows a read in `receiver`, with the arguments of every call
    left out: `.ok()?` after `fs::read(x?)` is, `x?` is not."""
    rest = receiver[read.end() :]
    if read.group(0).endswith("("):
        depth, i = 1, 0
        while i < len(rest) and depth:
            depth += {"(": 1, ")": -1}.get(rest[i], 0)
            i += 1
        rest = rest[i:]
    out, depth = [], 0
    for c in rest:
        if c in "([":
            depth += 1
        elif c in ")]":
            depth = max(0, depth - 1)
        elif depth == 0:
            out.append(c)
    return "".join(out)


def read_hits(source):
    """(line, call, receiver) for each `.ok()` or `.unwrap_or_default()` made
    on a read of the store, the ledger, an event log or a file, in non-test
    code of `source`."""
    code = strip_cfg_test(blank(source))
    hits = []
    for m in SWALLOW.finditer(code):
        receiver = _receiver(code, m.start())
        line = code.count("\n", 0, m.start()) + 1
        # A read whose failure is logged first (`best_effort("what", read)`
        # then `.unwrap_or_default()`) is the sanctioned fallback, not a hit.
        if BEST_EFFORT.search(receiver):
            continue
        # A `?` after the read has already returned its error: what is
        # swallowed is the `Option` inside (`read(x)?.map(..).unwrap_or_default()`).
        reads_in = list(READS.finditer(receiver))
        if reads_in and "?" in _after_call(receiver, reads_in[-1]):
            continue
        # One read swallowed twice on a line (`.ok()?).ok()?`) is one hit.
        if READS.search(receiver) and not any(seen == line for seen, _, _ in hits):
            hits.append((line, m.group(1), " ".join(receiver.split())[-80:]))
    return hits


def scan_reads(root):
    """{file: [(line, call, receiver)]} for the tree at `root`."""
    found = {}
    for path in sorted((root / "crates").glob("*/src/**/*.rs")):
        rel = path.relative_to(root).as_posix()
        if rel.startswith(READS_EXEMPT_PREFIXES):
            continue
        hits = read_hits(path.read_text(encoding="utf-8"))
        if hits:
            found[rel] = hits
    return found


def check_reads(found, ratchet):
    """Problems between what `scan_reads` found and the ratchet's `reads`."""
    problems = []
    listed = ratchet.get("reads", {})
    for rel, hits in sorted(found.items()):
        entry = listed.get(rel)
        allowed = entry["count"] if entry else 0
        if len(hits) > allowed:
            for line, call, receiver in hits:
                problems.append(
                    f"{rel}:{line}: `.{call}()` on `{receiver}` turns a read error into 'nothing'; "
                    "return the error (Error::could_not), or use branchyard_support::best_effort "
                    f"and say why a fallback is safe. Existing uses are counted in {RATCHET}, which may not grow"
                )
        elif len(hits) < allowed:
            problems.append(f"{rel}: {len(hits)} left, {RATCHET} allows {allowed}; lower its count to {len(hits)}")
    for rel, entry in sorted(listed.items()):
        if rel not in found:
            problems.append(f"{RATCHET}: {rel} has none left; remove its entry")
        if not str(entry.get("why", "")).strip():
            problems.append(f"{RATCHET}: {rel} needs a `why`: the reason these are tolerated")
    return problems


def load_ratchet(root=ROOT):
    path = root / RATCHET
    if not path.exists():
        return {"reads": {}}
    return json.loads(path.read_text(encoding="utf-8"))


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    found = scan_reads(ROOT)
    if "--list" in argv:
        for rel, hits in found.items():
            print(f"{len(hits):3d} {rel}")
            for line, call, receiver in hits:
                print(f"      {line}: .{call}() <- {receiver}")
        print(f"{sum(len(h) for h in found.values())} in {len(found)} files")
        return 0
    if "--update" in argv:
        ratchet = load_ratchet()
        listed = ratchet.setdefault("reads", {})
        for rel in list(listed):
            have = len(found.get(rel, []))
            if have == 0:
                del listed[rel]
            elif have < listed[rel]["count"]:
                listed[rel]["count"] = have
        (ROOT / RATCHET).write_text(json.dumps(ratchet, indent=2) + "\n", encoding="utf-8")
        print(f"{RATCHET} lowered to what is left; it never rises")
        return 0
    problems = check(scan(ROOT)) + check_reads(found, load_ratchet())
    for problem in problems:
        print(problem)
    if problems:
        return 1
    total = sum(len(h) for h in found.values())
    print(f"silent failures ok: no poison idiom; {total} read(s) turned into answers, all counted in {RATCHET}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
