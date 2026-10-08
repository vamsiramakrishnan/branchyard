#!/usr/bin/env python3
"""Score a real-harness battery run: the fixed evaluator for delegation changes.

    python3 tools/qualify_delegation/score.py QUALIFY_OUT [--json FILE] [--compare BASE.json]

Reads every scenario that `run-repo.sh` left under QUALIFY_OUT/out and
reports, per scenario:

- `pass`: its check passed on the meta branch (`verify exit=0`)
- `cost_usd`: the whole tree's recorded spend
- `wall_s`: the scenario's run, from `started` to `ended` (else the meta's first to last logged event)
- `branches` and `meta_turns`
- `protocol_violations`: harness frames Branchyard could not place, across every branch
- `refusals`: Branchyard refusals the meta hit (`refused:` in its run log)
- `friction`: items in the meta's "## Branchyard friction" section

`--compare` prints each metric's change against an earlier `--json` file.
A change keeps its place only if it holds `pass` and lowers protocol
violations, refusals or friction without raising cost by more than 10%.
"""

import argparse
import json
import re
import sys
from datetime import datetime
from pathlib import Path

STAMP = re.compile(r"^(\d{4}-\d\d-\d\dT[\d:.]+Z)\s{2}(.*)$")
ITEM = re.compile(r"^ {0,3}(\d+\.|[-*]) ")
EVENT = re.compile(r"^(usage |turn \d+ |session |checkpoint |candidate |status:)")


def text_lines(path):
    """A branch log's lines without their timestamp column, with each line's time."""
    out = []
    for raw in path.read_text(errors="replace").splitlines():
        found = STAMP.match(raw)
        if found:
            out.append((found.group(1), found.group(2)))
        elif raw.startswith(" " * 26):
            out.append((None, raw[26:]))
        else:
            out.append((None, raw))
    return out


def moment(stamp):
    return datetime.strptime(stamp[:19], "%Y-%m-%dT%H:%M:%S")


def seconds(start, end):
    return int((moment(end) - moment(start)).total_seconds())


def wall(out, stamps):
    started, ended = out / "started", out / "ended"
    if started.exists() and ended.exists():
        return seconds(started.read_text().strip(), ended.read_text().strip())
    return seconds(stamps[0], stamps[-1]) if len(stamps) > 1 else None


def friction(lines):
    """Top-level items under the last "## Branchyard friction" heading."""
    heads = [i for i, (_, line) in enumerate(lines) if line.strip().lower().startswith("## branchyard friction")]
    if not heads:
        return 0
    count = 0
    for stamp, line in lines[heads[-1] + 1 :]:
        if (stamp and EVENT.match(line)) or line.startswith("## "):
            break
        if ITEM.match(line):
            count += 1
    return count


def score(out):
    verify = out / "verify.txt"
    meta_log = out / "log.meta.txt"
    branches = []
    if (out / "ls.json").exists():
        try:
            listed = json.loads((out / "ls.json").read_text())
            branches = listed if isinstance(listed, list) else listed.get("branches", [])
        except ValueError:
            branches = []
    meta = next((b for b in branches if b.get("name") == "meta"), {})
    lines = text_lines(meta_log) if meta_log.exists() else []
    stamps = [s for s, _ in lines if s]
    logs = list(out.glob("log.*.txt"))
    run_log = (out / "run.log").read_text(errors="replace") if (out / "run.log").exists() else ""
    return {
        "pass": verify.exists() and "verify exit=0" in verify.read_text(errors="replace"),
        "cost_usd": round(sum(b.get("cost_usd") or 0 for b in branches), 4),
        "wall_s": wall(out, stamps),
        "branches": len(branches),
        "meta_turns": meta.get("turns"),
        "protocol_violations": sum(p.read_text(errors="replace").count("protocol violation") for p in logs),
        "refusals": run_log.count("refused:"),
        "friction": friction(lines),
    }


def table(scores):
    keys = ["pass", "cost_usd", "wall_s", "branches", "meta_turns", "protocol_violations", "refusals", "friction"]
    rows = ["| scenario | " + " | ".join(keys) + " |", "|---" * (len(keys) + 1) + "|"]
    for name, s in sorted(scores.items()):
        rows.append(f"| {name} | " + " | ".join(str(s[k]) for k in keys) + " |")
    return "\n".join(rows)


def totals(scores):
    walls = sorted(s["wall_s"] for s in scores.values() if s["wall_s"] is not None)
    return {
        "scenarios": len(scores),
        "passed": sum(s["pass"] for s in scores.values()),
        "cost_usd": round(sum(s["cost_usd"] for s in scores.values()), 2),
        "median_wall_s": walls[len(walls) // 2] if walls else None,
        "protocol_violations": sum(s["protocol_violations"] for s in scores.values()),
        "refusals": sum(s["refusals"] for s in scores.values()),
        "friction": sum(s["friction"] for s in scores.values()),
    }


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("qualify_out", type=Path)
    parser.add_argument("--json", type=Path, help="write the scores here")
    parser.add_argument("--compare", type=Path, help="an earlier --json file to compare with")
    args = parser.parse_args(argv)
    outs = sorted(p for p in (args.qualify_out / "out").iterdir() if p.is_dir())
    scores = {p.name: score(p) for p in outs if (p / "verify.txt").exists() or (p / "run.log").exists()}
    result = {"scenarios": scores, "totals": totals(scores)}
    print(table(scores))
    print()
    print(json.dumps(result["totals"]))
    if args.json:
        args.json.write_text(json.dumps(result, indent=2) + "\n")
    if args.compare:
        base = json.loads(args.compare.read_text())["totals"]
        now = result["totals"]
        print("\nchange against", args.compare)
        for key in now:
            if isinstance(now[key], (int, float)) and isinstance(base.get(key), (int, float)):
                print(f"  {key}: {base[key]} -> {now[key]} ({now[key] - base[key]:+g})")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
