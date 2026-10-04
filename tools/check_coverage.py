#!/usr/bin/env python3
"""The coverage floor: line coverage per crate and per watched file may not
fall below tools/coverage_floor.json, and a source file with no covered line
at all must be on that file's `uncovered` list, which can only shrink.

    cargo llvm-cov --workspace --locked --offline --lcov --output-path lcov.info
    cargo llvm-cov -p branchyard -p branchyard-provision -p branchyard-substrate \\
        --lib --locked --offline --lcov --output-path unit.lcov
    python3 tools/check_coverage.py lcov.info --unit unit.lcov

The floors are rules, not goals: `crates` holds each crate's coverage over
its src/ files, `files` the files worth watching on their own, `unit_files`
the files whose floor is measured from the unit tests alone (so an
integration test cannot stand in for the missing unit tests), and
`uncovered` the src files nothing covers yet. Every comparison allows
`tolerance` percentage points, because timing-dependent tests vary a little
between runs. `--update` rewrites the floors from the reports: floors only
go up, files that gained coverage leave `uncovered`, and a file that has
none is never added, so write its test instead. `--seed` writes a first
floor file and is the only way to fill `uncovered`.

Floors come from the CI coverage job, never from a local run: a developer's
machine runs tests the hosted runner skips (root-only and namespace tests),
so a floor measured there fails in CI. Download the `lcov` artifact of a
green run and pass `--from-ci` with `--update` or `--seed` (the job itself,
under GITHUB_ACTIONS, needs no flag). `--local` overrides the guard for a
throwaway floor file, with a warning.

Only crates/<name>/src/ files count: tests, examples, build scripts and
vendored code do not.
"""

import argparse
import json
import math
import os
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FLOOR = ROOT / "tools" / "coverage_floor.json"
SRC = re.compile(r"^crates/([^/]+)/src/.+\.rs$")
# An absolute report path from another machine (llvm-cov writes
# /home/runner/work/<repo>/<repo>/crates/<name>/src/...): from the first crates/<name>/src/ on.
FOREIGN = re.compile(r"/(crates/[^/]+/src/.+\.rs)$")
NOT_OURS = ("/.cargo/", "/vendor/", "/target/", "/rustc/")


def relative(path, root):
    """`path` relative to the repository: by `root` when it is under it, else from its first crates/<name>/src/."""
    if path.startswith(root):
        return path[len(root) :]
    found = FOREIGN.search(path)
    if found and not any(part in path for part in NOT_OURS):
        return found.group(1)
    return path


def parse_lcov(text, root):
    """{relative path: [lines found, lines hit]} for each crate src file."""
    files = {}
    current = None
    root = str(root).rstrip("/") + "/"
    for line in text.splitlines():
        if line.startswith("SF:"):
            path = line[3:]
            path = relative(path, root)
            current = files.setdefault(path, {}) if SRC.match(path) else None
        elif current is not None and line.startswith("DA:"):
            number, count = line[3:].split(",")[:2]
            # A line can appear once per monomorphization: hit if any hit.
            current[number] = max(current.get(number, 0), int(count))
        elif line == "end_of_record":
            current = None
    return {path: [len(lines), sum(1 for c in lines.values() if c > 0)] for path, lines in files.items()}


def percent(found, hit):
    return 100.0 * hit / found if found else 100.0


def crate_percentages(files):
    totals = {}
    for path, (found, hit) in files.items():
        crate = SRC.match(path).group(1)
        t = totals.setdefault(crate, [0, 0])
        t[0] += found
        t[1] += hit
    return {crate: percent(*t) for crate, t in totals.items()}


def floor1(value):
    """Rounded down to one decimal: a floor never starts above the baseline."""
    return math.floor(value * 10 + 1e-9) / 10


def check(floor, files, unit_files):
    errors = []
    tolerance = floor.get("tolerance", 1.0)
    crates = crate_percentages(files)
    for crate, want in sorted(floor["crates"].items()):
        have = crates.get(crate)
        if have is None:
            errors.append(f"{crate}: no coverage in the report, floor is {want}%")
        elif have < want - tolerance:
            errors.append(f"{crate}: line coverage {have:.1f}% is below its floor of {want}% (tolerance {tolerance})")
    for crate in sorted(set(crates) - set(floor["crates"])):
        errors.append(f"{crate}: has no floor in {FLOOR.name}; run tools/check_coverage.py --update")
    for section, report in (("files", files), ("unit_files", unit_files)):
        for path, want in sorted(floor.get(section, {}).items()):
            if report is None:
                errors.append(f"{path}: the {section} floor needs the unit report (--unit)")
                continue
            found, hit = report.get(path, (0, 0))
            have = percent(found, hit) if found else 0.0
            if have < want - tolerance:
                kind = "unit-test " if section == "unit_files" else ""
                errors.append(f"{path}: {kind}line coverage {have:.1f}% is below its floor of {want}%")
    allowed = set(floor.get("uncovered", []))
    for path, (found, hit) in sorted(files.items()):
        if found and hit == 0 and path not in allowed:
            errors.append(f"{path}: no line is covered by any test; add one (new files cannot join the uncovered list)")
    for path in sorted(allowed):
        found, hit = files.get(path, (0, 0))
        if hit > 0:
            errors.append(
                f"{path}: now covered ({percent(found, hit):.1f}%); "
                "remove it from `uncovered` and give it a floor under `files`"
            )
        elif path not in files:
            errors.append(f"{path}: on the uncovered list but not in the report (deleted or renamed?); remove it")
    return errors


def update(floor, files, unit_files):
    """Raise floors to the measured values; shrink `uncovered`; never add to it."""
    crates = crate_percentages(files)
    for crate, have in crates.items():
        floor["crates"][crate] = max(floor["crates"].get(crate, 0), floor1(have))
    for section, report in (("files", files), ("unit_files", unit_files)):
        for path in list(floor.get(section, {})):
            if report is not None and path in report:
                floor[section][path] = max(floor[section][path], floor1(percent(*report[path])))
    floor["uncovered"] = sorted(p for p in floor.get("uncovered", []) if p in files and files[p][1] == 0)
    return floor


def seed(files, unit_files, watch, unit_watch):
    """A first floor from the reports: the one place `uncovered` is filled."""
    floor = {"tolerance": 1.0, "crates": {}, "files": {}, "unit_files": {}, "uncovered": []}
    for crate, have in sorted(crate_percentages(files).items()):
        floor["crates"][crate] = floor1(have)
    for path in watch:
        floor["files"][path] = floor1(percent(*files[path]))
    for path in unit_watch:
        floor["unit_files"][path] = floor1(percent(*(unit_files or {}).get(path, (0, 0))))
    floor["uncovered"] = sorted(p for p, (found, hit) in files.items() if found and hit == 0)
    return floor


def load(path, root):
    return parse_lcov(Path(path).read_text(), root)


def may_write_floors(args):
    """True when the reports may set floors: they are the CI job's (--from-ci, or under GITHUB_ACTIONS), or --local."""
    if args.from_ci or os.environ.get("GITHUB_ACTIONS") == "true":
        return True
    if args.local:
        print("warning: floors written from a local run may fail in CI", file=sys.stderr)
        return True
    print(
        "error: floors are seeded and updated from the CI coverage artifact, not a local run.\n"
        "Download the `lcov` artifact of the CI coverage job and pass --from-ci (or --local to override).",
        file=sys.stderr,
    )
    return False


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("lcov", help="lcov report of the whole workspace's tests")
    parser.add_argument("--unit", help="lcov report of the unit tests (--lib) alone")
    parser.add_argument("--root", default=str(ROOT), help="repository root, to relativize report paths")
    parser.add_argument("--floor", default=str(FLOOR))
    parser.add_argument("--update", action="store_true", help="raise the floors to the measured values")
    parser.add_argument("--from-ci", action="store_true", help="the reports are the CI coverage job's artifact")
    parser.add_argument("--local", action="store_true", help="let --update/--seed use reports from this machine")
    parser.add_argument("--seed", action="store_true", help="write a first floor file (see --watch, --unit-watch)")
    parser.add_argument("--watch", nargs="*", default=[], help="with --seed: files that get their own floor")
    parser.add_argument("--unit-watch", nargs="*", default=[], help="with --seed: files that get a unit-test floor")
    args = parser.parse_args(argv[1:])
    if (args.update or args.seed) and not may_write_floors(args):
        return 1
    files = load(args.lcov, args.root)
    if (args.update or args.seed) and not files:
        print(
            f"error: no crates/<name>/src/ file in {args.lcov}; refusing to write floors from an empty report",
            file=sys.stderr,
        )
        return 1
    unit = load(args.unit, args.root) if args.unit else None
    floor_path = Path(args.floor)
    if args.seed:
        floor_path.write_text(json.dumps(seed(files, unit, args.watch, args.unit_watch), indent=2) + "\n")
        print(f"seeded {floor_path}")
        return 0
    floor = json.loads(floor_path.read_text())
    if args.update:
        floor = update(floor, files, unit)
        floor_path.write_text(json.dumps(floor, indent=2) + "\n")
        print(f"updated {floor_path}")
        return 0
    errors = check(floor, files, unit)
    for e in errors:
        print(f"error: {e}", file=sys.stderr)
    if errors:
        return 1
    crates = crate_percentages(files)
    uncovered = len(floor.get("uncovered", []))
    print(f"coverage floor holds: {len(crates)} crates, {len(files)} files, {uncovered} uncovered")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
