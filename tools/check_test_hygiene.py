#!/usr/bin/env python3
"""Keep the tests on branchyard-testkit (crates/branchyard-testkit).

Every test crate is built separately, so a helper copied into one test file
cannot be reused by the next, and the copies drift: 23 fake_agent() that each
shelled out to cargo, wait loops with four deadlines and two poll intervals,
mock servers that swallowed I/O errors and so passed while broken. The kit is
the one home for those. This check fails when they come back, and when a test
takes one of the shapes that made tests fail on a slow machine: a number freed
and then reused as a port, a wait that is a guess at a duration, a variable set
for the whole process.

Scope: crates/*/tests/**/*.rs, crates/branchyard/src/conformance.rs, and the
test code inside crates/*/src: `#[cfg(test)]` items (a module, a function, an
out-of-line `#[cfg(test)] mod tests;` file) and `#[test]` functions. The kit's
own crate is the one place allowed to poll, sleep and build binaries.

Hard rules (no counts; a violation fails):
  * no `fn` named wait_until, wait_for, wait_gone, wait_exec, fake_agent,
    eventually, until or poll_until: use branchyard_testkit::wait / fake_agent
  * no raw thread::sleep, tokio::time::sleep or bare sleep(): wait for an event
    with wait::until, or, when time itself must pass (a quiet period in which
    nothing may happen, an expiry), wait::settle("why", duration)
  * no bind-then-drop: a `TcpListener::bind(..)` whose listener is dropped
    (`.local_addr()` read from a temporary, or `drop(listener)` after the port
    was read) hands out a number that another test's `bind(0)` can be given
    before the user binds it. Hold the listener, let the child bind port 0 and
    report its address, or refuse on a privileged port (127.0.0.1:1)
  * no `env::set_var` / `env::remove_var` in a test body for the whole process,
    unless it runs once (inside Once / OnceLock / LazyLock), is in a fixture (a
    function that is not itself a test), sets a BY_TEST_* input of the test, or
    the file is a test binary of one test. Otherwise use Command::env, or a
    test binary of its own
  * no `/tmp/...` path literal: a fixed path is shared by every run on the
    machine. Use branchyard_testkit::Scratch (or Repo). A guest path in a
    sandbox, or a product contract about /tmp, carries a comment on that line
    or the one above, `hygiene-allow: tmp <why>`
  * no wait::settle of 500 ms or more unless tools/test_hygiene_allow.json
    names it (file and the settle's `why` text) with the reason time must pass.
    A listed settle that is gone or shorter fails too: remove the entry

Ratchets (tools/test_hygiene_ratchet.json, a counted allowlist that can only
fall: a count above it fails, and so does a count below it until the file is
lowered with --lower):
  * settle:       wait::settle calls per file; each is a place that might
                  await an event instead
  * temp_dir:     hand-rolled std::env::temp_dir() scratch directories per
                  file; use branchyard_testkit::Scratch (or Repo)
  * tcp_listener: hand-rolled TcpListener::bind servers per file; use
                  branchyard_testkit::MockHttp

Usage: tools/check_test_hygiene.py [--lower]
"""

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
RATCHET = "tools/test_hygiene_ratchet.json"
ALLOW = "tools/test_hygiene_allow.json"
KIT = "crates/branchyard-testkit/"
LONG_SETTLE_MS = 500

BANNED_FNS = (
    "wait_until",
    "wait_for",
    "wait_gone",
    "wait_exec",
    "fake_agent",
    "eventually",
    "until",
    "poll_until",
)
FN_RE = re.compile(rf"\bfn\s+({'|'.join(BANNED_FNS)})\b")
SLEEP_RE = re.compile(r"\bthread::sleep\b|\btime::sleep\b")
# A bare `sleep(` after `use std::thread::sleep`: not a method, not a path, not a definition.
BARE_SLEEP_RE = re.compile(r"(?<![\w.:])(?<!fn )sleep\s*\(")
# `TcpListener::bind(..)` straight into `.local_addr()`: the listener is a temporary.
TEMPORARY_LISTENER_RE = re.compile(
    r"TcpListener::bind\s*\([^;{}]*?\)\s*(?:\.await)?\s*(?:\.unwrap\(\)|\.expect\([^)]*\)|\?)?\s*\.local_addr\s*\("
)
NAMED_LISTENER_RE = re.compile(r"\blet\s+(?:mut\s+)?(\w+)\s*(?::[^=;]+)?=\s*[^;]*?TcpListener::bind\s*\(")
SET_VAR_RE = re.compile(r"(?<![\w.])(?<!fn )(?:env::)?(set_var|remove_var)\s*\(\s*(\"[^\"]*\")?")
# `/tmp/x` as a path root: at the start of the literal or after a space, `=`, `:`, `(`, `,` or `/`
# (`file:///tmp/x`), not the `.branchyard/tmp/x` of a yard.
TMP_PATH_RE = re.compile(r"\"(?:[^\"\n]*?[\s=:(,/])?/tmp/[\w.\-][^\"\n]*\"")
TMP_BARE_RE = re.compile(
    r"(?:Path::new|PathBuf::from|create_dir_all|create_dir|fs::write|File::create|current_dir|\.join)\s*\(\s*\"/tmp\""
)
TMP_ALLOW_RE = re.compile(r"hygiene-allow:\s*tmp\b")
ONCE_OPENERS = ("call_once(", "get_or_init(", "get_or_try_init(", "LazyLock::new(", "Lazy::new(")
FN_DECL_RE = re.compile(r"^\s*(?:pub(?:\([a-z]+\))?\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+(\w+)")
TEST_ATTR_RE = re.compile(r"#\[(?:[\w:]+::)?test(?:\(|\])")
CFG_TEST_RE = re.compile(r"#\[cfg\((?:all\()?test\b")
OUT_OF_LINE_MOD_RE = re.compile(r"#\[cfg\(test\)\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([a-z]+\))?\s+)?mod\s+(\w+)\s*;")

RATCHETED = {
    "settle": re.compile(r"\bwait::settle\("),
    "temp_dir": re.compile(r"\benv::temp_dir\(\)"),
    "tcp_listener": re.compile(r"\bTcpListener::bind\("),
}
# A file with none of these words can break none of the rules.
WORDS = re.compile(rf"\bfn\s+(?:{'|'.join(BANNED_FNS)})\b|sleep|TcpListener|_var|/tmp|settle|temp_dir")
NEEDLE = {"settle": "settle", "temp_dir": "temp_dir", "tcp_listener": "TcpListener"}
ADVICE = {
    "settle": "wait::settle is for time that must pass; if an event marks the moment, wait::until it instead",
    "temp_dir": "use branchyard_testkit::Scratch (or Repo) for a scratch directory",
    "tcp_listener": "use branchyard_testkit::MockHttp for a mock server",
}

_LEXEME = re.compile(
    r"""
      (?P<line>//[^\n]*)
    | (?P<block>/\*(?:[^*]|\*(?!/))*\*/)
    | (?P<raw>(?<![\w])b?r(?P<hashes>\#*)"(?:.|\n)*?"(?P=hashes))
    | (?P<string>(?<![\w])b?"(?:\\.|[^"\\])*")
    | (?P<char>'(?:\\.[^']*|[^\\'\n])')
    """,
    re.VERBOSE,
)


def blanked(piece):
    """`piece` as spaces, newlines kept."""
    if "\n" not in piece:
        return " " * len(piece)
    return "\n".join(" " * len(line) for line in piece.split("\n"))


def masks(text):
    """(code, commented): `text` with comments and string and char literals blanked to spaces, and
    with comments alone blanked. Newlines stay, so offsets and line numbers line up."""
    code, commented, last = [], [], 0
    for match in _LEXEME.finditer(text):
        code.append(text[last : match.start()])
        commented.append(text[last : match.start()])
        piece = match.group(0)
        code.append(blanked(piece))
        # A comment is blanked in both; a string or a char literal only in `code`.
        commented.append(blanked(piece) if match.lastgroup in ("line", "block") else piece)
        last = match.end()
    code.append(text[last:])
    commented.append(text[last:])
    return "".join(code), "".join(commented)


def line_starts(text):
    return [0] + [match.end() for match in re.finditer("\n", text)]


def line_of(starts, offset):
    """1-based line number of `offset`."""
    low, high = 0, len(starts)
    while low < high - 1:
        middle = (low + high) // 2
        if starts[middle] <= offset:
            low = middle
        else:
            high = middle
    return low + 1


def matching(code, open_at, opener="{", closer="}"):
    """Offset just past the bracket that closes the one at `open_at` (in masked code)."""
    depth = 0
    for index in range(open_at, len(code)):
        char = code[index]
        if char == opener:
            depth += 1
        elif char == closer:
            depth -= 1
            if depth == 0:
                return index + 1
    return len(code)


def test_ranges(code):
    """(start, end) offsets of the test items in masked `code`: whatever follows a
    `#[cfg(test)]` or a `#[test]` attribute, up to the end of its block or its `;`."""
    ranges = []
    for pattern in (CFG_TEST_RE, TEST_ATTR_RE):
        for match in pattern.finditer(code):
            at = match.start()
            if any(start <= at < end for start, end in ranges):
                continue
            brace, semicolon = code.find("{", at), code.find(";", at)
            if brace != -1 and (semicolon == -1 or brace < semicolon):
                ranges.append((at, matching(code, brace)))
            elif semicolon != -1:
                ranges.append((at, semicolon + 1))
    return ranges


def out_of_line_test_modules(code):
    return {match.group(1) for match in OUT_OF_LINE_MOD_RE.finditer(code)}


def sources(root):
    """The files the rules cover, as repo-relative posix paths: all of the test files, and the
    sources whose `#[cfg(test)]` items are scanned."""
    found = set()
    for path in (root / "crates").glob("*/tests/**/*.rs"):
        found.add(path.relative_to(root).as_posix())
    for path in (root / "crates").glob("*/src/**/*.rs"):
        found.add(path.relative_to(root).as_posix())
    return sorted(p for p in found if not p.startswith(KIT))


def is_test_file(rel):
    """Whole-file test code: a file of a crate's tests/ directory, or the conformance suite."""
    parts = rel.split("/")
    return parts[2] == "tests" or rel == "crates/branchyard/src/conformance.rs"


def own_binary(rel):
    """A test file of a crate's tests/ directory itself (cargo builds each into a binary)."""
    parts = rel.split("/")
    return len(parts) == 4 and parts[2] == "tests"


def enclosing_fn(lines, index):
    """(name, line index, is a test) of the function a statement on lines[index] is in."""
    indent = len(lines[index]) - len(lines[index].lstrip())
    for at in range(index, -1, -1):
        match = FN_DECL_RE.match(lines[at])
        if match and (len(lines[at]) - len(lines[at].lstrip())) < indent:
            attributes = ""
            back = at - 1
            while back >= 0 and (lines[back].lstrip().startswith(("#[", "///")) or not lines[back].strip()):
                attributes += lines[back]
                back -= 1
            return match.group(1), at, bool(TEST_ATTR_RE.search(attributes))
    return None, 0, False


def inside_once(code, fn_start, at):
    """Whether the statement at offset `at` is inside the parentheses of a call_once(..),
    get_or_init(..) or LazyLock::new(..) that begins after offset `fn_start`."""
    best = -1
    for opener in ONCE_OPENERS:
        found = code.rfind(opener, fn_start, at)
        if found > best:
            best = found + len(opener) - 1
    if best == -1:
        return False
    return matching(code, best, "(", ")") > at


def settle_calls(commented, code):
    """(offset, why, milliseconds or None) for each wait::settle(..) call. None: a duration this
    cannot evaluate, which counts as long."""
    calls = []
    for match in re.finditer(r"\bwait::settle\s*\(", code):
        open_at = match.end() - 1
        close = matching(code, open_at, "(", ")")
        arguments = commented[open_at + 1 : close - 1]
        why = re.match(r"\s*\"((?:[^\"\\]|\\.)*)\"\s*,\s*(.*)\Z", arguments, re.DOTALL)
        if not why:
            calls.append((match.start(), arguments.strip(), None))
            continue
        calls.append((match.start(), why.group(1), duration_ms(why.group(2))))
    return calls


def duration_ms(expression):
    match = re.fullmatch(
        r"\s*(?:std::time::)?Duration::(from_millis|from_secs|from_secs_f64|from_micros)\(\s*([\d_.]+)\s*\)\s*,?\s*",
        expression,
    )
    if not match:
        return None
    value = float(match.group(2).replace("_", ""))
    return value * {"from_millis": 1, "from_secs": 1000, "from_secs_f64": 1000, "from_micros": 0.001}[match.group(1)]


def scan_text(rel, text, whole=False):
    """(hard violations, {ratchet: count}, [(why, ms, line)] of the long settles) for one file.
    `whole`: all of the file is test code (a test directory's file, or an out-of-line test module)."""
    whole = whole or is_test_file(rel)
    if not whole and "cfg(test" not in text and "test]" not in text:
        return [], dict.fromkeys(RATCHETED, 0), []
    if not WORDS.search(text):
        return [], dict.fromkeys(RATCHETED, 0), []
    code, commented = masks(text)
    starts = line_starts(text)
    raw_lines = text.splitlines()
    if whole:
        ranges = [(0, len(text) + 1)]
    else:
        ranges = test_ranges(code)
        if not ranges:
            return [], dict.fromkeys(RATCHETED, 0), []

    def in_test(offset):
        return any(start <= offset < end for start, end in ranges)

    violations = []
    counts = dict.fromkeys(RATCHETED, 0)
    long_settles = []

    def report(offset, message):
        violations.append(f"{rel}:{line_of(starts, offset)}: {message}")

    for match in FN_RE.finditer(commented):
        if in_test(match.start()):
            report(
                match.start(),
                f"`fn {match.group(1)}` is a local copy of branchyard-testkit "
                "(wait::until / wait::gone / wait::exec / fake_agent!); use the kit",
            )
    sleep_message = (
        "raw sleep in a test; wait for the event with wait::until, or say why time must pass with "
        'wait::settle("why", duration)'
    )
    # Each rule below starts from a word the file must contain, so the regular expressions run only on
    # the files that could match: the scan reads the whole workspace's sources.
    if "sleep" in text:
        for match in SLEEP_RE.finditer(commented):
            if in_test(match.start()):
                report(match.start(), sleep_message)
        for match in BARE_SLEEP_RE.finditer(code):
            if in_test(match.start()):
                report(match.start(), sleep_message)

    bind_then_drop = (
        "a port is read from a listener that is then dropped; another test's bind(0) can be given the "
        "number before it is used. Hold the listener, have the child bind port 0 and report its address, "
        "or refuse on a privileged port (127.0.0.1:1)"
    )
    for match in TEMPORARY_LISTENER_RE.finditer(commented) if "TcpListener" in text else ():
        if in_test(match.start()):
            report(match.start(), bind_then_drop)
    for match in NAMED_LISTENER_RE.finditer(commented) if "TcpListener" in text else ():
        if not in_test(match.start()):
            continue
        name = re.escape(match.group(1))
        _, fn_at, _ = enclosing_fn(raw_lines, line_of(starts, match.start()) - 1)
        opening = code.find("{", starts[fn_at])
        block_end = matching(code, opening) if opening != -1 else len(code)
        dropped = re.search(rf"\bdrop\(\s*{name}\s*\)", commented[match.end() : block_end])
        if dropped and re.search(
            rf"\b{name}\s*\.local_addr\s*\(", commented[match.end() : match.end() + dropped.start()]
        ):
            report(match.start(), bind_then_drop)

    sole_test = own_binary(rel) and len(re.findall(r"#\[(?:[\w:]+::)?test\b", code)) == 1
    for match in SET_VAR_RE.finditer(commented) if "_var" in text else ():
        if not in_test(match.start()):
            continue
        index = line_of(starts, match.start()) - 1
        name, fn_at, is_test = enclosing_fn(raw_lines, index)
        variable = (match.group(2) or "").strip('"')
        if (
            not is_test
            or sole_test
            or variable.startswith("BY_TEST_")
            or inside_once(code, starts[fn_at], match.start())
        ):
            continue
        report(
            match.start(),
            f"{match.group(1)} in the test `{name}` changes the environment of every test in the binary; "
            "use Command::env, a test binary of its own, or set it once in a fixture "
            "(Once / OnceLock)",
        )

    for pattern in (TMP_PATH_RE, TMP_BARE_RE) if "/tmp" in text else ():
        for match in pattern.finditer(commented):
            if not in_test(match.start()):
                continue
            index = line_of(starts, match.start()) - 1
            nearby = raw_lines[max(index - 1, 0) : index + 1]
            if any(TMP_ALLOW_RE.search(line) for line in nearby):
                continue
            report(
                match.start(),
                "a /tmp path literal is shared by every run on the machine; use branchyard_testkit::Scratch "
                "(or Repo), or say why with `hygiene-allow: tmp <why>` on this line or the one above",
            )

    for offset, why, ms in settle_calls(commented, code) if "settle" in text else ():
        if not in_test(offset):
            continue
        if ms is None or ms >= LONG_SETTLE_MS:
            long_settles.append((why, ms, line_of(starts, offset)))
    for name, pattern in RATCHETED.items():
        for match in pattern.finditer(code) if NEEDLE[name] in text else ():
            if in_test(match.start()):
                counts[name] += 1
    return violations, counts, long_settles


def scan(root):
    """(hard violations, {ratchet: {path: count}}, {path: [(why, ms, line)]})."""
    violations = []
    counts = {name: {} for name in RATCHETED}
    longs = {}
    declared = set()  # out-of-line #[cfg(test)] modules, as paths
    texts = {}
    for rel in sources(root):
        texts[rel] = (root / rel).read_text(encoding="utf-8", errors="replace")
        if not is_test_file(rel):
            parent = rel.rsplit("/", 1)[0]
            for name in out_of_line_test_modules(texts[rel]):
                declared.add(f"{parent}/{name}.rs")
                declared.add(f"{parent}/{name}/mod.rs")
    for rel, text in texts.items():
        found, ratcheted, settles = scan_text(rel, text, whole=rel in declared)
        violations.extend(found)
        for name, count in ratcheted.items():
            if count:
                counts[name][rel] = count
        if settles:
            longs[rel] = settles
    return violations, counts, longs


def load_ratchet(root):
    path = root / RATCHET
    if not path.is_file():
        return {name: {} for name in RATCHETED}
    data = json.loads(path.read_text())
    return {name: dict(data.get(name, {})) for name in RATCHETED}


def load_allow(root):
    """{(file, why): reason} of the long settles that may stay."""
    path = root / ALLOW
    if not path.is_file():
        return {}
    data = json.loads(path.read_text())
    return {(entry["file"], entry["why"]): entry["reason"] for entry in data.get("long_settle", [])}


def check(root, lower=False):
    """The problems found under `root`; with `lower`, also rewrite the ratchet
    file with every count that fell (it never raises one)."""
    violations, counts, longs = scan(root)
    allowed = load_ratchet(root)
    problems = list(violations)
    allow = load_allow(root)
    used = set()
    for rel, settles in sorted(longs.items()):
        for why, ms, line in settles:
            if (rel, why) in allow:
                used.add((rel, why))
                continue
            length = "a duration this cannot evaluate" if ms is None else f"{ms:g} ms"
            problems.append(
                f"{rel}:{line}: wait::settle({why!r}) is {length}; wait for the event with wait::until, or list it "
                f"in {ALLOW} with the reason time itself must pass"
            )
    for rel, why in sorted(set(allow) - used):
        problems.append(
            f"{ALLOW}: {rel} no longer has a long settle {why!r} (gone, shorter than {LONG_SETTLE_MS} ms, or "
            "renamed); remove the entry"
        )
    lowered = {name: dict(files) for name, files in allowed.items()}
    changed = False
    for name in RATCHETED:
        for rel in sorted(set(counts[name]) | set(allowed[name])):
            have = counts[name].get(rel, 0)
            may = allowed[name].get(rel, 0)
            if have > may:
                problems.append(f"{rel}: {have} {name} (the ratchet allows {may}); {ADVICE[name]}")
            elif have < may:
                if lower:
                    changed = True
                    if have:
                        lowered[name][rel] = have
                    else:
                        lowered[name].pop(rel, None)
                else:
                    problems.append(
                        f"{rel}: {have} {name}, the ratchet still allows {may}; "
                        f"lower it ({RATCHET}, or run {Path(__file__).name} --lower)"
                    )
    if lower and changed:
        out = {"format": 1}
        out.update({name: dict(sorted(lowered[name].items())) for name in RATCHETED})
        (root / RATCHET).write_text(json.dumps(out, indent=2) + "\n")
        print(f"lowered {RATCHET}")
        problems = [p for p in problems if "the ratchet still allows" not in p]
    return problems


def main(argv):
    problems = check(ROOT, lower="--lower" in argv)
    if problems:
        print("test hygiene:", file=sys.stderr)
        for problem in problems:
            print("  " + problem, file=sys.stderr)
        return 1
    _, counts, longs = scan(ROOT)
    summary = ", ".join(f"{sum(c.values())} {name}" for name, c in counts.items())
    listed = sum(len(settles) for settles in longs.values())
    print(f"test hygiene ok ({summary}; {listed} long settles listed; the ratchet can only fall)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
