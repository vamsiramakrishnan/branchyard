#!/usr/bin/env python3
"""Guard: HTTP/1.1 framing is parsed in one place, crates/branchyard-wire.

Branchyard once had four hand-written chunked and header parsers that
disagreed on malformed input (a bad chunk size or Content-Length quietly
became a default). They now share crates/branchyard-wire. This check keeps
it that way. No network, no build.

Hard rules, no allowlist (a hit is always a failure):

  1. No chunk-size parsing (`from_str_radix` on a chunk line) outside the
     wire crate. `ChunkedReader` is the one chunked decoder.
  2. No `httparse` outside the wire crate: not in a Cargo.toml, not in
     Rust. Heads are parsed with `wire::read_request_head` and
     `wire::read_response_head`.

Ratchet (tools/wire_ratchet.json): what is left to migrate, counted per
file. A count can only fall. A file over its count, or not listed, fails;
a file under its count also fails until the count is lowered, so the
allowlist shrinks as the code does and never grows.

  - "framing_literals": a `"content-length"` or `"transfer-encoding"`
    string literal in non-test source outside the wire crate. Most are
    header drop lists and serialisers, which is fine; the ratchet is
    there so a new hand-written length or chunk decision shows up in
    review as a new count.
  - "hand_written_heads": a source file outside the wire crate that
    reads an HTTP head itself (`read_until`, or a `fn read_head`, in a file that
    speaks `HTTP/1.`).

Run `python3 tools/check_wire.py`; `--list` prints the current counts.
"""
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WIRE = "crates/branchyard-wire/"
RATCHET = "tools/wire_ratchet.json"

FRAMING_LITERAL = re.compile(r"""["'](?:content-length|transfer-encoding)["']""", re.I)
RADIX = re.compile(r"\bfrom_str_radix\b")
CHUNK = re.compile(r"chunk", re.I)
HTTPARSE = re.compile(r"\bhttparse\b")
LINE_READ = re.compile(r"\bread_until\s*\(|\bfn\s+read_head\b")


def rust_sources(root, tests):
    """Rust files under crates/*/src (or crates/*/tests when `tests`)."""
    sub = "tests" if tests else "src"
    for path in sorted((root / "crates").glob(f"*/{sub}/**/*.rs")):
        rel = path.relative_to(root).as_posix()
        if not rel.startswith(WIRE):
            yield rel, path.read_text(errors="replace")


def strip_tests(text):
    """Drop `#[cfg(test)]` modules, which run to the end of the file here."""
    cut = re.search(r"^#\[cfg\(test\)\]\s*\nmod\s+\w+\s*\{", text, re.M)
    return text[: cut.start()] if cut else text


def scan(root=ROOT):
    """(hard violations, current ratchet counts) for the tree at `root`."""
    problems = []
    counts = {"framing_literals": {}, "hand_written_heads": {}}
    # Chunk-size parsing anywhere in Rust, tests included: a test that
    # parses chunk sizes by hand is a second decoder too.
    for tests in (False, True):
        for rel, text in rust_sources(root, tests):
            lines = text.splitlines()
            for i, line in enumerate(lines):
                if RADIX.search(line):
                    window = "\n".join(lines[max(0, i - 4) : i + 5])
                    if CHUNK.search(window):
                        problems.append(
                            f"{rel}:{i + 1}: parses a chunk size by hand; use "
                            "branchyard_wire::ChunkedReader (or parse_chunk_size)"
                        )
            if HTTPARSE.search(text):
                problems.append(
                    f"{rel}: uses httparse; use branchyard_wire's heads instead"
                )
    for manifest in sorted((root / "crates").glob("*/Cargo.toml")):
        rel = manifest.relative_to(root).as_posix()
        if rel.startswith(WIRE):
            continue
        if HTTPARSE.search(manifest.read_text()):
            problems.append(f"{rel}: depends on httparse; depend on branchyard-wire")
    for rel, text in rust_sources(root, False):
        body = strip_tests(text)
        literals = len(FRAMING_LITERAL.findall(body))
        if literals:
            counts["framing_literals"][rel] = literals
        if "HTTP/1." in body and LINE_READ.search(body):
            counts["hand_written_heads"][rel] = 1
    return problems, counts


def compare(counts, allowed):
    problems = []
    for kind, found in counts.items():
        ceiling = allowed.get(kind, {})
        for rel in sorted(set(found) | set(ceiling)):
            have, may = found.get(rel, 0), ceiling.get(rel, 0)
            if have > may:
                problems.append(
                    f"{rel}: {have} {kind} (ratchet allows {may}); use branchyard-wire "
                    f"instead of a new hand-written one, or see CONTRIBUTING.md"
                )
            elif have < may:
                problems.append(
                    f"{rel}: {kind} fell to {have} (ratchet says {may}); lower it in "
                    f"{RATCHET} so it can only fall"
                )
    return problems


def check(root=ROOT):
    problems, counts = scan(root)
    path = root / RATCHET
    allowed = json.loads(path.read_text()) if path.exists() else {}
    problems += compare(counts, allowed)
    return problems, counts


def main():
    problems, counts = check()
    if "--list" in sys.argv:
        print(json.dumps(counts, indent=2, sort_keys=True))
        return
    if problems:
        raise SystemExit("HTTP wire codec guard failed:\n" + "\n".join(problems))
    left = sum(sum(v.values()) for v in counts.values())
    print(f"HTTP framing is parsed only in branchyard-wire; {left} ratcheted sites remain.")


if __name__ == "__main__":
    main()
