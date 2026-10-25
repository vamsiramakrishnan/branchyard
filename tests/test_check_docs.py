#!/usr/bin/env python3
"""tools/check_docs.py: GitHub-style heading anchors, the link check for `file.md#anchor`, and the
date-order rule for the opening lines it recognises. Standard library only.

Run: `python3 tests/test_check_docs.py`.
"""

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("check_docs", ROOT / "tools" / "check_docs.py")
assert spec is not None and spec.loader is not None
check_docs = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check_docs)


class Slugs(unittest.TestCase):
    def test_github_slugs(self):
        cases = {
            "6. Agent Substrate cluster": "6-agent-substrate-cluster",
            "Gemini through the OpenAI-compatible endpoint": "gemini-through-the-openai-compatible-endpoint",
            "Resolved upstream incompatibility": "resolved-upstream-incompatibility",
            "`by init plugin`": "by-init-plugin",
            "Install, sign-in and key": "install-sign-in-and-key",
            'What "qualified" means': "what-qualified-means",
            "A — B": "a--b",
            "The [wire](wire.md) format": "the-wire-format",
            "snake_case_name and _emphasis_": "snake_case_name-and-emphasis",
            "Why *not* PGMQ?": "why-not-pgmq",
            "<code>x</code> in HTML": "x-in-html",
        }
        for heading, expected in cases.items():
            self.assertEqual(check_docs.slug(heading), expected, heading)

    def test_a_repeated_heading_gets_a_numbered_suffix(self):
        found = check_docs.anchors("# Page\n\n## Notes\n\ntext\n\n## Notes\n\n## Notes\n")
        self.assertEqual(found, {"page", "notes", "notes-1", "notes-2"})

    def test_headings_in_a_code_fence_are_not_anchors(self):
        found = check_docs.anchors("# Real\n\n```sh\n# comment, not a heading\n```\n")
        self.assertEqual(found, {"real"})

    def test_a_closing_hash_run_and_an_html_id_are_handled(self):
        found = check_docs.anchors('## Closed ##\n\n<a id="custom-target"></a>\n<a name="old-style"></a>\n')
        self.assertEqual(found, {"closed", "custom-target", "old-style"})


class Links(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        (self.root / "docs").mkdir()
        (self.root / "docs" / "target.md").write_text("# Target\n\n## 6. Agent Substrate cluster\n\n## Notes\n")
        (self.root / "docs" / "data.json").write_text("{}")
        patcher = mock.patch.object(check_docs, "ROOT", self.root)
        patcher.start()
        self.addCleanup(patcher.stop)

    def problems(self, body):
        source = self.root / "docs" / "page.md"
        return check_docs.link_problems(source, body, {})

    def test_a_good_anchor_passes(self):
        self.assertEqual(self.problems("[x](target.md#6-agent-substrate-cluster) [y](target.md#notes)"), [])

    def test_a_renamed_heading_is_reported_with_its_link(self):
        found = self.problems("[x](target.md#6-agent-substrate-cluster-model-calls)")
        self.assertEqual(len(found), 1)
        self.assertIn("docs/page.md -> target.md#6-agent-substrate-cluster-model-calls", found[0])
        self.assertIn("no heading with that anchor", found[0])

    def test_an_anchor_is_case_and_percent_insensitive(self):
        self.assertEqual(self.problems("[x](target.md#Notes)"), [])

    def test_a_missing_file_is_still_reported(self):
        self.assertEqual(self.problems("[x](nowhere.md#a)"), ["docs/page.md -> nowhere.md#a"])

    def test_an_anchor_alone_names_the_page_itself(self):
        source = self.root / "docs" / "page.md"
        body = "# Page\n\n## Section\n\n[a](#section) [b](#missing)"
        source.write_text(body)
        self.assertEqual(
            check_docs.link_problems(source, body, {}),
            ["docs/page.md -> #missing (no heading with that anchor)"],
        )

    def test_links_in_code_fences_and_external_urls_are_not_checked(self):
        body = "```\n[x](nowhere.md#a)\n```\n[y](https://example.com/a.md#b)\n"
        self.assertEqual(self.problems(body), [])

    def test_a_fragment_on_a_file_that_is_not_markdown_is_not_checked(self):
        self.assertEqual(self.problems("[x](data.json#anything)"), [])


class Dates(unittest.TestCase):
    def problems(self, text):
        return check_docs.date_problems(check_docs.ROOT / "docs" / "x.md", text)

    def test_a_research_baseline_opener_is_a_dated_opener(self):
        text = "# Page\n\nResearch baseline: 16 September 2026.\n\n**Status, 3 October 2026.** Something newer.\n"
        found = self.problems(text)
        self.assertEqual(len(found), 1)
        self.assertIn("update the opening line", found[0])

    def test_an_older_section_is_fine(self):
        text = "# Page\n\nResearch baseline: 16 October 2026.\n\n**Status, 3 October 2026.** Older.\n"
        self.assertEqual(self.problems(text), [])

    def test_written_still_works(self):
        text = "# Page\n\nWritten 1 October 2026.\n\n**Status, 3 October 2026.** Newer.\n"
        self.assertEqual(len(self.problems(text)), 1)


class Repository(unittest.TestCase):
    def test_every_local_link_and_anchor_in_the_repository_resolves(self):
        offered, problems = {}, []
        for source in sorted(ROOT.glob("*.md")) + sorted((ROOT / "docs").rglob("*.md")):
            problems += check_docs.link_problems(source, source.read_text(), offered)
        self.assertEqual(problems, [])


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
