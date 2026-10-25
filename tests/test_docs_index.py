#!/usr/bin/env python3
"""docs/README.md is the table of contents: every Markdown page under docs/ is linked from it.

A page nobody can find from the index does not exist for a newcomer. The rule has no exclusion list:
a generated or history page is listed too, under the shelf that says what it is, and a page that
belongs to a group (the dogfood reports) is linked from the row of the group's first page.
"""

import re
import sys
import tempfile
import unittest
from pathlib import Path
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[1]
FENCE = re.compile(r"(?ms)^(```|~~~).*?^\1[^\n]*$")
LINK = re.compile(r"\[[^\]]*\]\(([^)\s]+)\)")


def linked_pages(index):
    """The files `index` links to, as resolved paths."""
    body = FENCE.sub("", index.read_text())
    found = set()
    for target in LINK.findall(body):
        url = urlsplit(target)
        if url.scheme or url.netloc or not url.path:
            continue
        found.add((index.parent / unquote(url.path)).resolve())
    return found


def unlisted_pages(docs):
    """Every Markdown page under `docs` that its README.md does not link, relative to `docs`."""
    listed = linked_pages(docs / "README.md")
    pages = sorted(p for p in docs.rglob("*.md") if p != docs / "README.md")
    return [p.relative_to(docs).as_posix() for p in pages if p.resolve() not in listed]


class DocsIndex(unittest.TestCase):
    def test_every_page_in_the_repository_is_listed(self):
        self.assertEqual(unlisted_pages(ROOT / "docs"), [], "add the page to docs/README.md under its shelf")

    def test_the_index_exists_and_lists_something(self):
        self.assertGreater(len(linked_pages(ROOT / "docs" / "README.md")), 20)


class Detection(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.docs = Path(self.tmp.name) / "docs"
        (self.docs / "nested").mkdir(parents=True)
        for name in ("a.md", "b.md", "nested/c.md"):
            (self.docs / name).write_text(f"# {name}\n")

    def tearDown(self):
        self.tmp.cleanup()

    def index(self, text):
        (self.docs / "README.md").write_text(text)

    def test_a_page_the_index_does_not_link_is_reported(self):
        self.index("# Docs\n\n| [A](a.md) | q |\n")
        self.assertEqual(unlisted_pages(self.docs), ["b.md", "nested/c.md"])

    def test_a_nested_page_counts_when_linked_with_its_path(self):
        self.index("[A](a.md) [B](b.md) [C](nested/c.md)\n")
        self.assertEqual(unlisted_pages(self.docs), [])

    def test_a_link_with_an_anchor_counts(self):
        self.index("[A](a.md#top) [B](b.md) [C](nested/c.md)\n")
        self.assertEqual(unlisted_pages(self.docs), [])

    def test_a_link_inside_a_code_block_does_not_count(self):
        self.index("[A](a.md) [B](b.md)\n\n```md\n[C](nested/c.md)\n```\n")
        self.assertEqual(unlisted_pages(self.docs), ["nested/c.md"])

    def test_an_external_link_to_the_same_name_does_not_count(self):
        self.index("[A](a.md) [B](b.md) [C](https://example.com/nested/c.md)\n")
        self.assertEqual(unlisted_pages(self.docs), ["nested/c.md"])


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
