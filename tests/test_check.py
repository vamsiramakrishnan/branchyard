#!/usr/bin/env python3
"""tools/check.py: step selection, --fast and --fix, the verdict and exit code, and the
step table against .github/workflows/check.yml (and what that workflow promises: one run per
change, no job that repeats another, a cache on every job that compiles, actions pinned to a
commit, and the cargo gates in parallel jobs that test every crate once, still run every step
the single `controls` job ran, and leave `controls` green only when all of them pass). A fake
step table runs the runner; no cargo."""

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
NIGHTLY = (ROOT / ".github/workflows/nightly.yml").read_text()


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
        known = {"fast", "docs", "controls", "lint", "schemas", "test-engine", "walkthrough"}
        self.assertTrue({step.job for step in check.STEPS} <= known)
        self.assertTrue(
            {step.job for step in check.STEPS} <= set(controls_jobs()), "a step names a job check.yml lacks"
        )

    def test_each_cargo_step_is_labelled_with_the_job_that_runs_it(self):
        # Not the schema steps: each runs one test file here, where CI runs the crate's whole suite
        # with the feature (and branchyard-setup's schema test is part of test-engine's run).
        for step in check.STEPS:
            if not step.cargo or step.fix or step.name.startswith("schema_"):
                continue
            command = " ".join(step.argv[1:] if step.argv[0] == check.PY else step.argv)
            self.assertIn(command, job_text(step.job), f"{step.name} is labelled {step.job}")

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

    def test_the_python_gates_run_once_in_the_fast_job(self):
        runs = [
            name
            for name, lines in controls_jobs().items()
            for line in lines
            if line.lstrip().startswith("- run:") and "tools/check.py" in line
        ]
        self.assertEqual(runs, ["fast"])
        self.assertIn("python3 tools/check.py --fast --keep-going", job_text("fast"))
        self.assertIn("cargo fmt --all -- --check", job_text("fast"))

    def test_every_python_script_check_yml_runs_is_a_step(self):
        scripts = set()
        # The jobs whose gates tools/check.py also runs; coverage, deny and unused-deps run
        # tools only CI uses (check_coverage.py, install_ci_tool.py).
        for job in controls_jobs():
            if job in ("coverage", "deny", "unused-deps"):
                continue
            scripts |= set(re.findall(r"\bpython3?\s+((?:tools|tests)/[\w./-]+\.py)", job_text(job)))
        steps = {arg for step in check.STEPS for arg in step.argv if isinstance(arg, str)}
        self.assertEqual(sorted(scripts - steps - {"tools/check.py"}), [])


class Workflow(unittest.TestCase):
    """What .github/workflows/check.yml promises beyond running the steps."""

    def test_a_change_gets_one_run_not_two(self):
        header = WORKFLOW.split("\njobs:", 1)[0]
        self.assertNotIn("[push, pull_request]", header)
        self.assertRegex(header, r"(?m)^on:\n  push:\n    branches: \[main\]\n  pull_request:\n")

    def test_the_python_job_is_gone_and_the_fast_job_is_first(self):
        jobs = list(controls_jobs())
        self.assertNotIn("python", jobs)
        self.assertEqual(jobs[0], "fast")

    def test_no_job_reruns_a_filtered_part_of_the_workspace_tests(self):
        self.assertEqual(len(re.findall(r"- run: cargo test --workspace", WORKFLOW)), 1)
        self.assertIn("- run: cargo test --workspace", job_text("test-engine"))
        self.assertNotIn("branchyard-controls --locked --offline catalog", WORKFLOW)
        self.assertNotIn("--test compatibility", WORKFLOW)

    def test_every_crate_of_the_workspace_is_tested_by_exactly_one_job(self):
        # test-engine takes the workspace less what it excludes; the excluded crates are exactly the
        # ones test-cli and test-server name, and no crate is in both.
        excluded = set(re.findall(r"--exclude ([\w-]+)", job_text("test-engine")))
        named = {job: set(re.findall(r"-p ([\w-]+)", test_command(job))) for job in ("test-cli", "test-server")}
        self.assertEqual(named["test-cli"] & named["test-server"], set())
        self.assertEqual(excluded, named["test-cli"] | named["test-server"])
        self.assertTrue(excluded < workspace_members(), excluded - workspace_members())

    def test_every_step_the_single_cargo_job_ran_still_runs(self):
        # What `controls` ran until October 2026, one step at a time, besides `cargo test --workspace`
        # (the test above holds that one).
        for command in (
            'test -z "${BRANCHYARD_BLESS+x}" && test -z "${BY_UPDATE_SCHEMA+x}"',
            "cargo build --locked --offline -p branchyard-cli -p branchyard-runtime --bin by --bin fake-acp-agent",
            "REQUIRE_BINARIES=1 python3 tests/test_getting_started.py",
            "python tools/check_missing_docs.py",
            "cargo clippy --workspace --all-targets --locked --offline -- -D warnings",
            "cargo test -p branchyard-client --locked --offline --features schema",
            "cargo clippy -p branchyard-client --all-targets --locked --offline --features schema -- -D warnings",
            "cargo test -p branchyard-server --locked --offline --features schema",
            "cargo clippy -p branchyard-server --all-targets --locked --offline --features schema -- -D warnings",
            "cargo test -p branchyard-cli --locked --offline --features schema rig",
            "cargo clippy -p branchyard-cli --all-targets --locked --offline --features schema -- -D warnings",
        ):
            self.assertIn(command, WORKFLOW)

    def test_controls_is_green_only_when_every_cargo_gate_is(self):
        text = job_text("controls")
        # Every job split from it, which is every job but the ones that ran beside it before.
        beside = {
            "fast",
            "controls",
            "postgres",
            "microsandbox",
            "docs",
            "all-features",
            "unused-deps",
            "deny",
            "coverage",
        }
        needs = re.search(r"(?m)^    needs: \[([^\]]*)\]$", text)
        assert needs is not None, text
        self.assertEqual({name.strip() for name in needs.group(1).split(",")}, set(controls_jobs()) - beside)
        # Run after a needed job failed (skipped, it would count as passed), and fail unless all passed.
        self.assertIn("if: ${{ !cancelled() }}", text)
        self.assertIn("RESULTS: ${{ join(needs.*.result, ' ') }}", text)
        self.assertIn('test "$result" = success || exit 1', text)

    def test_every_job_has_a_timeout(self):
        for name in controls_jobs():
            self.assertIn("timeout-minutes:", job_text(name), name)

    def test_every_job_that_compiles_restores_a_cache(self):
        compiling = re.compile(r"cargo (?:test|clippy|doc|check|build|llvm-cov)\b")
        for name in controls_jobs():
            text = job_text(name)
            if compiling.search(text):
                self.assertIn("Swatinem/rust-cache@", text, f"the {name} job compiles without a cache")

    def test_the_cache_is_written_only_on_main(self):
        for name in controls_jobs():
            text = job_text(name)
            if "Swatinem/rust-cache@" in text:
                self.assertIn("save-if: ${{ github.ref == 'refs/heads/main' }}", text, name)

    def test_every_action_is_pinned_to_a_full_commit(self):
        for text in (WORKFLOW, NIGHTLY):
            uses = re.findall(r"(?m)^\s*(?:- )?uses:\s*(\S+)", text)
            self.assertTrue(uses)
            for use in uses:
                self.assertRegex(use, r"^[\w./-]+@[0-9a-f]{40}$", use)

    def test_the_nightly_workflow_runs_on_a_schedule_and_on_request(self):
        header = NIGHTLY.split("\njobs:", 1)[0]
        self.assertIn("schedule:", header)
        self.assertIn("workflow_dispatch:", header)
        self.assertNotIn("pull_request", header)
        self.assertNotIn("push:", header)
        self.assertIn("permissions:\n  contents: read", header)

    def test_the_nightly_workflow_skips_what_a_stock_runner_cannot_run(self):
        # Each skip is explained where it is made; none of the KVM, cluster and Anvil tests runs.
        # Not the doctests: `--ignored` would compile the examples marked `ignore`.
        self.assertIn("--tests -- --ignored", NIGHTLY)
        self.assertIn("--skip cluster_", NIGHTLY)
        self.assertIn("--skip create_bridge_template", NIGHTLY)
        self.assertNotIn("--features microsandbox", NIGHTLY)


def job_text(name):
    """The text of one job in check.yml."""
    return "\n".join(controls_jobs()[name])


def test_command(job):
    """The one `cargo test` line of a job in check.yml."""
    found = [line for line in controls_jobs()[job] if line.lstrip().startswith("- run: cargo test")]
    assert len(found) == 1, (job, found)
    return found[0]


def workspace_members():
    """The package names of the workspace's members (each member's directory is its name)."""
    manifest = (ROOT / "Cargo.toml").read_text()
    members = re.search(r"(?ms)^members = \[(.*?)^\]", manifest)
    assert members is not None
    return {path.rsplit("/", 1)[-1] for path in re.findall(r'"([^"]+)"', members.group(1))}


def controls_jobs():
    """{job name: its lines} for the jobs of check.yml, in file order."""
    jobs, current = {}, None
    lines = WORKFLOW.splitlines()
    for line in lines[lines.index("jobs:") + 1 :]:
        match = re.match(r"^  (\w[\w-]*):\s*$", line)
        if match:
            current = jobs.setdefault(match.group(1), [])
        elif current is not None:
            current.append(line)
    return jobs


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
