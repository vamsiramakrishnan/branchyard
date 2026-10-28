#!/usr/bin/env python3
"""The Python examples (examples/python/) against a built `by` and the fake ACP
agent, in a temporary git repository: no model, no network, no real harness.

`first_run.py` makes a change and reports it, and reports a harness that fails as
a failure (exit 1, the branch named, a fix); `stream_events.py` prints the
branch's activity until it settles; `delegate.py` spawns a child as a named
branch of a server of its own and integrates it, and says so when it has no
branch to act as.

The binaries come from BRANCHYARD_BY and BRANCHYARD_FAKE_AGENT, else
target/{debug,release}/ under the checkout (or CARGO_TARGET_DIR), else `by` on
PATH with `fake-acp-agent` beside it. With neither, the tests skip: the python
job has no cargo build. `crates/branchyard-cli/tests/sdk_examples.rs` runs this
file with the binaries it was built with, so `cargo test` runs it for real.

Run: `python3 tests/test_sdk_examples.py`.
"""

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
SDK = ROOT / "sdk" / "python"
EXAMPLES = ROOT / "examples" / "python"
sys.path.insert(0, str(SDK))

import branchyard_client as bc  # noqa: E402

AGENT_NAME = "fake-acp-agent"
GIT = {"GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}


def target_dirs() -> list[Path]:
    base = Path(os.environ["CARGO_TARGET_DIR"]) if os.environ.get("CARGO_TARGET_DIR") else ROOT / "target"
    return [base / "debug", base / "release"]


def executable(path: str | None) -> str | None:
    return path if path and os.path.isfile(path) and os.access(path, os.X_OK) else None


def find_by() -> str | None:
    """The `by` to test: BRANCHYARD_BY, else the checkout's build, else PATH."""
    found = executable(os.environ.get("BRANCHYARD_BY"))
    for directory in target_dirs():
        found = found or executable(str(directory / "by"))
    return found or executable(shutil.which("by"))


def find_agent(by: str | None) -> str | None:
    """The fake ACP agent: BRANCHYARD_FAKE_AGENT, else beside `by`, else the checkout's build."""
    found = executable(os.environ.get("BRANCHYARD_FAKE_AGENT"))
    if by:
        found = found or executable(str(Path(by).resolve().parent / AGENT_NAME))
    for directory in target_dirs():
        found = found or executable(str(directory / AGENT_NAME))
    return found


BY = find_by()
AGENT = find_agent(BY)
SKIP = (
    None
    if BY and AGENT
    else "`by` and fake-acp-agent are not built (cargo build -p branchyard-cli -p branchyard-runtime)"
)


def git(repo: Path, *args: str) -> str:
    done = subprocess.run(
        ["git", *args], cwd=repo, env={**os.environ, **GIT}, capture_output=True, text=True, stdin=subprocess.DEVNULL
    )
    if done.returncode != 0:
        raise AssertionError(f"git {args}: {done.stderr}")
    return done.stdout


@unittest.skipIf(SKIP, SKIP or "")
class Examples(unittest.TestCase):
    def setUp(self) -> None:
        assert BY and AGENT
        # A login shell some machine's profile starts for a harness may still be writing into HOME
        # (credential stubs, caches) as the directory goes: that is not this test's failure.
        self.tmp = tempfile.TemporaryDirectory(ignore_cleanup_errors=True)
        self.addCleanup(self.tmp.cleanup)
        self.home = Path(self.tmp.name) / "home"
        self.home.mkdir()
        self.repo = Path(self.tmp.name) / "app"
        self.repo.mkdir()
        git(self.repo, "init", "-q", "-b", "main")
        git(self.repo, "config", "user.name", "Test")
        git(self.repo, "config", "user.email", "test@localhost")
        git(self.repo, "config", "commit.gpgsign", "false")
        (self.repo / "parser.py").write_text("def parse(text):\n    return text\n")
        git(self.repo, "add", ".")
        git(self.repo, "commit", "-q", "-m", "initial")
        self.env = self.environment()

    def environment(self, **extra: str) -> dict[str, str]:
        """A process environment no developer setting reaches: no BRANCHYARD_* but ours,
        no user git configuration, no user Branchyard configuration."""
        env = {k: v for k, v in os.environ.items() if not k.startswith("BRANCHYARD_")}
        env.update(
            GIT,
            HOME=str(self.home),
            NO_COLOR="1",
            PYTHONDONTWRITEBYTECODE="1",
            PYTHONPATH=str(SDK),
            GIT_CEILING_DIRECTORIES=str(Path(tempfile.gettempdir()).resolve()),
            BRANCHYARD_BY=str(BY),
            BRANCHYARD_USER_CONFIG="/nonexistent/branchyard-config.toml",
        )
        env.update(extra)
        return env

    def run_example(self, script: str, *args: str, env: dict[str, str] | None = None, cwd: Path | None = None):
        return subprocess.run(
            [sys.executable, str(EXAMPLES / script), *args],
            cwd=cwd or self.repo,
            env=env or self.env,
            capture_output=True,
            text=True,
            stdin=subprocess.DEVNULL,
            timeout=300,
        )

    def stand_in(self) -> list[str]:
        """The arguments that run the harness as the fake agent."""
        return ["--repo", str(self.repo), "--command", str(AGENT)]

    # -- first_run.py ------------------------------------------------------

    def test_first_run_makes_a_change_and_reports_its_status_and_diff(self):
        done = self.run_example("first_run.py", "WRITE hello.txt=hi", "--name", "hello", *self.stand_in())
        self.assertEqual(done.returncode, 0, done.stderr)
        lines = done.stdout.splitlines()
        self.assertEqual(lines[0], "hello: ready")
        self.assertRegex(lines[1], r"^  1 file\(s\) changed, \+1 -0 at [0-9a-f]{10}$")
        self.assertEqual(lines[2], "  hello.txt")
        self.assertEqual(lines[3], "next: by diff hello    # then: by merge hello, or by rm hello")
        # The branch is the repository's, not the script's: it outlives the server it ran on.
        self.assertEqual(git(self.repo, "show", "by/hello:hello.txt"), "hi\n")

    def test_a_relative_command_is_resolved_where_it_was_typed_not_in_the_repository(self):
        work = Path(self.tmp.name) / "work"
        work.mkdir()
        (work / "agent").symlink_to(AGENT)
        common = ["--repo", str(self.repo), "--command", "./agent"]
        done = self.run_example("first_run.py", "WRITE rel.txt=1", "--name", "rel", *common, cwd=work)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout.splitlines()[0], "rel: ready")
        streamed = self.run_example("stream_events.py", "WRITE rel2.txt=1", "--name", "rel2", *common, cwd=work)
        self.assertEqual(streamed.returncode, 0, streamed.stderr)
        self.assertEqual(streamed.stdout.splitlines()[-1], "rel2: ready")

    def test_first_run_says_when_nothing_changed(self):
        done = self.run_example("first_run.py", "just say hello", "--name", "talk", *self.stand_in())
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout.splitlines()[:2], ["talk: no_changes", "  no changes"])
        self.assertIn("next: by log talk", done.stdout)

    def test_first_run_fails_when_the_branch_it_made_failed_though_the_operation_succeeded(self):
        # The fake agent exits mid-turn: the request is admitted and runs, the turn fails.
        done = self.run_example("first_run.py", "EXIT now", "--name", "boom", *self.stand_in())
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertEqual(done.stdout.splitlines(), ["boom: failed"])
        self.assertIn("first_run: branch boom ended failed:", done.stderr)
        self.assertIn("to fix: ", done.stderr)
        self.assertIn("by log boom", done.stderr)

    def test_first_run_reports_a_name_that_is_taken_and_a_missing_by(self):
        again = ["WRITE a.txt=1", "--name", "twice", *self.stand_in()]
        self.assertEqual(self.run_example("first_run.py", *again).returncode, 0)
        taken = self.run_example("first_run.py", *again)
        self.assertEqual(taken.returncode, 1)
        self.assertIn("first_run: branch_exists", taken.stderr)
        self.assertNotIn("Traceback", taken.stderr)
        # No `by` anywhere: the error names the way to get one, with no traceback.
        nowhere = Path(self.tmp.name) / "empty-path"
        nowhere.mkdir()
        env = self.environment(PATH=str(nowhere))
        env.pop("BRANCHYARD_BY")
        missing = subprocess.run(
            [sys.executable, str(EXAMPLES / "first_run.py"), "x", "--repo", str(self.repo)],
            cwd=self.repo,
            env=env,
            capture_output=True,
            text=True,
            stdin=subprocess.DEVNULL,
            timeout=120,
        )
        self.assertEqual(missing.returncode, 1, missing.stdout)
        self.assertIn("`by` is not on PATH (or set BRANCHYARD_BY)", missing.stderr)
        self.assertIn("to fix: install released binaries with `curl -fsSL", missing.stderr)
        self.assertIn("releases/latest/download/install.sh | sh -s -- --no-setup", missing.stderr)
        self.assertIn(
            "build from a checkout with `cargo install --locked --path crates/branchyard-cli`", missing.stderr
        )
        self.assertNotIn("no release is published yet", missing.stderr)
        self.assertNotIn("Traceback", missing.stderr)

    # -- stream_events.py --------------------------------------------------

    def test_stream_events_prints_the_activity_until_the_branch_is_ready(self):
        done = self.run_example("stream_events.py", "WRITE hello.txt=hi", "--name", "live", *self.stand_in())
        self.assertEqual(done.returncode, 0, done.stderr)
        lines = [line.split(None, 2) for line in done.stdout.splitlines()]
        said = [line[2] for line in lines[:-1]]
        self.assertEqual(lines[-1], ["live:", "ready"])
        self.assertTrue(all(line[1] == "live" for line in lines[:-1]), lines)
        for expected in (
            "status: running",
            "prompt: WRITE hello.txt=hi",
            "says: wrote hello.txt",
            "harness: turn_ended",
            "candidate: 1 file(s), +1 -0",
            "status: ready",
        ):
            self.assertIn(expected, said)
        self.assertLess(said.index("status: running"), said.index("says: wrote hello.txt"))
        self.assertLess(said.index("says: wrote hello.txt"), said.index("status: ready"))
        # The feed's positions only rise.
        positions = [int(line[0]) for line in lines[:-1]]
        self.assertEqual(positions, sorted(positions))

    def test_stream_events_stops_at_a_failed_branch_and_exits_1(self):
        done = self.run_example("stream_events.py", "EXIT now", "--name", "dead", *self.stand_in())
        self.assertEqual(done.returncode, 1, done.stderr)
        lines = done.stdout.splitlines()
        self.assertTrue(any("status: failed (" in line for line in lines), lines)
        self.assertEqual(lines[-1], "dead: failed")

    # -- delegate.py -------------------------------------------------------

    def test_delegate_without_a_branch_to_act_as_says_so(self):
        done = self.run_example("delegate.py")
        self.assertEqual(done.returncode, 2)
        self.assertEqual(done.stdout, "")
        self.assertIn("nothing to act as", done.stderr)
        self.assertIn("BRANCHYARD_REMOTE", done.stderr)

    def test_delegate_spawns_waits_and_integrates_as_a_branch_of_a_server(self):
        serve = ["--allow-delegation", "--harness-command", f"gemini-cli={AGENT}"]
        patched = mock.patch.dict(os.environ, self.environment(), clear=True)
        with patched, bc.Client.local(str(self.repo), name="app", by=str(BY), args=serve) as client:
            app = client.repo("app")
            lead = app.task(
                "WRITE lead.txt=1",
                name="lead",
                harness="gemini-cli",
                budget_usd=1.0,
                allow_delegation=True,
                delegation={"max_depth": 1, "max_children": 2, "harnesses": []},
            )
            client.wait(lead, timeout=120, check=True)
            env = self.environment(
                BRANCHYARD_REMOTE=client.url,
                BRANCHYARD_TOKEN_FILE=str(Path(client.server_dir or "") / "token"),
                BRANCHYARD_REPO="app",
                BRANCHYARD_BRANCH="lead",
            )
            done = self.run_example(
                "delegate.py",
                "WRITE notes.txt=hello",
                "--name",
                "notes",
                "--harness",
                "gemini-cli",
                "--budget-usd",
                "0.5",
                env=env,
            )
            self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
            lines = done.stdout.splitlines()
            self.assertEqual(lines[0], "notes: ready")
            self.assertRegex(lines[1], r"^merged notes into by/lead at [0-9a-f]{10}$")
            inspected = app.inspect("lead")
            self.assertEqual(inspected.children, ["notes"])
            self.assertEqual(app.branch("notes").state, "merged")
            # The child's file landed on the lead's branch.
            self.assertEqual(git(self.repo, "show", "by/lead:notes.txt"), "hello\n")

    def test_the_examples_are_python_3_9_source_with_a_module_docstring(self):
        import ast

        for path in sorted(EXAMPLES.glob("*.py")):
            source = path.read_text()
            tree = ast.parse(source, feature_version=(3, 9))
            self.assertTrue(ast.get_docstring(tree), f"{path.name} has no docstring")
            self.assertTrue(source.startswith("#!/usr/bin/env python3\n"), path.name)
        self.assertEqual({p.name for p in EXAMPLES.glob("*.py")}, {"first_run.py", "stream_events.py", "delegate.py"})


class Discovery(unittest.TestCase):
    """How the binaries are found: the skip must be a skip, not a failure."""

    def test_nothing_built_is_none_not_an_error(self):
        with mock.patch.dict(os.environ, {"CARGO_TARGET_DIR": "/nonexistent/target", "PATH": "/nonexistent"}):
            os.environ.pop("BRANCHYARD_BY", None)
            os.environ.pop("BRANCHYARD_FAKE_AGENT", None)
            self.assertIsNone(find_by())
            self.assertIsNone(find_agent(None))

    def test_the_environment_wins_and_the_agent_is_found_beside_by(self):
        with tempfile.TemporaryDirectory() as tmp:
            by = Path(tmp) / "by"
            agent = Path(tmp) / AGENT_NAME
            for path in (by, agent):
                path.write_text("#!/bin/sh\n")
                path.chmod(0o755)
            with mock.patch.dict(os.environ, {"BRANCHYARD_BY": str(by)}):
                os.environ.pop("BRANCHYARD_FAKE_AGENT", None)
                self.assertEqual(find_by(), str(by))
                self.assertEqual(find_agent(find_by()), str(agent))
            not_executable = Path(tmp) / "plain"
            not_executable.write_text("x")
            with mock.patch.dict(os.environ, {"BRANCHYARD_BY": str(not_executable)}):
                self.assertNotEqual(find_by(), str(not_executable))


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
