#!/usr/bin/env python3
"""tools/check_coverage.py on small lcov reports: the per-crate and per-file
floors with their tolerance, the unit-test floors, the uncovered ratchet, and
--seed/--update."""

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
import check_coverage

ROOT = "/repo"


def lcov(**files):
    """An lcov report; each file maps to a list of hit counts, one per line."""
    out = []
    for path, counts in files.items():
        out.append(f"SF:{ROOT}/{path}")
        out += [f"DA:{n},{c}" for n, c in enumerate(counts, 1)]
        out.append("end_of_record")
    return "\n".join(out) + "\n"


A = "crates/a/src/lib.rs"
B = "crates/a/src/b.rs"
C = "crates/c/src/lib.rs"


def floor(**kw):
    base = {"tolerance": 1.0, "crates": {}, "files": {}, "unit_files": {}, "uncovered": []}
    base.update(kw)
    return base


class Parse(unittest.TestCase):
    def test_only_crate_src_files_count_and_paths_are_relative(self):
        text = lcov(
            **{
                A: [1, 0],
                "crates/a/tests/it.rs": [1],
                "crates/a/build.rs": [1],
                "vendor/x/src/lib.rs": [1],
            }
        )
        self.assertEqual(check_coverage.parse_lcov(text, ROOT), {A: [2, 1]})

    def test_a_line_seen_in_several_instantiations_is_hit_if_any_hit(self):
        text = f"SF:{ROOT}/{A}\nDA:1,0\nDA:1,3\nDA:2,0\nend_of_record\n"
        self.assertEqual(check_coverage.parse_lcov(text, ROOT), {A: [2, 1]})

    def test_crate_percentages_sum_a_crates_files(self):
        files = {A: [2, 1], B: [2, 2], C: [4, 0]}
        pct = check_coverage.crate_percentages(files)
        self.assertEqual(pct, {"a": 75.0, "c": 0.0})

    def test_floor1_rounds_down_so_a_baseline_always_passes(self):
        self.assertEqual(check_coverage.floor1(71.349), 71.3)
        self.assertEqual(check_coverage.floor1(100.0), 100.0)
        self.assertEqual(check_coverage.floor1(0.0), 0.0)


class Check(unittest.TestCase):
    def test_the_baseline_passes(self):
        files = {A: [10, 8], C: [4, 4]}
        f = floor(crates={"a": 80.0, "c": 100.0}, files={A: 80.0})
        self.assertEqual(check_coverage.check(f, files, None), [])

    def test_a_crate_below_its_floor_by_more_than_the_tolerance_fails(self):
        f = floor(crates={"a": 80.0})
        self.assertEqual(check_coverage.check(f, {A: [10, 7]}, None).__len__(), 1)  # 70%
        self.assertEqual(check_coverage.check(f, {A: [100, 79]}, None), [])  # within 1 point
        errors = check_coverage.check(f, {A: [100, 78]}, None)
        self.assertIn("below its floor", errors[0])

    def test_a_crate_missing_from_the_report_or_the_floor_fails(self):
        self.assertIn("no coverage", check_coverage.check(floor(crates={"gone": 50.0}), {}, None)[0])
        self.assertIn("no floor", check_coverage.check(floor(), {A: [1, 1]}, None)[0])

    def test_a_watched_file_below_its_floor_fails(self):
        f = floor(crates={"a": 0.0}, files={B: 90.0})
        self.assertEqual(check_coverage.check(f, {B: [10, 9]}, None), [])
        self.assertIn(B, check_coverage.check(f, {B: [10, 5]}, None)[0])
        self.assertTrue(any(B in e for e in check_coverage.check(f, {}, None)), "a watched file that vanished is 0%")

    def test_unit_floors_need_the_unit_report_and_ignore_the_full_one(self):
        f = floor(crates={"a": 0.0}, unit_files={B: 50.0})
        files = {B: [10, 10]}
        self.assertIn("--unit", check_coverage.check(f, files, None)[0])
        self.assertIn("unit-test", check_coverage.check(f, files, {B: [10, 0]})[0])
        self.assertEqual(check_coverage.check(f, files, {B: [10, 6]}), [])

    def test_a_new_file_with_no_covered_line_fails(self):
        f = floor(crates={"a": 0.0})
        errors = check_coverage.check(f, {A: [4, 4], B: [3, 0]}, None)
        self.assertEqual(len(errors), 1)
        self.assertIn(B, errors[0])

    def test_a_listed_file_that_gained_coverage_must_leave_the_list(self):
        f = floor(crates={"a": 0.0}, uncovered=[B])
        self.assertEqual(check_coverage.check(f, {B: [3, 0]}, None), [])
        errors = check_coverage.check(f, {B: [3, 1]}, None)
        self.assertIn("remove it from `uncovered`", errors[0])

    def test_a_listed_file_that_is_gone_must_leave_the_list(self):
        f = floor(crates={"a": 0.0}, uncovered=[B])
        errors = check_coverage.check(f, {A: [1, 1]}, None)
        self.assertIn("deleted or renamed", errors[0])

    def test_a_file_with_no_instrumented_lines_is_not_uncovered(self):
        f = floor(crates={"a": 100.0})
        self.assertEqual(check_coverage.check(f, {A: [1, 1], B: [0, 0]}, None), [])


class Update(unittest.TestCase):
    def test_seed_fills_every_section_from_the_reports(self):
        files = {A: [10, 5], B: [4, 0], C: [2, 2]}
        seeded = check_coverage.seed(files, {A: [10, 3]}, [A], [A])
        self.assertEqual(seeded["crates"], {"a": 35.7, "c": 100.0})
        self.assertEqual(seeded["files"], {A: 50.0})
        self.assertEqual(seeded["unit_files"], {A: 30.0})
        self.assertEqual(seeded["uncovered"], [B])
        self.assertEqual(check_coverage.check(seeded, files, {A: [10, 3]}), [])

    def test_update_only_raises_floors_and_shrinks_the_list(self):
        f = floor(crates={"a": 40.0}, files={A: 90.0}, uncovered=[B, "crates/a/src/old.rs"])
        files = {A: [10, 5], B: [3, 3]}
        updated = check_coverage.update(f, files, None)
        self.assertEqual(updated["crates"]["a"], 61.5)
        self.assertEqual(updated["files"][A], 90.0, "a floor never goes down")
        self.assertEqual(updated["uncovered"], [], "covered and vanished files leave the list")

    def test_update_never_adds_an_uncovered_file(self):
        f = floor(crates={"a": 0.0})
        updated = check_coverage.update(f, {A: [3, 0]}, None)
        self.assertEqual(updated["uncovered"], [])


class Cli(unittest.TestCase):
    def run_cli(self, floor_data, report_text, *extra):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            (tmp / "floor.json").write_text(json.dumps(floor_data))
            (tmp / "r.lcov").write_text(report_text)
            argv = ["check_coverage", str(tmp / "r.lcov"), "--root", ROOT, "--floor", str(tmp / "floor.json"), *extra]
            code = check_coverage.main(argv)
            return code, json.loads((tmp / "floor.json").read_text())

    def test_exit_codes(self):
        text = lcov(**{A: [1, 1, 0, 0]})
        code, _ = self.run_cli(floor(crates={"a": 50.0}), text)
        self.assertEqual(code, 0)
        code, _ = self.run_cli(floor(crates={"a": 60.0}), text)
        self.assertEqual(code, 1)

    def test_update_and_seed_refuse_a_local_run(self):
        text = lcov(**{A: [1, 1, 0, 0]})
        for flag in ("--update", "--seed"):
            with mock.patch.dict(os.environ, {"GITHUB_ACTIONS": ""}):
                code, written = self.run_cli(floor(crates={"a": 10.0}), text, flag)
            self.assertEqual(code, 1, flag)
            self.assertEqual(written["crates"], {"a": 10.0}, "the floor file is untouched")

    def test_update_is_allowed_from_the_ci_artifact_or_in_ci(self):
        text = lcov(**{A: [1, 1, 0, 0]})
        with mock.patch.dict(os.environ, {"GITHUB_ACTIONS": ""}):
            code, written = self.run_cli(floor(crates={"a": 10.0}), text, "--update", "--from-ci")
            self.assertEqual((code, written["crates"]["a"]), (0, 50.0))
            code, _ = self.run_cli(floor(crates={"a": 10.0}), text, "--update", "--local")
            self.assertEqual(code, 0)
        with mock.patch.dict(os.environ, {"GITHUB_ACTIONS": "true"}):
            code, _ = self.run_cli(floor(crates={"a": 10.0}), text, "--update")
            self.assertEqual(code, 0)

    def test_the_committed_floor_file_is_well_formed(self):
        data = json.loads(check_coverage.FLOOR.read_text())
        self.assertEqual(set(data), {"tolerance", "crates", "files", "unit_files", "uncovered"})
        self.assertTrue(all(isinstance(v, (int, float)) for v in data["crates"].values()))
        self.assertEqual(data["uncovered"], sorted(data["uncovered"]))
        for path in [*data["files"], *data["unit_files"], *data["uncovered"]]:
            self.assertTrue((check_coverage.ROOT / path).is_file(), path)
        overlap = set(data["uncovered"]) & set(data["files"])
        self.assertFalse(overlap, "a covered file has a floor; an uncovered one is on the list")


if __name__ == "__main__":
    unittest.main()
