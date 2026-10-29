#!/usr/bin/env python3
"""Run the gates CI runs, in CI's order, with one command.

    python3 tools/check.py                  every step in STEPS
    python3 tools/check.py --fast           without the cargo steps (the pre-push hook runs this)
    python3 tools/check.py --fix            rewrite first: docs facts, cargo fmt, ruff format, the schemas
    python3 tools/check.py --only A,B       only the named steps
    python3 tools/check.py --keep-going     run past a failing step and report every failure

Each step prints `ok` or `FAILED` (with the last lines of its output); a step that --fast
skips prints `skip`. The verdict is `check: PASSED` or `check: FAILED (n steps)`, and the exit
code is 0 or 1. Standard library only.

CI's fast job runs `--fast --keep-going` (and rustfmt), so each Python gate is defined once,
here, and runs once. The cargo steps stay in the workflow's other jobs, where they are: they
need `cargo fetch` first, and check_missing_docs.py runs `cargo check` itself. A new Python
gate goes in STEPS; tests/test_check.py fails when check.yml runs a script that STEPS does not.
"""

import argparse
import os
import subprocess
import sys
from pathlib import Path
from typing import NamedTuple

ROOT = Path(__file__).resolve().parents[1]
PY = sys.executable
TARGETS = (
    "tools",
    "tests",
    "sdk/python/branchyard_client.py",
    "sdk/python/branchyard_artifacts",
    "crates/branchyard/src/contract/capture/read_file.py",
    "examples/python",
)
TAIL_LINES = 20
# A check only reads. These make the schema generators and the catalog rewrite what they compare.
REWRITE_VARIABLES = ("BY_UPDATE_SCHEMA", "BRANCHYARD_BLESS")


class Step(NamedTuple):
    """One gate. `job` names a job of check.yml: the `fast` job runs every step --fast keeps (the
    Python gates, whichever label they carry; `controls` marks the ones the single cargo job ran
    before they moved to `fast`, and is now the job that is green when every cargo job is), and
    `docs`, `lint`, `schemas`, `test-engine` and `walkthrough` run the cargo ones named for them;
    `cargo` steps are skipped by --fast."""

    name: str
    job: str
    argv: tuple
    cargo: bool = False
    fix: bool = False  # rewrites files: runs only with --fix, before the checks
    env: tuple = ()  # extra (name, value) pairs for this step
    stdout: str = ""  # on success, the command's stdout is written to this file


STEPS = (
    # --fix, in the order it runs.
    Step("fix_artifact_assets", "controls", (PY, "tools/artifact_assets.py", "--write"), fix=True),
    Step("fix_docs_facts", "controls", (PY, "tools/docs_facts.py", "--write"), fix=True),
    Step("fix_cargo_fmt", "fast", ("cargo", "fmt", "--all"), cargo=True, fix=True),
    Step("fix_ruff_format", "fast", (PY, "-m", "ruff", "format", *TARGETS), fix=True),
    Step(
        "fix_cli_reference",
        "test-cli",
        (
            "cargo",
            "run",
            "-q",
            "-p",
            "branchyard-cli",
            "--bin",
            "by",
            "--locked",
            "--offline",
            "--",
            "man",
            "--markdown",
        ),
        cargo=True,
        fix=True,
        stdout="docs/cli-reference.md",
    ),
    Step(
        "fix_contract_schema",
        "schemas",
        (
            "cargo",
            "run",
            "-q",
            "-p",
            "branchyard-client",
            "--features",
            "schema",
            "--example",
            "generate_contract",
            "--locked",
            "--offline",
        ),
        cargo=True,
        fix=True,
        stdout="schema/contract.json",
    ),
    Step(
        "fix_setup_schemas",
        "test-engine",
        ("cargo", "run", "-q", "-p", "branchyard-setup", "--example", "generate_schemas", "--locked", "--offline"),
        cargo=True,
        fix=True,
    ),
    Step(
        "fix_server_config_schema",
        "schemas",
        (
            "cargo",
            "run",
            "-q",
            "-p",
            "branchyard-server",
            "--features",
            "schema",
            "--example",
            "generate_server_config_schema",
            "--locked",
            "--offline",
        ),
        cargo=True,
        fix=True,
        stdout="schema/server.config.json",
    ),
    # schema/rig.json is not written by an example: its test rewrites it when BY_UPDATE_SCHEMA is set.
    Step(
        "fix_rig_schema",
        "schemas",
        (
            "cargo",
            "test",
            "-p",
            "branchyard-cli",
            "--locked",
            "--offline",
            "--features",
            "schema",
            "--bin",
            "by",
            "rig_json",
        ),
        cargo=True,
        fix=True,
        env=(("BY_UPDATE_SCHEMA", "1"),),
    ),
    # Lint, format and type-check tools/, tests/ and the shipped client.
    Step("ruff_check", "fast", (PY, "-m", "ruff", "check", *TARGETS)),
    Step("ruff_format", "fast", (PY, "-m", "ruff", "format", "--check", *TARGETS)),
    Step(
        "mypy",
        "fast",
        (
            PY,
            "-m",
            "mypy",
            "--python-version",
            "3.12",
            "--ignore-missing-imports",
            "--explicit-package-bases",
            *TARGETS,
        ),
    ),
    Step("test_install_ci_tool", "fast", (PY, "tests/test_install_ci_tool.py")),
    Step("test_substrate_gke", "fast", (PY, "tests/test_substrate_gke.py")),
    Step("test_compute_gce", "fast", (PY, "tests/test_compute_gce.py")),
    Step("test_cloud_setup", "fast", (PY, "tests/test_cloud_setup.py")),
    Step("test_fetch_agy", "fast", (PY, "tests/test_fetch_agy.py")),
    Step("test_substrate_image", "fast", (PY, "tests/test_substrate_image.py")),
    Step("test_check_coverage", "fast", (PY, "tests/test_check_coverage.py")),
    Step("test_supply_chain", "fast", (PY, "tests/test_supply_chain.py")),
    # The controls job's Python gates, in its order.
    Step("verify_vendor", "controls", (PY, "tools/verify_vendor.py")),
    Step("test_verify_vendor", "controls", (PY, "tests/test_verify_vendor.py")),
    Step("test_vendor_pin", "controls", (PY, "tests/test_vendor_pin.py")),
    Step("verify_derivatives", "controls", (PY, "tools/verify_derivatives.py")),
    Step("check_catalog", "controls", (PY, "tools/check_catalog.py")),
    # No hand-rolled percent/hex/base64/shell/tar codecs (tools/handrolled_banlist.toml).
    Step("check_handrolled", "controls", (PY, "tools/check_handrolled.py")),
    Step("test_check_handrolled", "controls", (PY, "tests/test_check_handrolled.py")),
    Step("check_docs", "controls", (PY, "tools/check_docs.py")),
    Step("test_check_docs", "controls", (PY, "tests/test_check_docs.py")),
    # The branch-state table in docs/concepts.md against the BranchStatus enum and `by ls`'s words.
    Step("test_concepts_states", "controls", (PY, "tests/test_concepts_states.py")),
    # The new guides' messages and build commands against the code. The walkthrough and the command checks
    # need a build, so they wait for test_getting_started_built below.
    Step("test_getting_started", "controls", (PY, "tests/test_getting_started.py")),
    Step("check_test_hygiene", "controls", (PY, "tools/check_test_hygiene.py")),
    Step("test_check_test_hygiene", "controls", (PY, "tests/test_check_test_hygiene.py")),
    Step("check_silent_failures", "controls", (PY, "tools/check_silent_failures.py")),
    Step("test_check_silent_failures", "controls", (PY, "tests/test_check_silent_failures.py")),
    # The CLI's failures are Reports with a kind and an exit status; the bare messages left only fall.
    Step("check_failure_messages", "fast", (PY, "tools/check_failure_messages.py")),
    Step("test_check_failure_messages", "fast", (PY, "tests/test_check_failure_messages.py")),
    # The lint table's exceptions only fall, and every crate inherits the table.
    Step("check_lint_ratchet", "controls", (PY, "tools/check_lint_ratchet.py")),
    Step("test_check_lint_ratchet", "controls", (PY, "tests/test_check_lint_ratchet.py")),
    # HTTP framing is parsed only in branchyard-wire.
    Step("check_wire", "controls", (PY, "tools/check_wire.py")),
    Step("test_check_wire", "controls", (PY, "tests/test_check_wire.py")),
    Step("artifact_assets", "controls", (PY, "tools/artifact_assets.py")),
    Step("test_artifact_assets", "controls", (PY, "tests/test_artifact_assets.py")),
    Step("test_artifact_bootstrap", "controls", (PY, "tests/test_artifact_bootstrap.py")),
    Step("test_artifacts", "controls", (PY, "tests/test_artifacts.py")),
    Step("test_knowledge_evidence", "controls", (PY, "tests/test_knowledge_evidence.py")),
    Step("test_knowledge_analysis", "controls", (PY, "tests/test_knowledge_analysis.py")),
    Step("test_knowledge_cli", "controls", (PY, "tests/test_knowledge_cli.py")),
    Step("test_artifacts_distribution", "controls", (PY, "tests/test_artifacts_distribution.py")),
    Step("docs_facts", "controls", (PY, "tools/docs_facts.py", "--check")),
    Step("test_docs_facts", "controls", (PY, "tests/test_docs_facts.py")),
    # docs/README.md lists every page under docs/.
    Step("test_docs_index", "controls", (PY, "tests/test_docs_index.py")),
    # Dependency versions are declared once, in [workspace.dependencies].
    Step("check_workspace_deps", "controls", (PY, "tools/check_workspace_deps.py")),
    Step("test_check_workspace_deps", "controls", (PY, "tests/test_check_workspace_deps.py")),
    # The delegation battery's evaluator.
    Step("test_qualify_score", "controls", (PY, "tests/test_qualify_score.py")),
    Step("test_scion", "controls", (PY, "tools/test_scion.py", "--qualified")),
    Step("check_scion_compatibility", "controls", (PY, "tools/check_scion_compatibility.py")),
    Step("test_distribution", "controls", (PY, "tests/test_distribution.py")),
    Step("test_schema_validation", "controls", (PY, "tests/test_schema_validation.py")),
    Step("test_sdk_client", "controls", (PY, "tests/test_sdk_client.py")),
    # examples/python/ against a built `by` and the fake agent. This job has no cargo build when it
    # runs, so the file skips here; `cargo test --workspace` runs it for real
    # (crates/branchyard-cli/tests/sdk_client.rs), and so does a local run once `by` is built.
    Step("test_sdk_examples", "controls", (PY, "tests/test_sdk_examples.py")),
    Step("test_check", "controls", (PY, "tests/test_check.py")),
    Step("test_install_hooks", "controls", (PY, "tests/test_install_hooks.py")),
    # The packaging for Codex, Gemini CLI, Qwen Code, Copilot CLI, Droid and DeepSeek Harness.
    Step("test_plugin_claude_compat", "controls", (PY, "tests/test_plugin_claude_compat.py")),
    # The Pi/Oh My Pi, Amp and OpenCode plugins under node with a stub `by` (skips without node).
    Step("test_plugin_ts", "controls", (PY, "tests/test_plugin_ts.py")),
    Step("test_plugin_config", "controls", (PY, "tests/test_plugin_config.py")),
    # What the skills, slash commands and instructions files tell a model about `by` stays true.
    Step("test_agent_text_drift", "controls", (PY, "tests/test_agent_text_drift.py")),
    # The catalog, the driver profiles, the install manifests, the hook dialects and docs/harness-hooks.md
    # agree on which harnesses exist (docs/adding-a-harness.md).
    Step("test_harness_identity", "controls", (PY, "tests/test_harness_identity.py")),
    # Cargo: rustdoc (the docs job), formatting, the undocumented-items ratchet, Clippy, and the
    # schema freshness tests. The Clippy passes CI runs for the feature builds stay in the workflow.
    Step(
        "cargo_doc",
        "docs",
        ("cargo", "doc", "--workspace", "--no-deps", "--locked", "--offline"),
        cargo=True,
        env=(("RUSTDOCFLAGS", "-D warnings"),),
    ),
    Step("cargo_fmt", "fast", ("cargo", "fmt", "--all", "--", "--check"), cargo=True),
    Step("check_missing_docs", "lint", (PY, "tools/check_missing_docs.py"), cargo=True),
    Step(
        "cargo_clippy",
        "lint",
        ("cargo", "clippy", "--workspace", "--all-targets", "--locked", "--offline", "--", "-D", "warnings"),
        cargo=True,
    ),
    Step(
        "schema_contract",
        "schemas",
        (
            "cargo",
            "test",
            "-p",
            "branchyard-client",
            "--locked",
            "--offline",
            "--features",
            "schema",
            "--test",
            "contract",
        ),
        cargo=True,
    ),
    Step(
        "schema_server_config",
        "schemas",
        (
            "cargo",
            "test",
            "-p",
            "branchyard-server",
            "--locked",
            "--offline",
            "--features",
            "schema",
            "--test",
            "server_config_schema",
        ),
        cargo=True,
    ),
    Step(
        "schema_setup",
        "test-engine",
        ("cargo", "test", "-p", "branchyard-setup", "--locked", "--offline", "--test", "schema"),
        cargo=True,
    ),
    Step(
        "schema_rig",
        "schemas",
        (
            "cargo",
            "test",
            "-p",
            "branchyard-cli",
            "--locked",
            "--offline",
            "--features",
            "schema",
            "--bin",
            "by",
            "rig_json",
        ),
        cargo=True,
    ),
    # docs/getting-started.md run for real: `by` and the fake ACP agent built, then every tested block run
    # in a temporary repository with a private HOME, and every `by` command the new guides show checked
    # against the built `by --help`. CI's walkthrough job runs the same build and the same test.
    Step(
        "build_by",
        "walkthrough",
        (
            "cargo",
            "build",
            "--locked",
            "--offline",
            "-p",
            "branchyard-cli",
            "-p",
            "branchyard-runtime",
            "--bin",
            "by",
            "--bin",
            "fake-acp-agent",
        ),
        cargo=True,
    ),
    Step(
        "test_getting_started_built",
        "walkthrough",
        (PY, "tests/test_getting_started.py"),
        cargo=True,
        env=(("REQUIRE_BINARIES", "1"),),
    ),
)


def plan(steps, only=None, fast=False, fix=False):
    """The steps this run covers, as (step, skipped) pairs: the rewrites first, then the checks,
    each in table order.

    `only` is the --only text. Raises ValueError for a request that cannot run.
    """
    ordered = [step for step in steps if step.fix] + [step for step in steps if not step.fix]
    by_name = {step.name: step for step in ordered}
    names = [name for name in (only or "").split(",") if name]
    unknown = [name for name in names if name not in by_name]
    if unknown:
        raise ValueError(f"unknown step: {', '.join(unknown)}")
    if names:
        chosen = [step for step in ordered if step.name in names]
        for step in chosen:
            if step.fix and not fix:
                raise ValueError(f"{step.name} rewrites files; pass --fix to run it")
            if step.cargo and fast:
                raise ValueError(f"{step.name} is a cargo step, and --fast skips those")
    else:
        chosen = [step for step in ordered if fix or not step.fix]
    return [(step, fast and step.cargo) for step in chosen]


def run_step(step):
    """Run one step: (exit code, its stdout, the text to show when it fails)."""
    env = {key: value for key, value in os.environ.items() if key not in REWRITE_VARIABLES}
    env.update(step.env)
    try:
        done = subprocess.run(list(step.argv), cwd=ROOT, env=env, capture_output=True)
    except OSError as error:
        return 1, b"", str(error)
    shown = done.stderr if step.stdout else done.stdout + done.stderr
    return done.returncode, done.stdout, shown.decode("utf-8", "replace")


def say(status, name):
    print(f"{status:<8}{name}", flush=True)


def main(argv=None, steps=STEPS):
    parser = argparse.ArgumentParser(
        description=__doc__.split("\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--fast", action="store_true", help="skip the cargo steps")
    parser.add_argument("--fix", action="store_true", help="rewrite the generated files and formatting first")
    parser.add_argument("--only", metavar="NAME[,NAME]", help="run only these steps")
    parser.add_argument("--keep-going", action="store_true", help="run past a failing step")
    args = parser.parse_args(argv)
    try:
        chosen = plan(steps, args.only, args.fast, args.fix)
    except ValueError as error:
        parser.error(str(error))
    failed = 0
    for step, skipped in chosen:
        if skipped:
            say("skip", step.name)
            continue
        code, out, shown = run_step(step)
        if code == 0:
            if step.stdout:
                (ROOT / step.stdout).write_bytes(out)
            say("ok", step.name)
            continue
        failed += 1
        say("FAILED", step.name)
        for line in shown.splitlines()[-TAIL_LINES:]:
            print(f"    {line}", flush=True)
        if not args.keep_going:
            break
    if failed:
        print(f"check: FAILED ({failed} {'step' if failed == 1 else 'steps'})", flush=True)
        return 1
    print("check: PASSED", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
