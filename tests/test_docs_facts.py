#!/usr/bin/env python3
"""tools/docs_facts.py and the date rule of tools/check_docs.py on a copy of the
tree: a fact derived from the code cannot be hand-edited or go stale, a new
route needs documenting, the allowlist only shrinks, and an opening date line
cannot be older than a dated section below it."""
import json
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO / "tools"))
import check_docs  # noqa: E402

KEEP = shutil.ignore_patterns("target", "node_modules", "__pycache__", ".git")


def copy_tree(root):
    for name in ("Cargo.toml", "vendor.lock.json", "vendor.patches.json", "README.md", "CONTRIBUTING.md"):
        shutil.copy(REPO / name, root / name)
    for name in ("docs", "tools", "patches"):
        shutil.copytree(REPO / name, root / name, ignore=KEEP)
    for crate in (REPO / "crates").iterdir():
        for part in ("Cargo.toml", "src", "tests"):
            source = crate / part
            target = root / "crates" / crate.name / part
            if source.is_dir():
                shutil.copytree(source, target, ignore=shutil.ignore_patterns("fixtures", "*.js"))
            elif source.exists():
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy(source, target)


class Tree(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        copy_tree(self.root)

    def tearDown(self):
        self.tmp.cleanup()

    def run_tool(self, *args):
        return subprocess.run([sys.executable, str(self.root / "tools" / args[0]), *args[1:]],
                              capture_output=True, text=True)

    def facts(self, *args):
        return self.run_tool("docs_facts.py", *args)

    def edit(self, rel, old, new):
        path = self.root / rel
        text = path.read_text()
        self.assertIn(old, text)
        path.write_text(text.replace(old, new, 1))


class DocsFacts(Tree):
    def test_the_repository_is_current(self):
        done = subprocess.run([sys.executable, str(REPO / "tools" / "docs_facts.py"), "--check"],
                              capture_output=True, text=True)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)

    def test_a_vendored_file_added_without_write_fails(self):
        lock = self.root / "vendor.lock.json"
        data = json.loads(lock.read_text())
        extra = dict(data["files"][0], path="vendor/scion/EXTRA.md")
        data["files"].append(extra)
        lock.write_text(json.dumps(data, indent=2) + "\n")
        done = self.facts("--check")
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("stale", done.stderr)
        self.assertEqual(self.facts("--write").returncode, 0)
        self.assertEqual(self.facts("--check").returncode, 0)
        validation = (self.root / "docs" / "validation.md").read_text()
        count = len(data["files"])
        self.assertIn(f"<!-- fact:vendor.files -->{count}<!-- /fact -->", validation)

    def test_a_hand_edited_fact_fails(self):
        self.edit("README.md", "<!-- fact:vendor.files -->", "<!-- fact:vendor.files -->1")
        done = self.facts("--check")
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("README.md", done.stderr)
        self.assertEqual(self.facts("--write").returncode, 0)
        self.assertEqual(self.facts("--check").returncode, 0)

    def test_an_unknown_region_fails(self):
        self.edit("README.md", "<!-- fact:vendor.files -->", "<!-- fact:no.such.fact -->")
        done = self.facts("--check")
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("unknown fact region", done.stderr)

    def test_a_marker_quoted_in_code_is_documentation_not_a_region(self):
        with (self.root / "CONTRIBUTING.md").open("a") as page:
            page.write("\nWrite `<!-- fact:made.up -->x<!-- /fact -->` around it.\n")
        self.assertEqual(self.facts("--check").returncode, 0)

    def test_a_changed_runtime_prefix_list_reaches_the_docs(self):
        self.edit("crates/branchyard-runtime/src/lib.rs", '"CODEX"]', '"CODEX", "GEMINI"]')
        self.edit("crates/branchyard-runtime/src/lib.rs", "[&str; 4]", "[&str; 5]")
        self.assertNotEqual(self.facts("--check").returncode, 0)
        self.assertEqual(self.facts("--write").returncode, 0)
        self.assertIn("`GEMINI*`", (self.root / "docs" / "provisioning.md").read_text())

    def test_derivative_counts_come_from_the_manifests(self):
        ports = json.loads((self.root / "patches" / "ports.json").read_text())
        validation = (self.root / "docs" / "validation.md").read_text()
        count = len(ports["derivatives"])
        self.assertIn(f"<!-- fact:derivatives.ports -->{count}<!-- /fact -->", validation)
        sources = sum(len(s) for d in ports["derivatives"] for s in d["from"].values())
        self.assertIn(f"<!-- fact:derivatives.ports_sources -->{sources}<!-- /fact -->", validation)
        ports["derivatives"].pop()
        (self.root / "patches" / "ports.json").write_text(json.dumps(ports, indent=2) + "\n")
        done = self.facts("--check")
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("docs/validation.md", done.stderr)
        self.assertEqual(self.facts("--write").returncode, 0)
        validation = (self.root / "docs" / "validation.md").read_text()
        self.assertIn(f"<!-- fact:derivatives.ports -->{count - 1}<!-- /fact -->", validation)

    def test_the_qualification_page_takes_the_prefix_list_from_the_runtime(self):
        self.edit("crates/branchyard-runtime/src/lib.rs", '"CODEX"]', '"CODEX", "GEMINI"]')
        self.edit("crates/branchyard-runtime/src/lib.rs", "[&str; 4]", "[&str; 5]")
        self.assertEqual(self.facts("--write").returncode, 0)
        self.assertIn("`GEMINI*`", (self.root / "docs" / "qualification" / "README.md").read_text())

    def test_the_test_count_is_not_called_what_cargo_runs(self):
        claim = re.compile(r"cargo test --workspace[^|\n]*?\bruns\b[^|\n]*fact:tests\.total")
        for path in (REPO / "docs").rglob("*.md"):
            self.assertIsNone(claim.search(path.read_text()), path)

    def test_a_new_route_must_be_documented(self):
        api = self.root / "crates/branchyard-server/src/api.rs"
        api.write_text(api.read_text() + '\nfn extra() { r.route("/v1/repos/{repo}/frobnicate", get(f)); }\n')
        self.assertEqual(self.facts("--write").returncode, 0)
        done = self.facts("--check")
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("/v1/repos/{repo}/frobnicate", done.stderr)
        with (self.root / "docs" / "server.md").open("a") as server:
            server.write("\n| `GET /v1/repos/{repo}/frobnicate` | Frobnicate | `Frob` |\n")
        self.assertEqual(self.facts("--check").returncode, 0)

    def test_the_allowlist_only_shrinks(self):
        with (self.root / "tools" / "docs_facts_allow.txt").open("a") as allow:
            allow.write("/v1/repos/{repo}/branches\n")
        done = self.facts("--check")
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("remove it", done.stderr)

    def test_an_allowed_undocumented_route_passes(self):
        api = self.root / "crates/branchyard-server/src/api.rs"
        api.write_text(api.read_text() + '\nfn extra() { r.route("/v1/frobnicate", get(f)); }\n')
        self.assertEqual(self.facts("--write").returncode, 0)
        self.assertNotEqual(self.facts("--check").returncode, 0)
        with (self.root / "tools" / "docs_facts_allow.txt").open("a") as allow:
            allow.write("/v1/frobnicate\n")
        self.assertEqual(self.facts("--check").returncode, 0)


class DateOrder(unittest.TestCase):
    def problems(self, text):
        return check_docs.date_problems(REPO / "docs" / "dated.md", text)

    def test_the_repository_dates_run_forwards(self):
        done = subprocess.run([sys.executable, str(REPO / "tools" / "check_docs.py")],
                              capture_output=True, text=True)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)

    def test_an_opening_date_older_than_a_status_paragraph_fails(self):
        found = self.problems("# Dated\n\nWritten 30 September 2026 from studies.\n\n"
                              "**Status, 3 October 2026.** Done.\n")
        self.assertEqual(len(found), 1)
        self.assertIn("docs/dated.md:5", found[0])

    def test_a_table_row_dated_after_the_opening_line_fails(self):
        self.assertEqual(len(self.problems(
            "# Dated\n\nPrepared 1 October 2026.\n\n| Check | Result |\n|---|---|\n"
            "| Sync | 3 October 2026, hermetic |\n")), 1)

    def test_an_updated_opening_line_passes(self):
        self.assertEqual(self.problems(
            "# Dated\n\nWritten 30 September 2026 and updated 3 October 2026.\n\n"
            "**Status, 3 October 2026.** Done.\n"), [])

    def test_a_page_without_an_opening_date_is_not_checked(self):
        self.assertEqual(self.problems("# Dated\n\nNothing here.\n\n**Status, 3 October 2026.** Done.\n"), [])


if __name__ == "__main__":
    unittest.main()
