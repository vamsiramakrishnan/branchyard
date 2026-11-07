#!/usr/bin/env python3
"""The skill-discovery eval (tools/skill_eval.py, docs/skill-eval.md), hermetically.

* The scenarios cannot rot: every expected skill is one a turn's tool tree lists, every expected operation is
  one the Anvil mock packages (checked by running the mock), every helper is a catalog harness.
* The scorer counts searched, found, wrong opens, tokens and time from logs written by hand in the shape
  `by log --json` emits (tests/fixtures/skill_eval/logs; the shape comes from crates/branchyard-cli/src/json.rs).
* The runner builds the `by run` line it should, and skips what it cannot run.
* With a built `by` and fake-acp-agent (REQUIRE_BINARIES=1, or BY_BIN and FAKE_ACP_AGENT, as in
  tests/test_getting_started.py) the runner runs three scenarios against the fake agent, scripted to search and
  open a skill, and the scorer reads what that really logged. No model is called.

Run: `python3 tests/test_skill_eval.py`.
"""

import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "tests/fixtures/skill_eval"
LOGS = FIXTURES / "logs"
MOCK = ROOT / "tests/fixtures/anvil/anvil"

spec = importlib.util.spec_from_file_location("skill_eval", ROOT / "tools/skill_eval.py")
assert spec is not None and spec.loader is not None
skill_eval = importlib.util.module_from_spec(spec)
sys.modules["skill_eval"] = skill_eval
spec.loader.exec_module(skill_eval)

sys.path.insert(0, str(ROOT / "tests"))
from test_getting_started import need_binaries  # noqa: E402  (shared with the walkthrough: the built binaries)

UNIVERSE = skill_eval.build_universe()
SCENARIOS = {s.id: s for s in skill_eval.load_scenarios()}


def score(name, meta=None):
    scenario = SCENARIOS[name]
    events = skill_eval.load_events(LOGS / f"{name}.log.json")
    if meta is None:
        meta_path = LOGS / f"{name}.meta.json"
        meta = json.loads(meta_path.read_text()) if meta_path.is_file() else {}
    return skill_eval.score_scenario(scenario, events, meta, UNIVERSE)


def event(kind, at_ms=0, **fields):
    return {"activity": "harness", "event": {"type": kind, **fields}, "at_ms": at_ms}


def started(name, target=None, at_ms=0):
    """A `tool_started` event as `by log --json` writes it; `target` is {"kind", "value"} or absent."""
    fields = {"target": target} if target else {}
    return event("tool_started", at_ms, turn=1, call_id="c", name=name, **fields)


def done(at_ms=9):
    return event("turn_ended", at_ms, turn=1, outcome={"kind": "completed"})


def asked(command=None, at_ms=0, tool="Bash", **inp):
    inp = {"command": command, **inp} if command else inp
    return event("permission_requested", at_ms, turn=1, key="k", tool=tool, input=inp)


class Scenarios(unittest.TestCase):
    def test_there_are_thirty_to_sixty_across_every_area(self):
        self.assertGreaterEqual(len(SCENARIOS), 30)
        self.assertLessEqual(len(SCENARIOS), 60)
        areas = {s.area for s in SCENARIOS.values()}
        for area in (
            "documents",
            "knowledge",
            "design",
            "workflows",
            "runtime",
            "connectors",
            "helpers",
            "distractors",
            "references",
        ):
            self.assertIn(area, areas)

    def test_every_scenario_is_sound(self):
        self.assertEqual(skill_eval.problems(SCENARIOS.values()), [])

    def test_every_built_in_skill_a_turn_lists_is_expected_somewhere(self):
        expected = {name for s in SCENARIOS.values() for name in s.expect_skills}
        self.assertEqual(sorted(skill_eval.tree_skills() - expected), [])

    def test_a_prompt_never_names_the_skill_it_expects(self):
        for s in SCENARIOS.values():
            if s.area == "runtime":
                continue  # asking what tools exist, or to prepare the tools, is the task there
            lowered = re.sub(r"\S+\.[a-z]{2,5}\b", "", s.prompt.lower())  # a file name is not a skill's name
            for name in s.expect_skills:
                self.assertNotIn(name, lowered, f"{s.id}: the prompt names {name}")
                self.assertNotIn(f"{name} skill", lowered, s.id)

    def test_distractors_expect_nothing_and_the_rest_expect_something(self):
        for s in SCENARIOS.values():
            self.assertEqual(s.none, s.area == "distractors" or s.id == "github-unexposed-op", s.id)
            self.assertEqual(bool(s.expected), not s.none, s.id)

    def test_expected_operations_are_what_the_mock_really_packages(self):
        with tempfile.TemporaryDirectory() as tmp:
            packaged = {}
            for bundle in sorted((ROOT / "tests/fixtures/anvil/bundles").iterdir()):
                out = Path(tmp) / bundle.name
                done = subprocess.run(
                    [str(MOCK), "package", "harness", str(bundle), "--out", str(out), "--connector", bundle.name],
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertEqual(done.returncode, 0, done.stderr)
                search = json.loads((out / "search.json").read_text())
                packaged.update({op["id"]: op for op in search["operations"]})
        for s in SCENARIOS.values():
            for op in (*s.expect_operations, *(m for m in s.must_not if "." in m)):
                self.assertIn(op, packaged, f"{s.id}: {op} is not in the mock's search.json")
        # Reading one operation takes `tools schema`/`show`, and the cli line is what a call would use.
        for op, entry in packaged.items():
            self.assertEqual(
                UNIVERSE["cli"].get(op, entry["cli"].replace("bin/", "")), entry["cli"].replace("bin/", "")
            )

    def test_helpers_are_catalog_harnesses(self):
        for s in SCENARIOS.values():
            for helper in s.expect_helpers:
                self.assertTrue(helper == "own" or helper in skill_eval.helper_ids(), s.id)
            self.assertEqual(bool(s.expect_helpers), s.helpers, s.id)

    def test_a_broken_scenario_is_reported(self):
        good = SCENARIOS["docx-status-report"]
        cases = {
            "skill that does not exist": {"expect_skills": ["docxx"]},
            "operation not in the mock": {"expect_operations": ["github.issues.explode"], "expect_skills": []},
            "expects nothing": {"expect_skills": []},
            "none with an expectation": {"none": True},
            "bad regular expression": {"check_matches": {"a.txt": "("}},
            "path outside the tree": {"files": {"../x": "y"}},
            "expected skill not among the chosen": {"skills": ["pdf"]},
            "helper without helpers": {"expect_helpers": ["codex"]},
            "reference of a skill that does not exist": {"expect_skills": ["docxx/references/a.md"]},
            "reference that is not a file of the skill": {"expect_skills": ["calculate/references/none.md"]},
            "reference without .md": {"expect_skills": ["calculate/references/units"]},
            "reference deeper than one file": {"expect_skills": ["calculate/references/a/units.md"]},
            "reference whose skill is not among the chosen": {
                "skills": ["docx"],
                "expect_skills": ["calculate/references/units.md"],
            },
        }
        for why, change in cases.items():
            bad = skill_eval.Scenario(**{**good.__dict__, **change})
            self.assertTrue(skill_eval.problems([bad]), why)

    def test_a_reference_id_is_an_expected_skill(self):
        good = SCENARIOS["docx-status-report"]
        scenario = skill_eval.Scenario(
            **{**good.__dict__, "skills": ["calculate", "docx"], "expect_skills": ["calculate/references/units.md"]}
        )
        self.assertEqual(skill_eval.problems([scenario]), [])
        self.assertEqual(skill_eval.reference_skill("calculate/references/units.md"), "calculate")
        self.assertEqual(skill_eval.reference_skill("calculate"), "calculate")
        self.assertEqual(skill_eval.expected_documents(scenario), [("reference", "calculate/references/units.md")])
        self.assertEqual(len(skill_eval.load_scenarios("references")), 7)
        for s in skill_eval.load_scenarios("references"):
            self.assertTrue(all("/references/" in name for name in s.expect_skills), s.id)

    def test_an_unknown_key_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            old = skill_eval.SCENARIOS
            (Path(tmp) / "x.toml").write_text('[[scenario]]\nid = "x"\nprompt = "p"\nexpect_skill = ["docx"]\n')
            skill_eval.SCENARIOS = Path(tmp)
            try:
                with self.assertRaisesRegex(ValueError, "unknown keys"):
                    skill_eval.load_scenarios()
            finally:
                skill_eval.SCENARIOS = old

    def test_the_glob_picks_by_id_area_or_file(self):
        self.assertEqual(
            {s.id for s in skill_eval.load_scenarios("helper-*")},
            {s.id for s in SCENARIOS.values() if s.area == "helpers"},
        )
        self.assertEqual(len(skill_eval.load_scenarios("distractors")), 4)
        self.assertEqual(
            {s.id for s in skill_eval.load_scenarios("pdf-fill-form,none-arithmetic")},
            {"pdf-fill-form", "none-arithmetic"},
        )


class Scoring(unittest.TestCase):
    def test_search_then_the_right_skill_then_a_wrong_one(self):
        row = score("docx-status-report")
        self.assertTrue(row["searched"])
        self.assertTrue(row["searched_command"])
        self.assertEqual(row["found"], ["skill:docx"])
        self.assertEqual(row["missed"], [])
        self.assertEqual(row["wrong_opens"], ["skill:pptx"])
        self.assertEqual(row["forbidden_opened"], ["pptx"])
        self.assertEqual((row["correct_opens"], row["total_opens"]), (1, 2))
        self.assertEqual(row["tokens"], {"input": 3700, "output": 480, "cached": 900, "total": 4180})
        self.assertEqual(row["seconds"], 5.0)
        self.assertEqual(row["checks"], {"file:status.docx": True})
        self.assertFalse(row["success"], "opening a forbidden skill fails the scenario")
        self.assertEqual(row["harness"], "claude-code")

    def test_the_forbidden_open_is_the_only_thing_that_fails_it(self):
        events = skill_eval.load_events(LOGS / "docx-status-report.log.json")
        kept = [e for e in events if "pptx" not in json.dumps(e)]
        row = skill_eval.score_scenario(
            SCENARIOS["docx-status-report"], kept, {"checks": {"files": {"status.docx": True}}}, UNIVERSE
        )
        self.assertTrue(row["success"])
        self.assertEqual(row["wrong_opens"], [])

    def test_a_log_of_json_lines_and_cumulative_usage(self):
        row = score("xlsx-pivot")
        self.assertEqual(row["found"], ["skill:xlsx"], "the native Skill tool opens a skill")
        self.assertEqual(row["wrong_opens"], ["skill:google-sheets"], "cat of a SKILL.md opens it")
        self.assertFalse(row["searched"], "no search and no INDEX.md read")
        self.assertEqual(row["tokens"]["total"], 2400, "a cumulative total replaces the last, it is not summed")
        self.assertFalse(row["success"])

    def test_an_operation_is_found_by_schema_and_used_by_a_connector_call(self):
        row = score("github-open-issue")
        self.assertEqual(row["found"], ["operation:github.issues.create"])
        self.assertEqual(row["used"], ["github.issues.create"])
        self.assertTrue(row["read_index"], "cat of INDEX.md is a look at the index")
        self.assertTrue(row["searched_command"], "a search after other commands on one line is found")
        self.assertEqual(row["wrong_opens"], [])
        self.assertTrue(row["success"])

    def test_a_call_with_no_schema_still_counts_as_found(self):
        events = [
            {"activity": "prompt", "text": "p", "at_ms": 1},
            {
                "activity": "connector_call",
                "connector_call": {"connector": "github", "operation": "pulls.merge", "decision": "allowed"},
                "at_ms": 2,
            },
            event("turn_ended", 3, turn=1, outcome={"kind": "completed"}),
        ]
        row = skill_eval.score_scenario(SCENARIOS["github-merge-pr"], events, {}, UNIVERSE)
        self.assertEqual(row["found"], ["operation:github.pulls.merge"])
        self.assertFalse(row["searched"])

    def test_another_connectors_operation_is_a_wrong_open(self):
        events = [
            asked("tools schema linear.tickets.search"),
            asked("tools schema github.issues.list"),
            event("turn_ended", 3, turn=1, outcome={"kind": "completed"}),
        ]
        row = skill_eval.score_scenario(SCENARIOS["github-list-issues"], events, {}, UNIVERSE)
        self.assertEqual(row["found"], ["operation:github.issues.list"])
        self.assertEqual(row["wrong_opens"], ["operation:linear.tickets.search"])
        self.assertEqual(row["forbidden_opened"], ["linear.tickets.search"])
        self.assertFalse(row["success"])

    def test_a_distractor_that_searches_but_opens_nothing_succeeds(self):
        row = score("none-write-text-file")
        self.assertTrue(row["searched"], "it read INDEX.md: recorded as a needless search")
        self.assertEqual(row["opened"], [])
        self.assertTrue(row["success"])
        self.assertEqual(row["found"], [])

    def test_a_distractor_that_opens_a_skill_fails(self):
        events = [
            asked(None, file_path="/t/skills/calculate/SKILL.md", tool="Read"),
            event("turn_ended", 3, turn=1, outcome={"kind": "completed"}),
        ]
        row = skill_eval.score_scenario(SCENARIOS["none-arithmetic"], events, {}, UNIVERSE)
        self.assertEqual(row["wrong_opens"], ["skill:calculate"])
        self.assertFalse(row["success"])

    def test_no_usage_events_is_unknown_not_zero(self):
        row = score("none-arithmetic")
        self.assertEqual(row["tokens"], {"input": None, "output": None, "cached": None, "total": None})
        self.assertEqual(row["seconds"], 0.5)

    def test_a_helper_is_found_by_reading_the_helpers_index_and_used_by_a_spawn(self):
        row = score("helper-codex-review")
        self.assertEqual(row["found"], ["helper:codex"])
        self.assertEqual(row["used"], ["codex"])
        self.assertTrue(row["searched"], "helpers/INDEX.md is an index")
        self.assertTrue(row["success"])

    def test_own_means_the_harness_the_run_used(self):
        events = [asked("tools show claude-code"), event("turn_ended", 3, turn=1, outcome={"kind": "completed"})]
        row = skill_eval.score_scenario(
            SCENARIOS["helper-which-available"], events, {"harness": "claude-code"}, UNIVERSE
        )
        self.assertEqual(row["found"], ["helper:claude-code"])

    def test_a_refused_permission_is_not_an_open(self):
        events = skill_eval.load_events(LOGS / "denied-read.log.json")
        row = skill_eval.score_scenario(SCENARIOS["docx-status-report"], events, {}, UNIVERSE)
        self.assertEqual(row["opened"], [])
        self.assertEqual(row["missed"], ["skill:docx"])
        self.assertFalse(row["completed"])

    def test_a_log_that_shows_only_tool_names_is_blind(self):
        events = skill_eval.load_events(LOGS / "blind-docx.log.json")
        row = skill_eval.score_scenario(SCENARIOS["docx-status-report"], events, {}, UNIVERSE)
        self.assertTrue(row["blind"])
        self.assertEqual(row["evidence"], {"tool_calls": 3, "visible_inputs": 0, "narrated": 0, "targets": 0})
        self.assertEqual(row["missed"], ["skill:docx"])

    def test_targets_on_tool_events_show_what_a_call_acted_on(self):
        events = [
            started("Bash", {"kind": "command", "value": "/t/bin/tools search word document | head -3"}),
            started("Read", {"kind": "file", "value": "tools/skills/docx/SKILL.md"}),
            done(),
        ]
        row = skill_eval.score_scenario(SCENARIOS["docx-status-report"], events, {"checks": {}}, UNIVERSE)
        self.assertTrue(row["searched_command"])
        self.assertEqual(row["found"], ["skill:docx"])
        self.assertFalse(row["blind"])
        self.assertEqual(row["evidence"], {"tool_calls": 2, "visible_inputs": 0, "narrated": 0, "targets": 2})

    def test_a_skill_tool_target_opens_the_skill_and_an_edit_target_opens_nothing(self):
        events = [
            started("Skill", {"kind": "skill", "value": "docx"}),
            started("Edit", {"kind": "file", "value": "tools/skills/pdf/SKILL.md"}),
            started("mcp__github__list_issues", {"kind": "mcp", "value": "github.list_issues"}),
            done(),
        ]
        acts, seen = skill_eval.actions(events)
        self.assertEqual([(a.kind, a.text) for a in acts], [("skill", "docx"), ("tool", "github.list_issues")])
        self.assertEqual(seen["targets"], 3)
        self.assertEqual(skill_eval.collect(acts, UNIVERSE).skills, {"docx"})

    def test_a_tool_call_without_a_target_is_still_blind_and_one_with_a_target_is_not(self):
        blind = skill_eval.actions([started("Bash"), started("Read"), done()])[1]
        self.assertEqual((blind["tool_calls"], blind["targets"]), (2, 0))
        mixed = skill_eval.actions([started("WebSearch"), started("Bash", {"kind": "command", "value": "ls"}), done()])[
            1
        ]
        self.assertEqual((mixed["tool_calls"], mixed["targets"]), (2, 1))

    def test_a_refused_call_loses_its_target_too(self):
        events = [
            started("Read", {"kind": "file", "value": "tools/skills/docx/SKILL.md"}),
            asked(None, file_path="tools/skills/docx/SKILL.md", tool="Read"),
            {"activity": "decision", "allowed": False, "at_ms": 2},
            done(),
        ]
        acts, _ = skill_eval.actions(events)
        self.assertEqual(acts, [])

    def test_narrated_commands_count_and_are_marked(self):
        text = "sh: 0\n$ /t/bin/tools search word\nresult\n$ cat /t/skills/docx/SKILL.md\n"
        events = [
            event("message_delta", 1, turn=1, text=text[:20]),
            event("message_delta", 2, turn=1, text=text[20:]),
            event("turn_ended", 3, turn=1, outcome={"kind": "completed"}),
        ]
        row = skill_eval.score_scenario(SCENARIOS["docx-layout-check"], events, {"checks": {}}, UNIVERSE)
        self.assertEqual(row["found"], ["skill:docx"])
        self.assertTrue(row["searched_command"])
        self.assertEqual(row["evidence"]["narrated"], 2)
        self.assertFalse(row["blind"])

    def test_a_file_read_that_is_not_a_skill_is_ignored(self):
        events = [
            asked(None, file_path="/repo/notes/docx/SKILL.md.bak", tool="Read"),
            asked("ls /t/skills/docx/SKILL.md"),
            asked(None, file_path="/t/skills/docx/SKILL.md", tool="Write"),
            event("turn_ended", 3, turn=1, outcome={"kind": "completed"}),
        ]
        row = skill_eval.score_scenario(SCENARIOS["docx-layout-check"], events, {}, UNIVERSE)
        self.assertEqual(row["opened"], [])

    def test_a_skill_routed_through_its_index_is_not_yet_found(self):
        events = [asked("cat /t/skills/docx/INDEX.md"), event("turn_ended", 3, turn=1, outcome={"kind": "completed"})]
        evidence = skill_eval.collect(skill_eval.actions(events)[0], UNIVERSE)
        self.assertEqual(evidence.routed, {"docx"})
        self.assertEqual(evidence.skills, set())
        self.assertTrue(evidence.read_index)

    def test_the_discovery_skill_is_never_a_wrong_open_but_can_be_the_expected_one(self):
        events = [
            asked(None, file_path="/t/skills/find-tools/SKILL.md", tool="Read"),
            event("turn_ended", 3, turn=1, outcome={"kind": "completed"}),
        ]
        row = skill_eval.score_scenario(SCENARIOS["docx-layout-check"], events, {}, UNIVERSE)
        self.assertEqual(row["wrong_opens"], [])
        row = skill_eval.score_scenario(SCENARIOS["find-tools-vague"], events, {}, UNIVERSE)
        self.assertEqual(row["found"], ["skill:find-tools"])

    def test_checks_that_were_never_run_leave_success_unknown(self):
        events = skill_eval.load_events(LOGS / "docx-status-report.log.json")
        kept = [e for e in events if "pptx" not in json.dumps(e)]
        row = skill_eval.score_scenario(SCENARIOS["docx-status-report"], kept, {}, UNIVERSE)
        self.assertIsNone(row["success"])
        row = skill_eval.score_scenario(
            SCENARIOS["docx-status-report"], kept, {"checks": {"files": {"status.docx": False}}}, UNIVERSE
        )
        self.assertFalse(row["success"])

    def test_a_turn_that_did_not_complete_is_not_a_success(self):
        events = [
            asked("tools search word"),
            asked("cat /t/skills/docx/SKILL.md"),
            event("turn_ended", 3, turn=1, outcome={"kind": "failed", "message": "x"}),
        ]
        row = skill_eval.score_scenario(SCENARIOS["docx-layout-check"], events, {}, UNIVERSE)
        self.assertEqual(row["found"], ["skill:docx"])
        self.assertFalse(row["success"])

    def test_a_command_check_reads_the_log(self):
        scenario = skill_eval.Scenario(
            **{**SCENARIOS["docx-layout-check"].__dict__, "check_commands": [r"soffice .*flyer"]}
        )
        events = [
            asked("cat /t/skills/docx/SKILL.md"),
            asked("soffice --convert-to pdf flyer.docx"),
            event("turn_ended", 3, turn=1, outcome={"kind": "completed"}),
        ]
        self.assertTrue(skill_eval.score_scenario(scenario, events, {}, UNIVERSE)["success"])
        self.assertFalse(skill_eval.score_scenario(scenario, events[:1] + events[2:], {}, UNIVERSE)["success"])


def have(*modules):
    try:
        for m in modules:
            __import__(m)
    except ImportError:
        return False
    return True


TOOLKIT = have("docx", "openpyxl", "pptx", "reportlab", "pypdf")


@unittest.skipUnless(TOOLKIT, "the artifact toolkit's libraries are not installed")
class Fixtures(unittest.TestCase):
    """The binary input files the scenarios name are generated, reproducibly, with the toolkit's libraries."""

    def test_every_named_input_has_a_generator_that_is_reproducible(self):
        names = sorted({n for s in SCENARIOS.values() for n in s.fixtures})
        self.assertEqual(len(names), 14)
        for name in names:
            first, second = skill_eval.FIXTURE_BUILDERS[name](), skill_eval.FIXTURE_BUILDERS[name]()
            self.assertEqual(first, second, name)
            self.assertGreater(len(first), 200, name)

    def test_fourteen_scenarios_get_their_inputs(self):
        # The thirteen that named binary inputs, and reference-pdf-render-review.
        self.assertEqual(sum(1 for s in SCENARIOS.values() if s.fixtures), 14)

    def test_the_files_open_with_the_libraries_that_made_them(self):
        import io

        import docx
        import openpyxl
        import pptx
        import pypdf

        build = lambda name: skill_eval.FIXTURE_BUILDERS[name]()  # noqa: E731
        text = "\n".join(p.text for p in docx.Document(io.BytesIO(build("contract.docx"))).paragraphs)
        self.assertEqual(text.count("Acme Ltd"), 3)
        self.assertEqual(len(docx.Document(io.BytesIO(build("spec.docx"))).tables), 3)
        sheet = openpyxl.load_workbook(io.BytesIO(build("sales.xlsx")))["Sales"]
        self.assertEqual([c.value for c in sheet[1]], ["region", "product", "amount"])
        self.assertEqual(sheet.max_row, 7)
        formulas = [
            c.value for row in openpyxl.load_workbook(io.BytesIO(build("forecast.xlsx")))["Forecast"] for c in row
        ]
        self.assertIn("=SUM(B2:C2)", formulas, "the totals still cover the old range")
        self.assertEqual(len(pptx.Presentation(io.BytesIO(build("pitch.pptx"))).slides), 5)
        self.assertEqual(len(pypdf.PdfReader(io.BytesIO(build("report.pdf"))).pages), 3)
        self.assertEqual(len(pypdf.PdfReader(io.BytesIO(build("part1.pdf"))).pages), 2)
        self.assertEqual(set(pypdf.PdfReader(io.BytesIO(build("application.pdf"))).get_fields()), {"name", "date"})

    def test_seeding_writes_the_binary_inputs_next_to_the_text_ones(self):
        with tempfile.TemporaryDirectory() as tmp:
            skill_eval.seed(Path(tmp), SCENARIOS["pdf-merge-stamp"])
            self.assertEqual(sorted(p.name for p in Path(tmp).iterdir()), ["part1.pdf", "part2.pdf"])
            skill_eval.seed(Path(tmp), SCENARIOS["docx-status-report"])
            self.assertTrue((Path(tmp) / "status.txt").is_file())

    def test_an_archive_check_reads_the_text_inside_an_office_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            skill_eval.seed(Path(tmp), SCENARIOS["docx-redline-edit"])
            found = skill_eval.evaluate_checks(SCENARIOS["docx-redline-edit"], Path(tmp))
            self.assertEqual(found["archive"], {"contract.docx": False}, "'Acme Holdings Ltd' is not there yet")
            self.assertEqual(found["files"], {"contract.orig.docx": False})
            self.assertTrue(skill_eval.archive_matches(Path(tmp) / "contract.docx", "Acme Ltd"))
            self.assertTrue(skill_eval.archive_matches(Path(tmp) / "contract.docx", r"word/document\.xml"))
            self.assertFalse(skill_eval.archive_matches(Path(tmp) / "missing.docx", "x"))
            (Path(tmp) / "plain.docx").write_text("not a zip")
            self.assertFalse(skill_eval.archive_matches(Path(tmp) / "plain.docx", "x"))

    def test_the_formula_check_wants_the_range_fixed(self):
        scenario = SCENARIOS["xlsx-fix-formulas"]
        with tempfile.TemporaryDirectory() as tmp:
            skill_eval.seed(Path(tmp), scenario)
            self.assertFalse(skill_eval.evaluate_checks(scenario, Path(tmp))["archive"]["forecast.xlsx"])
            import openpyxl

            book = openpyxl.load_workbook(Path(tmp) / "forecast.xlsx")
            book.active["E2"] = "=SUM(B2:D2)"
            book.save(Path(tmp) / "forecast.xlsx")
            self.assertTrue(skill_eval.evaluate_checks(scenario, Path(tmp))["archive"]["forecast.xlsx"])


class FixtureRules(unittest.TestCase):
    def test_a_prompt_may_not_name_a_file_nobody_provides(self):
        broken = skill_eval.Scenario(**{**SCENARIOS["pdf-merge-stamp"].__dict__, "fixtures": ["part1.pdf"]})
        why = skill_eval.problems([broken])
        self.assertTrue(any("part2.pdf" in w and "neither seeded" in w for w in why), why)

    def test_a_fixture_needs_a_generator_and_an_office_name(self):
        base = SCENARIOS["pdf-merge-stamp"].__dict__
        odd = skill_eval.Scenario(**{**base, "fixtures": ["part1.pdf", "part2.pdf", "other.pdf", "x.txt"]})
        why = " ".join(skill_eval.problems([odd]))
        self.assertIn("no generator", why)
        self.assertIn("must be a docx, xlsx, pptx, pdf or csv name", why)

    def test_a_scenario_is_skipped_when_the_toolkit_is_missing(self):
        real = skill_eval.need

        def missing(module):
            raise RuntimeError(f"{module} is needed to generate input files")

        skill_eval.need = missing
        try:
            why = skill_eval.runnable(SCENARIOS["pdf-extract-text"], skill_eval.RunOptions(harness="claude-code"))
        finally:
            skill_eval.need = real
        self.assertIn("needed to generate input files", why)


class Aggregation(unittest.TestCase):
    def setUp(self):
        self.report = skill_eval.score_dir(LOGS, skill_eval.load_scenarios("*"))

    def test_only_scenarios_with_a_log_are_counted(self):
        self.assertEqual(self.report["overall"]["scenarios"], 6)
        self.assertTrue(any("have no log and are not counted" in n for n in self.report["notes"]))

    def test_precision_recall_tokens_and_rates(self):
        a = self.report["overall"]
        self.assertEqual(a["successes"], 4)
        self.assertEqual(a["success_rate"], round(4 / 6, 4))
        self.assertEqual(a["discovery_recall"], 1.0, "every expected thing was found, wrongly opened ones aside")
        self.assertEqual(a["discovery_precision"], 0.6, "3 of 5 opens were right")
        self.assertEqual(a["wrong_opens"], 2)
        self.assertEqual(a["searched_rate"], 0.75, "everything but the xlsx scenario searched or read an index")
        self.assertEqual(a["needless_search_rate"], 0.5, "one of two distractors read INDEX.md")
        self.assertEqual(a["tokens_total"], 7500)
        self.assertEqual(a["tokens_per_success"], 1875)
        self.assertEqual(a["tokens_missing"], 3)

    def test_by_harness_splits_the_rows(self):
        self.assertEqual(set(self.report["by_harness"]), {"claude-code", "codex"})
        self.assertEqual(self.report["by_harness"]["codex"]["scenarios"], 2)
        self.assertEqual(self.report["by_harness"]["claude-code"]["scenarios"], 4)

    def test_the_markdown_has_a_row_per_harness_and_per_scenario(self):
        text = skill_eval.markdown(self.report)
        self.assertIn("| claude-code | 4 |", text)
        self.assertIn("| codex | 2 |", text)
        self.assertIn("| all | 6 |", text)
        self.assertIn("| docx-status-report | claude-code | yes | 1/1 | skill:pptx | 4,180 | 5.0 | no |", text)
        self.assertIn("| none-arithmetic | codex | no | none needed |", text)

    def test_the_report_round_trips_as_json(self):
        self.assertEqual(json.loads(json.dumps(self.report)), self.report)
        self.assertEqual(self.report["schema"], "branchyard.skill-eval/v1")

    def test_the_score_command_writes_both_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            shown = subprocess.run(
                [sys.executable, str(ROOT / "tools/skill_eval.py"), "score", str(LOGS), "--out", tmp],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(shown.returncode, 0, shown.stderr)
            self.assertIn("# Skill-discovery eval", shown.stdout)
            self.assertEqual(json.loads((Path(tmp) / "report.json").read_text())["overall"]["scenarios"], 6)
            self.assertEqual((Path(tmp) / "report.md").read_text(), shown.stdout)


class Runner(unittest.TestCase):
    def options(self, **kw):
        return skill_eval.RunOptions(harness="claude-code", **kw)

    def test_the_by_run_line(self):
        argv = skill_eval.run_argv(SCENARIOS["docx-status-report"], self.options(model="opus"), Path("/r"))
        self.assertEqual(argv[:2], ["by", "run"])
        self.assertEqual(argv[2], SCENARIOS["docx-status-report"].prompt)
        self.assertEqual(
            argv[3:],
            [
                "--name",
                "docx-status-report",
                "--harness",
                "claude-code",
                "--yes",
                "--max-minutes",
                "10",
                "--model",
                "opus",
                "--skill",
                "docx",
                "--skill",
                "pdf",
                "--skill",
                "xlsx",
                "--skill",
                "pptx",
                "--skill",
                "visual-design",
                "--skill",
                "artifact-tools",
            ],
        )

    def test_helpers_connectors_and_a_fake_command(self):
        argv = skill_eval.run_argv(SCENARIOS["helper-which-available"], self.options(command="/x/fake"), Path("/r"))
        self.assertIn("--delegate=1", argv)
        self.assertNotIn("--helpers", argv, "the default envelope already allows the harness's own profile")
        self.assertEqual(argv[argv.index("--command") + 1], "/x/fake")
        argv = skill_eval.run_argv(SCENARIOS["helper-codex-review"], self.options(), Path("/r"))
        self.assertEqual(
            argv[argv.index("--helpers") + 1], "claude-code,codex", "own harness first, then the scenario's"
        )
        argv = skill_eval.run_argv(SCENARIOS["github-open-issue"], self.options(anvil="a", bundles="b"), Path("/r"))
        self.assertEqual(
            [argv[i + 1] for i, a in enumerate(argv) if a == "--connector"], ["github:write", "linear:read"]
        )
        self.assertIn("--isolated", argv)

    def test_a_script_replaces_the_prompt_and_fills_the_paths(self):
        options = self.options(script={"docx-status-report": "SH ls {tools}\nSH cd {repo}"})
        prompt = skill_eval.scenario_prompt(SCENARIOS["docx-status-report"], options, Path("/r"))
        self.assertEqual(prompt, "SH ls /r/.branchyard/capabilities/*/.branchyard/tools\nSH cd /r")
        self.assertEqual(
            skill_eval.scenario_prompt(SCENARIOS["pdf-fill-form"], options, Path("/r")),
            SCENARIOS["pdf-fill-form"].prompt,
        )

    def test_what_cannot_run_is_skipped_with_the_reason(self):
        options = self.options()
        self.assertIn("gateway", skill_eval.runnable(SCENARIOS["github-open-issue"], options))
        self.assertIsNone(skill_eval.runnable(SCENARIOS["github-open-issue"], self.options(anvil="a", bundles="b")))
        self.assertIsNone(skill_eval.runnable(SCENARIOS["helper-codex-review"], options))
        self.assertIsNone(skill_eval.runnable(SCENARIOS["docx-status-report"], options))

    def test_the_file_checks_look_in_the_worktree(self):
        with tempfile.TemporaryDirectory() as tmp:
            (Path(tmp) / "hello.txt").write_text("hello from the eval\n")
            found = skill_eval.evaluate_checks(SCENARIOS["none-write-text-file"], Path(tmp))
            self.assertEqual(found, {"files": {}, "matches": {"hello.txt": True}, "archive": {}})
            (Path(tmp) / "hello.txt").write_text("nope\n")
            self.assertEqual(
                skill_eval.evaluate_checks(SCENARIOS["none-write-text-file"], Path(tmp))["matches"],
                {"hello.txt": False},
            )
            self.assertEqual(
                skill_eval.evaluate_checks(SCENARIOS["docx-status-report"], Path(tmp))["files"], {"status.docx": False}
            )

    def test_the_sandbox_env_has_a_private_home_and_no_tokens(self):
        os.environ["SKILL_EVAL_TEST_TOKEN"] = "secret"
        try:
            with tempfile.TemporaryDirectory() as tmp:
                env = skill_eval.sandbox_env(Path(tmp))
        finally:
            del os.environ["SKILL_EVAL_TEST_TOKEN"]
        self.assertTrue(env["HOME"].startswith(tmp))
        self.assertNotIn("SKILL_EVAL_TEST_TOKEN", env)


class Sizes(unittest.TestCase):
    def test_the_static_sizes_name_what_a_turn_pays_for(self):
        found = skill_eval.sizes()
        for name in ("find-tools SKILL.md", "delegate SKILL.md", "leaf.md", "skills/INDEX.md (shipped)"):
            self.assertIn(name, found)
        self.assertEqual(found["leaf.md"]["bytes"], (ROOT / "plugins/branchyard/leaf.md").stat().st_size)

    def test_each_delegate_reference_has_a_row(self):
        found = skill_eval.sizes()
        references = sorted((ROOT / "plugins/branchyard/skills/delegate/references").glob("*.md"))
        self.assertTrue(references)
        for reference in references:
            row = found[f"delegate references/{reference.name} (read on demand)"]
            self.assertEqual(row["bytes"], reference.stat().st_size)

    def test_a_mounted_tree_adds_its_index_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            tree = Path(tmp)
            (tree / "helpers").mkdir()
            (tree / "INDEX.md").write_text("x" * 10)
            (tree / "helpers/INDEX.md").write_text("y" * 4)
            found = skill_eval.sizes(tree)
        self.assertEqual(found["tools/INDEX.md (mounted)"]["bytes"], 10)
        self.assertEqual(found["tools/helpers/INDEX.md (mounted)"]["bytes"], 4)


class Rank(unittest.TestCase):
    """`rank`: each prompt as a query to the shipped `tools` script over a tree, no model."""

    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        cls.tree = Path(cls.tmp.name) / "tools"
        cls.contents = skill_eval.shipped_tree(Path(cls.tmp.name))
        cls.report = skill_eval.rank_scenarios(list(SCENARIOS.values()), cls.tree)

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def test_the_shipped_tree_is_what_a_turn_mounts_today(self):
        listed = json.loads((ROOT / "plugins/branchyard/capabilities.json").read_text(encoding="utf-8"))
        self.assertEqual(self.contents["skills"], [s["id"] for s in listed["skills"]])
        self.assertEqual(self.contents["connectors"], ["github", "linear"])
        self.assertEqual(self.contents["notes"], [])
        index = (self.tree / "INDEX.md").read_text(encoding="utf-8")
        self.assertIn("## Skills", index)
        self.assertIn("- github", index)
        self.assertTrue((self.tree / "skills/docx/SKILL.md").is_file())
        self.assertTrue((self.tree / "connectors/github/search.json").is_file())

    def test_every_expected_id_is_ranked_or_absent_for_a_reason(self):
        rows = {r["id"]: r for r in self.report["scenarios"]}
        self.assertNotIn("none-arithmetic", rows)  # a distractor expects nothing
        for row in rows.values():
            for e in row["expected"]:
                if e["absent"]:
                    self.assertEqual(e["kind"], "helper", e)  # the shipped tree has no helpers/INDEX.md
                else:
                    self.assertIsInstance(e["rank"], int, e)
        overall = self.report["overall"]
        self.assertEqual(overall["scenarios"], len(rows))
        self.assertEqual(overall["expected"] + overall["absent"], sum(len(r["expected"]) for r in rows.values()))
        self.assertEqual(overall["unranked"], 0)
        self.assertEqual(self.report["refused"], [])

    def test_the_reference_scenarios_reach_the_top_three(self):
        """docs/skill-registry.md, R1: the references.toml scenarios reach a top-3 rate of 0.9 or better."""
        ranks = [
            e["rank"]
            for r in self.report["scenarios"]
            if r["area"] == "references"
            for e in r["expected"]
            if not e["absent"]
        ]
        self.assertEqual(len(ranks), 7)
        self.assertGreaterEqual(sum(1 for at in ranks if at <= 3) / len(ranks), 0.9, ranks)

    def test_the_whole_set_keeps_its_top_three_rate(self):
        """The hermetic number docs/skill-eval.md quotes; a change that lowers it is read before it merges."""
        self.assertGreaterEqual(self.report["overall"]["top3_rate"], 0.95, self.report["overall"])

    def test_plain_turns_the_chosen_factor_off(self):
        plain = skill_eval.rank_scenarios(list(SCENARIOS.values()), self.tree, plain=True)
        self.assertTrue(plain["plain"])
        self.assertFalse(self.report["plain"])
        self.assertEqual(plain["overall"]["expected"], self.report["overall"]["expected"])
        # Without the factor a reference can outrank a tool of equal match, so the rate can only fall.
        self.assertLessEqual(plain["overall"]["top3_rate"], self.report["overall"]["top3_rate"])

    def test_the_markdown_and_the_command(self):
        text = skill_eval.rank_markdown(self.report)
        self.assertIn("top-3", text)
        self.assertIn("| reference-mermaid-recipe | reference:diagram-design/references/diagrams.md |", text)
        done = subprocess.run(
            [sys.executable, str(ROOT / "tools/skill_eval.py"), "rank", "--scenarios", "references", "--json"],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(done.returncode, 0, done.stderr)
        report = json.loads(done.stdout)
        self.assertEqual(report["schema"], "branchyard.skill-eval-rank/v1")
        self.assertEqual(report["overall"]["scenarios"], 7)

    def test_another_commits_tools_script_ranks_the_same_tree(self):
        """`--tools` ranks with another copy of the script, so a before and after can be measured at one tree."""
        copy = Path(self.tmp.name) / "tools-copy.py"
        copy.write_text(skill_eval.TOOLS_SCRIPT.read_text(encoding="utf-8"), encoding="utf-8")
        again = skill_eval.rank_scenarios(list(SCENARIOS.values()), self.tree, script=copy)
        self.assertEqual(again["overall"], self.report["overall"])
        self.assertEqual(again["tools"], str(copy))


class FakeAgentEndToEnd(unittest.TestCase):
    """The runner, `by run`, `by log --json` and the scorer together, on the fake ACP agent."""

    def test_three_scripted_scenarios_are_run_and_scored(self):
        by, fake = need_binaries()
        script = FIXTURES / "fake-script.json"
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "out"
            done = subprocess.run(
                [
                    sys.executable,
                    str(ROOT / "tools/skill_eval.py"),
                    "run",
                    "--harness",
                    "gemini-cli",
                    "--by",
                    str(by),
                    "--command",
                    str(fake),
                    "--script",
                    str(script),
                    "--scenarios",
                    "docx-status-report,xlsx-pivot,none-write-text-file",
                    "--out",
                    str(out),
                ],
                capture_output=True,
                text=True,
                check=False,
                timeout=300,
                env={**os.environ, "TMPDIR": tmp},
            )
            self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
            report = json.loads((out / "report.json").read_text())
            rows = {r["id"]: r for r in report["scenarios"]}
            skipped = out / "skipped.json"
            why = skipped.read_text(encoding="utf-8") if skipped.is_file() else "{}"
            self.assertEqual(set(rows), {"docx-status-report", "xlsx-pivot", "none-write-text-file"}, f"skipped: {why}")

            docx = rows["docx-status-report"]
            self.assertTrue(docx["searched_command"])
            self.assertEqual(docx["found"], ["skill:docx"])
            self.assertEqual(docx["wrong_opens"], [])
            self.assertEqual(docx["checks"], {"file:status.docx": True})
            self.assertTrue(docx["success"])
            # The agent's calls were read from the log's targets, not from narrated lines.
            self.assertEqual(docx["evidence"]["narrated"], 0)
            self.assertGreaterEqual(docx["evidence"]["targets"], 3)
            self.assertFalse(docx["blind"])
            events = skill_eval.load_events(out / "docx-status-report.log.json")
            targets = [e["event"]["target"] for e in events if e.get("event", {}).get("type") == "tool_started"]
            self.assertIn({"kind": "command", "value": "echo x > status.docx"}, targets)
            self.assertTrue(
                any(t["value"].startswith("tools/bin/tools search word document") for t in targets), targets
            )
            self.assertTrue(
                any(t["kind"] == "file" and t["value"].endswith("skills/docx/SKILL.md") for t in targets), targets
            )
            meta = json.loads((out / "docx-status-report.meta.json").read_text())
            self.assertTrue(meta["skills_chosen_applied"])

            xlsx = rows["xlsx-pivot"]
            self.assertTrue(xlsx["searched"])
            self.assertEqual(xlsx["missed"], ["skill:xlsx"])
            self.assertEqual(xlsx["wrong_opens"], ["skill:google-sheets"])
            self.assertFalse(xlsx["success"])

            none = rows["none-write-text-file"]
            self.assertFalse(none["searched"])
            self.assertTrue(none["success"])
            self.assertEqual(none["checks"], {"match:hello.txt": True})

            self.assertEqual(report["overall"]["successes"], 2)
            self.assertTrue(
                any(n.startswith("SCRIPTED") for n in report["notes"]), "a scripted run says it measures nothing"
            )
            self.assertTrue((out / "report.md").is_file())
            # The score command reads the same directory and agrees.
            again = skill_eval.score_dir(out, skill_eval.load_scenarios("*"))
            self.assertEqual(again["overall"], report["overall"])


if __name__ == "__main__":
    if shutil.which("python3") is None:  # pragma: no cover
        sys.exit("python3 is required")
    unittest.main()
