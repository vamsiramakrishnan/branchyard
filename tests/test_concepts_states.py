#!/usr/bin/env python3
"""docs/concepts.md's table of branch states against the code, so neither can drift.

Every variant of `BranchStatus` (crates/branchyard/src/lib.rs) has exactly one row, named by its JSON
`state` value in the first column; no row names a state the enum lacks; and the second column, the
words `by ls` prints for the state, starts with what `status_text` (crates/branchyard-cli/src/render.rs)
prints for it. Standard library only.

Run: `python3 tests/test_concepts_states.py`.
"""

import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
LIB = ROOT / "crates/branchyard/src/lib.rs"
RENDER = ROOT / "crates/branchyard-cli/src/render.rs"
CONCEPTS = ROOT / "docs/concepts.md"


def snake(name):
    """CamelCase to snake_case, as serde's `rename_all = "snake_case"` does."""
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


def enum_states(source):
    """The variants of `BranchStatus`, as their JSON `state` values, in declaration order."""
    start = source.index("pub enum BranchStatus {")
    body = source[start:].split("\n}\n", 1)[0]
    return [snake(name) for name in re.findall(r"^    ([A-Z]\w*)", body, re.M)]


def printed_words(source):
    """What `status_text` prints for each state, up to the first `{` or `:`: {state: words}."""
    start = source.index("pub fn status_text(")
    body = source[start:].split("\n}\n", 1)[0]
    arms = re.findall(r'BranchStatus::(\w+)(?:\s*\{[^}]*\})?\s*=>\s*(?:\(\s*)?(?:format!\(\s*)?"([^"]*)"', body)
    return {snake(name): re.split(r"[{:]", text)[0].strip() for name, text in arms}


def table_rows(markdown):
    """The rows of the branch-state table: {state: shown-as cell}. The table is the one under the
    heading 'Branch states'; its first column holds the state in backticks."""
    section = re.search(r"^#+ Branch states\s*$(.*?)(?=^#+ |\Z)", markdown, re.M | re.S)
    if section is None:
        return {}
    rows = {}
    for line in section.group(1).splitlines():
        match = re.match(r"\|\s*`([a-z_]+)`\s*\|([^|]*)\|", line)
        if match:
            rows[match.group(1)] = match.group(2).strip()
    return rows


class StatesTable(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.states = enum_states(LIB.read_text())
        cls.words = printed_words(RENDER.read_text())
        cls.rows = table_rows(CONCEPTS.read_text())

    def test_the_enum_is_read(self):
        # A parse that finds nothing would make every other test pass.
        self.assertGreaterEqual(len(self.states), 12, self.states)
        self.assertIn("awaiting_plan_approval", self.states)
        self.assertEqual(sorted(self.words), sorted(self.states))

    def test_every_state_has_a_row(self):
        missing = [state for state in self.states if state not in self.rows]
        self.assertEqual(missing, [], "docs/concepts.md 'Branch states' lacks a row for these BranchStatus variants")

    def test_no_row_names_a_state_that_does_not_exist(self):
        extra = [state for state in self.rows if state not in self.states]
        self.assertEqual(extra, [], "docs/concepts.md 'Branch states' has rows for states BranchStatus lacks")

    def test_a_state_has_one_row(self):
        names = re.findall(r"^\|\s*`([a-z_]+)`\s*\|", self.section(), re.M)
        self.assertEqual(sorted(names), sorted(set(names)))

    def test_the_shown_as_column_starts_with_what_by_ls_prints(self):
        for state, shown in self.rows.items():
            printed = self.words.get(state)
            if printed is None:
                continue  # reported by test_no_row_names_a_state_that_does_not_exist
            self.assertTrue(
                shown.replace("`", "").startswith(printed),
                f"{state}: the table says {shown!r}, `by ls` prints {printed!r}",
            )

    def test_the_states_come_in_the_order_the_enum_declares_them(self):
        self.assertEqual(list(self.rows), self.states)

    def section(self):
        text = CONCEPTS.read_text()
        match = re.search(r"^#+ Branch states\s*$(.*?)(?=^#+ |\Z)", text, re.M | re.S)
        return match.group(1) if match else ""


class Parsing(unittest.TestCase):
    def test_snake_case_matches_serde(self):
        self.assertEqual(snake("AwaitingPlanApproval"), "awaiting_plan_approval")
        self.assertEqual(snake("Ready"), "ready")
        self.assertEqual(snake("WaitingOnChildren"), "waiting_on_children")

    def test_enum_states_skip_fields_and_comments(self):
        source = (
            "pub enum BranchStatus {\n    Running,\n    /// doc\n    Blocked {\n        reason: String,\n    },\n"
            "    NoChanges,\n}\n"
        )
        self.assertEqual(enum_states(source), ["running", "blocked", "no_changes"])

    def test_printed_words_take_the_text_before_a_field(self):
        source = (
            "pub fn status_text(s: &BranchStatus) {\n    match s {\n"
            '        BranchStatus::Ready => ("ready".into(), Tone::Green),\n'
            "        BranchStatus::BudgetExceeded { limit } => (\n"
            '            format!("over budget: {}", limit),\n            Tone::Yellow,\n        ),\n'
            '        BranchStatus::Merged { target, .. } => (format!("merged into {target}"), Tone::Blue),\n'
            "    }\n}\n"
        )
        self.assertEqual(
            printed_words(source),
            {"ready": "ready", "budget_exceeded": "over budget", "merged": "merged into"},
        )

    def test_table_rows_read_only_the_branch_states_section(self):
        text = (
            "# Page\n\n| `other` | x |\n\n## Branch states\n\n| State | Shows |\n|---|---|\n"
            "| `ready` | ready | go |\n\n## Next\n\n| `late` | y |\n"
        )
        self.assertEqual(table_rows(text), {"ready": "ready"})


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
