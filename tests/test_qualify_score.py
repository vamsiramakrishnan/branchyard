#!/usr/bin/env python3
"""tools/qualify_delegation/score.py, the battery's evaluator, on made-up
scenario outputs: a refusal is counted once, from the branches' recorded
events, however often the meta quotes it; a run from before events were
saved counts its run and resume logs; and runs counted differently are
not compared as if they were not."""

import contextlib
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO / "tools" / "qualify_delegation"))
import score  # noqa: E402

REFUSAL = 'refused: spawn task-A: "task-A" is not a usable branch name; try "task-a"'


def delegation(refused):
    delegated = {"tool": "spawn", "branch": "x", "outcome": "o", "refused": refused}
    return {"at_ms": 1, "activity": {"delegation": delegated}}


class Score(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.out = Path(self.tmp.name) / "out" / "crash"
        self.out.mkdir(parents=True)
        (self.out / "verify.txt").write_text("verify exit=0\n")
        (self.out / "run.log").write_text(f"meta │ {REFUSAL}\n## Branchyard friction\n1. `{REFUSAL}`\n")
        (self.out / "resume.log").write_text(f"meta │ {REFUSAL}\n")

    def tearDown(self):
        self.tmp.cleanup()

    def test_refusals_are_counted_once_from_events_across_branches(self):
        events = [delegation(True), delegation(False), {"at_ms": 2, "activity": {"warning": "refused: no"}}]
        (self.out / "events.meta.jsonl").write_text("\n".join(json.dumps(e) for e in events) + "\nnot json\n")
        (self.out / "events.kid.jsonl").write_text(json.dumps(delegation(True)) + "\n")
        scored = score.score(self.out)
        self.assertEqual((scored["refusals"], scored["refusals_from"]), (2, "events"))

    def test_without_events_the_run_and_resume_logs_are_counted(self):
        scored = score.score(self.out)
        self.assertEqual((scored["refusals"], scored["refusals_from"]), (3, "logs"))

    def test_runs_counted_differently_are_flagged_when_compared(self):
        (self.out / "events.meta.jsonl").write_text(json.dumps(delegation(True)) + "\n")
        base = Path(self.tmp.name) / "base.json"
        base.write_text(json.dumps({"totals": {"refusals": 9}}))
        shown = io.StringIO()
        with contextlib.redirect_stdout(shown):
            score.main([self.tmp.name, "--compare", str(base)])
        self.assertIn("refusals counted from logs then, events now: not comparable", shown.getvalue())
        self.assertIn("refusals: 9 -> 1", shown.getvalue())


if __name__ == "__main__":
    unittest.main()
