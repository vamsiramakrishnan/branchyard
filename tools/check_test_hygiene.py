#!/usr/bin/env python3
"""Keep the tests on branchyard-testkit (crates/branchyard-testkit).

Every test crate is built separately, so a helper copied into one test file
cannot be reused by the next, and the copies drift: 23 fake_agent() that each
shelled out to cargo, wait loops with four deadlines and two poll intervals,
mock servers that swallowed I/O errors and so passed while broken. The kit is
the one home for those. This check fails when they come back.

Scope: crates/*/tests/**/*.rs and crates/branchyard/src/conformance.rs. The
kit's own crate is the one place allowed to poll, sleep and build binaries.

Hard rules (no allowlist):
  * no `fn` named wait_until, wait_for, wait_gone, wait_exec, fake_agent,
    eventually, until or poll_until: use branchyard_testkit::wait / fake_agent
  * no raw thread::sleep or tokio::time::sleep: wait for an event with
    wait::until, or, when time itself must pass (a quiet period in which
    nothing may happen, an expiry), wait::settle("why", duration)

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
KIT = "crates/branchyard-testkit/"

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

RATCHETED = {
    "settle": re.compile(r"\bwait::settle\("),
    "temp_dir": re.compile(r"\benv::temp_dir\(\)"),
    "tcp_listener": re.compile(r"\bTcpListener::bind\("),
}
ADVICE = {
    "settle": "wait::settle is for time that must pass; if an event marks the moment, wait::until it instead",
    "temp_dir": "use branchyard_testkit::Scratch (or Repo) for a scratch directory",
    "tcp_listener": "use branchyard_testkit::MockHttp for a mock server",
}


def sources(root):
    """The test sources the rules cover, as repo-relative posix paths."""
    found = set()
    for path in (root / "crates").glob("*/tests/**/*.rs"):
        found.add(path.relative_to(root).as_posix())
    conformance = root / "crates/branchyard/src/conformance.rs"
    if conformance.is_file():
        found.add(conformance.relative_to(root).as_posix())
    return sorted(p for p in found if not p.startswith(KIT))


def code_lines(text):
    """(line number, line) for lines that are not whole-line comments."""
    for number, line in enumerate(text.splitlines(), 1):
        if not line.lstrip().startswith("//"):
            yield number, line


def scan(root):
    """(hard violations, {ratchet: {path: count}})."""
    violations = []
    counts = {name: {} for name in RATCHETED}
    for rel in sources(root):
        text = (root / rel).read_text(encoding="utf-8", errors="replace")
        for number, line in code_lines(text):
            match = FN_RE.search(line)
            if match:
                violations.append(
                    f"{rel}:{number}: `fn {match.group(1)}` is a local copy of "
                    "branchyard-testkit (wait::until / wait::gone / wait::exec / "
                    "fake_agent!); use the kit"
                )
            if SLEEP_RE.search(line):
                violations.append(
                    f"{rel}:{number}: raw sleep in a test; wait for the event "
                    "with wait::until, or say why time must pass with "
                    'wait::settle("why", duration)'
                )
            for name, pattern in RATCHETED.items():
                found = len(pattern.findall(line))
                if found:
                    counts[name][rel] = counts[name].get(rel, 0) + found
    return violations, counts


def load_ratchet(root):
    path = root / RATCHET
    if not path.is_file():
        return {name: {} for name in RATCHETED}
    data = json.loads(path.read_text())
    return {name: dict(data.get(name, {})) for name in RATCHETED}


def check(root, lower=False):
    """The problems found under `root`; with `lower`, also rewrite the ratchet
    file with every count that fell (it never raises one)."""
    violations, counts = scan(root)
    allowed = load_ratchet(root)
    problems = list(violations)
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
    _, counts = scan(ROOT)
    summary = ", ".join(f"{sum(c.values())} {name}" for name, c in counts.items())
    print(f"test hygiene ok ({summary}; the ratchet can only fall)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
