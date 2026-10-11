#!/usr/bin/env python3
"""tools/check.py: step selection, --fast and --fix, the verdict and exit code, and the
step table against .github/workflows/check.yml. A fake step table runs the runner; no cargo."""

import ast
import contextlib
import importlib.util
import io
import os
import re
import subprocess
import sys
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("check", ROOT / "tools" / "check.py")
assert spec is not None and spec.loader is not None
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)

WORKFLOW = (ROOT / ".github/workflows/check.yml").read_text()


def fake(name, code=0, *, job="controls", cargo=False, fix=False, lines=("ok",)):
    """A step that prints `lines` and exits with `code`."""
    script = "import sys; " + "".join(f"print({line!r}); " for line in lines) + f"sys.exit({code})"
    return check.Step(name, job, (sys.executable, "-c", script), cargo=cargo, fix=fix)


def run(steps, *argv):
    """main() over a fake table: (exit code, output lines)."""
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        code = check.main(list(argv), steps=steps)
    return code, out.getvalue().splitlines()


class Verdict(unittest.TestCase):
    def test_passing_steps_print_ok_and_pass(self):
        code, lines = run((fake("a"), fake("b")))
        self.assertEqual(code, 0)
        self.assertEqual(lines, ["ok      a", "ok      b", "check: PASSED"])

    def test_a_failing_step_prints_its_last_lines_and_fails(self):
        output = [f"line {number}" for number in range(30)]
        code, lines = run((fake("a", code=1, lines=output),))
        self.assertEqual(code, 1)
        self.assertEqual(lines[0], "FAILED  a")
        self.assertIn("    line 29", lines)
        self.assertIn("    line 10", lines)
        self.assertNotIn("    line 9", lines)
        self.assertEqual(lines[-1], "check: FAILED (1 step)")

    def test_without_keep_going_the_run_stops_at_the_first_failure(self):
        code, lines = run((fake("a", code=1), fake("b")))
        self.assertEqual(code, 1)
        self.assertFalse(any(line.endswith(" b") for line in lines), lines)
        self.assertEqual(lines[-1], "check: FAILED (1 step)")

    def test_keep_going_runs_every_step_and_counts_each_failure(self):
        code, lines = run((fake("a", code=1), fake("b", code=1), fake("c")), "--keep-going")
        self.assertEqual(code, 1)
        self.assertIn("ok      c", lines)
        self.assertEqual(lines[-1], "check: FAILED (2 steps)")

    def test_a_command_that_cannot_start_is_a_failed_step(self):
        missing = check.Step("missing", "controls", ("no-such-command-for-check-py",))
        code, lines = run((missing,))
        self.assertEqual(code, 1)
        self.assertEqual(lines[0], "FAILED  missing")
        self.assertEqual(lines[-1], "check: FAILED (1 step)")


class Selection(unittest.TestCase):
    def test_fast_skips_cargo_steps_and_says_so(self):
        code, lines = run((fake("python_gate"), fake("cargo_gate", code=1, cargo=True)), "--fast")
        self.assertEqual(code, 0)
        self.assertEqual(lines, ["ok      python_gate", "skip    cargo_gate", "check: PASSED"])

    def test_only_runs_the_named_steps_in_table_order(self):
        code, lines = run((fake("a"), fake("b"), fake("c")), "--only", "c,a")
        self.assertEqual(code, 0)
        self.assertEqual(lines, ["ok      a", "ok      c", "check: PASSED"])

    def test_an_unknown_name_is_a_usage_error(self):
        with contextlib.redirect_stderr(io.StringIO()) as err, self.assertRaises(SystemExit) as raised:
            run((fake("a"),), "--only", "nope")
        self.assertEqual(raised.exception.code, 2)
        self.assertIn("unknown step: nope", err.getvalue())

    def test_only_refuses_a_cargo_step_under_fast(self):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as raised:
            run((fake("cargo_gate", cargo=True),), "--fast", "--only", "cargo_gate")
        self.assertEqual(raised.exception.code, 2)

    def test_a_rewriting_step_runs_only_with_fix(self):
        steps = (fake("fix_it", fix=True), fake("check_it"))
        self.assertEqual(run(steps)[1], ["ok      check_it", "check: PASSED"])
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            run(steps, "--only", "fix_it")


class Fix(unittest.TestCase):
    def test_rewrites_run_before_the_checks(self):
        steps = (fake("check_one"), fake("fix_one", fix=True), fake("check_two"))
        _, lines = run(steps, "--fix")
        self.assertEqual(lines, ["ok      fix_one", "ok      check_one", "ok      check_two", "check: PASSED"])

    def test_a_failed_rewrite_stops_the_checks(self):
        steps = (fake("fix_one", code=1, fix=True), fake("check_one"))
        code, lines = run(steps, "--fix")
        self.assertEqual(code, 1)
        self.assertFalse(any("check_one" in line for line in lines), lines)

    def test_fix_with_fast_skips_the_cargo_rewrites(self):
        steps = (fake("fix_cargo", code=1, cargo=True, fix=True), fake("check_one"))
        code, lines = run(steps, "--fix", "--fast")
        self.assertEqual(code, 0)
        self.assertEqual(lines, ["skip    fix_cargo", "ok      check_one", "check: PASSED"])


class Environment(unittest.TestCase):
    PROBE = "import os, sys\nsys.exit(1 if {'BY_UPDATE_SCHEMA', 'BRANCHYARD_BLESS'} & set(os.environ) else 0)\n"

    def test_a_check_never_inherits_a_rewrite_switch(self):
        probe = check.Step("probe", "controls", (sys.executable, "-c", self.PROBE))
        with mock.patch.dict(os.environ, {"BY_UPDATE_SCHEMA": "1", "BRANCHYARD_BLESS": "1"}):
            self.assertEqual(run((probe,))[0], 0)

    def test_a_step_sets_its_own_variable(self):
        script = "import os, sys\nsys.exit(0 if os.environ.get('BY_UPDATE_SCHEMA') == '1' else 1)\n"
        probe = check.Step("probe", "controls", (sys.executable, "-c", script), env=(("BY_UPDATE_SCHEMA", "1"),))
        with mock.patch.dict(os.environ, {"BY_UPDATE_SCHEMA": "0"}):
            self.assertEqual(run((probe,))[0], 0)


class Interpreter(unittest.TestCase):
    def test_it_runs_under_isolated_mode(self):
        done = subprocess.run(
            [sys.executable, "-I", str(ROOT / "tools/check.py"), "--help"], capture_output=True, text=True
        )
        self.assertEqual(done.returncode, 0, done.stderr)

    def test_it_parses_as_python_39(self):
        source = (ROOT / "tools/check.py").read_text()
        ast.parse(source, feature_version=(3, 9))


class RealTable(unittest.TestCase):
    def test_names_are_unique_and_every_job_is_known(self):
        names = [step.name for step in check.STEPS]
        self.assertEqual(len(names), len(set(names)))
        self.assertTrue({step.job for step in check.STEPS} <= {"python", "docs", "controls"})

    def test_rewriting_steps_are_the_fix_steps(self):
        for step in check.STEPS:
            self.assertEqual(step.fix, step.name.startswith("fix_"), step.name)

    def test_every_command_names_a_file_that_exists(self):
        for step in check.STEPS:
            for arg in step.argv:
                if isinstance(arg, str) and arg.endswith(".py"):
                    self.assertTrue((ROOT / arg).is_file(), f"{step.name}: {arg}")

    def test_the_cargo_gates_are_the_workflow_commands(self):
        for name in ("cargo_doc", "cargo_fmt", "cargo_clippy"):
            step = next(step for step in check.STEPS if step.name == name)
            self.assertIn(" ".join(step.argv), WORKFLOW, name)

    def test_check_yml_runs_check_py_for_the_controls_python_gates(self):
        jobs = controls_job_lines()
        self.assertEqual(sum("python3 tools/check.py --fast --keep-going" in line for line in jobs), 1)

    def test_every_python_script_check_yml_runs_is_a_step(self):
        scripts = set()
        for job in ("python", "docs", "controls"):
            scripts |= set(re.findall(r"\bpython3?\s+((?:tools|tests)/[\w./-]+\.py)", job_text(job)))
        steps = {arg for step in check.STEPS for arg in step.argv if isinstance(arg, str)}
        self.assertEqual(sorted(scripts - steps - {"tools/check.py"}), [])


def job_text(name):
    """The text of one job in check.yml."""
    return "\n".join(controls_jobs()[name])


def controls_job_lines():
    return controls_jobs()["controls"]


def controls_jobs():
    jobs, current = {}, None
    for line in WORKFLOW.splitlines():
        match = re.match(r"^  (\w[\w-]*):\s*$", line)
        if match:
            current = jobs.setdefault(match.group(1), [])
        elif current is not None:
            current.append(line)
    return jobs


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
