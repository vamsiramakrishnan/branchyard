#!/usr/bin/env python3
"""The text a model is given to use `by` stays true to the code.

The delegate skill, the leaf note, the slash commands and the instructions files tell a model which
`by` commands, flags, result-schema keywords and MCP tools to use. Each is checked here against its
source: the result-schema keyword list in the skill equals the list the contract accepts (and the
one the CLI and the MCP tool print); every MCP tool the skill names is registered; and, when a built
`by` is at hand (`BY_BIN`, `$CARGO_TARGET_DIR/debug/by` or `target/debug/by`), every `by` command
and flag in these files exists in that binary's `--help`. Without a binary the last check is
skipped; `cargo test -p branchyard-cli --bin by operations` makes the same check against the clap
tree in CI.
"""

import os
import re
import shlex
import subprocess
import tempfile
import unittest
from functools import cache
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PLUGIN = ROOT / "plugins" / "branchyard"
SKILL = PLUGIN / "skills" / "delegate" / "SKILL.md"
JSON_SCHEMA = ROOT / "crates" / "branchyard" / "src" / "json_schema.rs"
MCP = ROOT / "crates" / "branchyard-mcp" / "src" / "lib.rs"
# The command line: `crates/branchyard-cli/src/args/`, one module per part.
ARGS = ROOT / "crates" / "branchyard-cli" / "src" / "args"

# Annotations that the keyword sentences use as examples, not as keywords.
ANNOTATION_EXAMPLES = {"title", "description"}
# What the skill refuses by name, as examples: none of these may be accepted.
REFUSED_EXAMPLES = {"$ref", "allOf", "format"}

AGENT_TEXT = [
    PLUGIN / "skills" / "delegate" / "SKILL.md",
    PLUGIN / "skills" / "setup" / "SKILL.md",
    PLUGIN / "leaf.md",
    *sorted((PLUGIN / "commands").glob("*.md")),
    PLUGIN / "harnesses" / "generic" / "AGENTS.md",
    ROOT / "AGENTS.md",
]


def rust_list(name):
    """The strings of `const NAME: &[&str] = &[...]` in json_schema.rs."""
    text = JSON_SCHEMA.read_text()
    body = re.search(rf"const {name}: &\[&str\] = &\[(.*?)\];", text, re.S)
    assert body, f"{name} not found in {JSON_SCHEMA}"
    return re.findall(r'"([^"]+)"', body.group(1))


def words(text):
    return [w.strip() for w in re.split(r"[,\s]+", text) if w.strip()]


def skill_keywords():
    """(keywords, type names) the delegate skill says a result schema may use."""
    text = SKILL.read_text()
    found = re.search(r"schema may use only(.*?)is refused by name", text, re.S)
    assert found, "the skill no longer says which keywords a result schema may use"
    allowed, refused = re.split(r";\s*any\s+other\s+keyword", found.group(1), maxsplit=1)
    named = re.findall(r"`([A-Za-z$]+)`", allowed)
    types = {w for w in named if w in rust_list("TYPES")}
    keywords = set(named) - types - ANNOTATION_EXAMPLES
    return keywords, types, set(re.findall(r"`([A-Za-z$]+)`", refused))


def mcp_keywords():
    text = MCP.read_text()
    found = re.search(r"A JSON Schema using only type \((.*?)\), (.*?), and annotations such as", text, re.S)
    assert found, "the MCP spawn tool no longer lists the result-schema keywords"
    return {"type", *words(found.group(2))}, set(words(found.group(1).replace(", or a list", "")))


def cli_keywords():
    text = "\n".join(path.read_text() for path in sorted(ARGS.glob("*.rs")))
    found = re.search(r"/// using only (.*?) and annotations such as", text, re.S)
    assert found, "`by spawn --result-schema` no longer lists the keywords"
    flat = re.sub(r"\s*///\s*", " ", found.group(1))
    return set(words(flat.replace(",", " ")))


class ResultSchemaKeywords(unittest.TestCase):
    def test_the_skill_lists_what_the_contract_accepts(self):
        accepted = set(rust_list("KEYWORDS"))
        keywords, types, refused = skill_keywords()
        self.assertEqual(sorted(keywords), sorted(accepted))
        self.assertEqual(sorted(types), sorted(rust_list("TYPES")))
        # The keywords it gives as examples of refused ones are refused.
        self.assertTrue(refused & REFUSED_EXAMPLES, refused)
        for word in refused & REFUSED_EXAMPLES:
            self.assertNotIn(word, accepted)
            self.assertNotIn(word, rust_list("ANNOTATIONS"))

    def test_the_mcp_tool_and_the_cli_help_list_the_same(self):
        accepted = set(rust_list("KEYWORDS"))
        mcp, mcp_types = mcp_keywords()
        self.assertEqual(sorted(mcp), sorted(accepted))
        self.assertEqual(sorted(mcp_types), sorted(rust_list("TYPES")))
        self.assertEqual(sorted(cli_keywords()), sorted(accepted))


class McpToolNames(unittest.TestCase):
    def test_every_tool_the_skill_names_is_registered(self):
        registered = set(re.findall(r'Tool::new\(\s*"([a-z_]+)"', MCP.read_text()))
        self.assertGreater(len(registered), 30)
        section = SKILL.read_text().split("## Without a shell", 1)[1]
        # Parameter names sit in parentheses; the rest of the backticked words are tools.
        section = re.sub(r"\([^)]*\)", "", section)
        named = set(re.findall(r"`([a-z_]+)`", section)) - {"branchyard"}
        self.assertIn("propose_integration", named)
        self.assertEqual(sorted(named - registered), [])


class RootAgentsFile(unittest.TestCase):
    """AGENTS.md says what to run and what to read; each must be there."""

    text = (ROOT / "AGENTS.md").read_text()

    def test_claude_md_imports_it(self):
        self.assertEqual((ROOT / "CLAUDE.md").read_text().strip(), "@AGENTS.md")
        self.assertLess(len(self.text.splitlines()), 120)

    def test_every_file_it_names_exists(self):
        named = [t for t in re.findall(r"`([^`\s]+)`", self.text) if re.fullmatch(r"[\w./-]+", t) and "/" in t]
        self.assertGreater(len(named), 15)
        # Build output is not in a checkout.
        missing = [t for t in named if not t.startswith("target/") and not (ROOT / t.rstrip("/")).exists()]
        self.assertEqual(missing, [])

    def test_every_tool_flag_it_names_is_one_the_tool_takes(self):
        found = 0
        for span in re.findall(r"`(python3 tools/[^`]+)`", self.text):
            words = span.split()
            script = ROOT / words[1]
            self.assertTrue(script.is_file(), span)
            source = script.read_text()
            for flag in (w for w in words[2:] if w.startswith("--")):
                found += 1
                self.assertIn(f'"{flag}"', source, span)
        self.assertGreater(found, 5)


def built_by():
    candidates = [os.environ.get("BY_BIN")]
    if os.environ.get("CARGO_TARGET_DIR"):
        candidates.append(str(Path(os.environ["CARGO_TARGET_DIR"]) / "debug" / "by"))
    candidates.append(str(ROOT / "target" / "debug" / "by"))
    for candidate in candidates:
        if candidate and os.access(candidate, os.X_OK):
            return candidate
    return None


@cache
def help_of(by, path):
    """`by PATH --help`'s text, or None when PATH is not a command of by."""
    env = {k: v for k, v in os.environ.items() if not k.startswith("BRANCHYARD_")}
    env["NO_COLOR"] = "1"
    with tempfile.TemporaryDirectory() as cwd:
        run = subprocess.run([by, *path, "--help"], capture_output=True, text=True, cwd=cwd, env=env, timeout=60)
    if run.returncode != 0:
        return None
    # A positional argument is not a command: the usage line names only real subcommands.
    usage = next((line for line in run.stdout.splitlines() if line.startswith("Usage:")), "")
    if not usage.startswith("Usage: " + " ".join(("by", *path))):
        return None
    return run.stdout


def spans(text):
    """The code in `text`: fenced lines (continuations joined) and inline backticks."""
    found, fenced, pending = [], False, ""
    for line in text.splitlines():
        if line.lstrip().startswith("```"):
            fenced = not fenced
            continue
        if fenced:
            pending += line.rstrip("\\ ").strip() + " " if line.rstrip().endswith("\\") else line.strip()
            if not line.rstrip().endswith("\\"):
                found.append(pending)
                pending = ""
        elif not line.lstrip().startswith("|"):
            found.extend(line.split("`")[1::2])
    return found


def invocation(span):
    """(subcommand words, flags) when `span` runs `by`, else None."""
    try:
        parts = shlex.split(span, comments=True)
    except ValueError:
        parts = span.split()
    if "by" not in parts:
        return None
    rest = parts[parts.index("by") + 1 :]
    words = []
    for part in rest:
        if re.fullmatch(r"[a-z][a-z-]*", part):
            words.append(part)
        else:
            break
    if not words:
        return None
    flags = [p.split("=")[0] for p in rest if re.fullmatch(r"--[a-z][a-z-]*(=.*)?", p)]
    return words, flags


class CommandsExist(unittest.TestCase):
    def test_every_command_and_flag_the_agent_text_names_is_in_help(self):
        by = built_by()
        if by is None:
            self.skipTest("no built `by` (set BY_BIN); cargo test -p branchyard-cli covers this in CI")
        problems, checked = [], 0
        for path in AGENT_TEXT:
            for span in spans(path.read_text()):
                found = invocation(span)
                if found is None:
                    continue
                words, flags = found
                # Walk down to the deepest real subcommand; the rest are arguments.
                command = ()
                for word in words:
                    if help_of(by, (*command, word)) is None:
                        break
                    command = (*command, word)
                name = path.relative_to(ROOT)
                if not command:
                    if span.startswith("by "):
                        problems.append(f"{name}: `{span}` is not a by command")
                    continue
                if command[0] in {"serve", "worker"}:
                    continue
                checked += 1
                text = help_of(by, command)
                for flag in flags:
                    if not re.search(rf"(?<![\w-]){re.escape(flag)}(?![\w-])", text or ""):
                        problems.append(f"{name}: `by {' '.join(command)}` has no {flag}")
        self.assertGreater(checked, 40)
        self.assertEqual(problems, [])


class Extraction(unittest.TestCase):
    """The checks above find nothing if the extraction does, so it is checked on its own."""

    def test_an_inline_span_and_a_continued_fenced_command_are_found(self):
        text = "Run `by spawn x --wait` now.\n\n```sh\nby delegate \\\n  --plan p.json --land\n```\n"
        found = [invocation(span) for span in spans(text)]
        self.assertEqual(found, [(["spawn", "x"], ["--wait"]), (["delegate"], ["--plan", "--land"])])

    def test_quoted_text_is_not_flags_and_a_comment_is_dropped(self):
        found = invocation('by spawn "mention --nothing here" --name a  # not --this')
        self.assertEqual(found, (["spawn"], ["--name"]))

    def test_prose_without_by_is_not_an_invocation(self):
        self.assertIsNone(invocation("cargo test --locked"))
        self.assertIsNone(invocation("by"))


if __name__ == "__main__":
    unittest.main()
