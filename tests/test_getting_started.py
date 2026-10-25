#!/usr/bin/env python3
"""The new guides, kept honest: docs/getting-started.md runs, and every `by` command the four guides
(getting-started, concepts, troubleshooting, upgrade) show exists with the flags shown.

* Each fenced block marked ```` ```sh test ```` in docs/getting-started.md runs, in order and in one
  shell, in a temporary directory with a private HOME, `by` and the fake ACP agent (`$FAKE`) on hand and
  no other `by` configuration. A block that fails fails the test. The ```` ```text ```` block right after
  it must be what the block printed: every line of it appears in the output, in order, with spaces
  collapsed, and `…` stands for any text (a hash, a time, a path); a line that is only `…` is skipped.
  Output the page leaves out does not matter, so only the lines the page teaches are pinned.
* Every `by` command in a ```` ```sh ```` block or a `by …` code span: its subcommand exists in the
  built `by`, and each `--flag` it shows is in that subcommand's `--help`. `by setup` is skipped while the
  built `by` lacks it.
* The symptoms and messages troubleshooting.md and upgrade.md quote are in the code and on the page.

The tests that run a program need a built `by` and `fake-acp-agent`, so they run only when asked, and
a stale build in `target/` cannot fail a push by surprise: set `BY_BIN` and `FAKE_ACP_AGENT` to the two
programs, or set `REQUIRE_BINARIES=1` to use Cargo's target directory (`CARGO_TARGET_DIR`, or what
`cargo metadata` reports) and fail when they are not there. `python3 tools/check.py` builds them and
sets it (the `test_getting_started_built` step); without either, those tests are skipped with the reason.
The rest (the matcher, the messages, the build commands) always runs.

Run: `python3 tests/test_getting_started.py`.
"""

import json
import os
import re
import shlex
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from typing import ClassVar, NamedTuple

ROOT = Path(__file__).resolve().parents[1]
DOCS = ROOT / "docs"
GUIDES = ("getting-started.md", "concepts.md", "troubleshooting.md", "upgrade.md")
GETTING_STARTED = DOCS / "getting-started.md"
RUN_TIMEOUT = 120
GLOBAL_WITH_VALUE = {"--remote", "--token-file", "--repo", "--ca-file"}
SEPARATORS = {";", "&&", "||", "|", "&", "(", ")"}


class Fence(NamedTuple):
    """A fenced block: its info string, its body, and the 1-based lines it opens and closes on."""

    info: str
    body: str
    first: int
    last: int


def fences(text):
    """Every ``` block of `text`, in order."""
    lines = text.splitlines()
    found, i = [], 0
    while i < len(lines):
        opening = re.match(r"^```(.*)$", lines[i])
        if not opening:
            i += 1
            continue
        j = i + 1
        while j < len(lines) and not lines[j].startswith("```"):
            j += 1
        found.append(Fence(opening.group(1).strip(), "\n".join(lines[i + 1 : j]), i + 1, j + 1))
        i = j + 1
    return found


def tested_blocks(text):
    """(command block, expected output or None) for each ```sh test block: the output is the ```text block
    that follows with only blank lines between."""
    blocks, lines = fences(text), text.splitlines()
    pairs = []
    for index, block in enumerate(blocks):
        if block.info != "sh test":
            continue
        following = blocks[index + 1] if index + 1 < len(blocks) else None
        expected = None
        if following and following.info == "text":
            between = lines[block.last : following.first - 1]
            if not any(line.strip() for line in between):
                expected = following.body
        pairs.append((block, expected))
    return pairs


def squash(line):
    return " ".join(line.split())


def line_pattern(expected):
    """A regular expression for one expected line: spaces collapsed, `…` for any text."""
    return re.compile(".*".join(re.escape(part) for part in squash(expected).split("…")))


def first_missing(expected, output):
    """The first expected line that does not appear, in order, in `output`; None when all do."""
    actual = [squash(line) for line in output.splitlines()]
    position = 0
    for line in expected.splitlines():
        if not squash(line) or squash(line) == "…":
            continue
        pattern = line_pattern(line)
        while position < len(actual) and not pattern.fullmatch(actual[position]):
            position += 1
        if position == len(actual):
            return line
        position += 1
    return None


def target_dir():
    given = os.environ.get("CARGO_TARGET_DIR")
    if given:
        return Path(given)
    try:
        done = subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--no-deps", "--offline"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=60,
        )
        if done.returncode == 0:
            return Path(json.loads(done.stdout)["target_directory"])
    except (OSError, subprocess.SubprocessError, ValueError, KeyError):
        pass
    return ROOT / "target"


def binary(name, variable):
    """The built program `name`: from the environment variable if set, else Cargo's target directory."""
    given = os.environ.get(variable)
    if given:
        return Path(given) if Path(given).is_file() else None
    for profile in ("debug", "release"):
        candidate = target_dir() / profile / name
        if candidate.is_file():
            return candidate
    return None


def need_binaries():
    """(by, fake agent). Skips unless asked for (BY_BIN and FAKE_ACP_AGENT, or REQUIRE_BINARIES=1); with
    REQUIRE_BINARIES=1, programs that are not built fail instead."""
    strict = os.environ.get("REQUIRE_BINARIES") == "1"
    explicit = bool(os.environ.get("BY_BIN") and os.environ.get("FAKE_ACP_AGENT"))
    if not (strict or explicit):
        raise unittest.SkipTest("set REQUIRE_BINARIES=1 (after cargo build), or BY_BIN and FAKE_ACP_AGENT")
    by, fake = binary("by", "BY_BIN"), binary("fake-acp-agent", "FAKE_ACP_AGENT")
    if by and fake:
        return by, fake
    raise AssertionError("build `by` and `fake-acp-agent` first: cargo build -p branchyard-cli -p branchyard-runtime")


def run_script(commands, by, fake, base):
    """Run `commands` (a list of shell blocks) in one `sh -e` in a fresh directory under `base`, with a
    private HOME. Returns (output per block that finished, the combined output)."""
    home, bin_dir, start = base / "home", base / "bin", base / "start"
    for directory in (home, bin_dir, start):
        directory.mkdir()
    (bin_dir / "by").symlink_to(by)
    script = ["set -e"]
    for index, command in enumerate(commands):
        script += [f"echo '@@BEGIN {index}'", command, f"echo '@@END {index}'"]
    env = {
        "PATH": f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '/usr/bin:/bin')}",
        "HOME": str(home),
        "XDG_CONFIG_HOME": str(home / ".config"),
        "XDG_DATA_HOME": str(home / ".local" / "share"),
        "XDG_CACHE_HOME": str(home / ".cache"),
        "TMPDIR": str(base),
        "FAKE": str(fake),
        "GIT_AUTHOR_NAME": "Doc Test",
        "GIT_AUTHOR_EMAIL": "doc-test@example.com",
        "GIT_COMMITTER_NAME": "Doc Test",
        "GIT_COMMITTER_EMAIL": "doc-test@example.com",
        "GIT_CONFIG_NOSYSTEM": "1",
        "LC_ALL": "C",
        "NO_COLOR": "1",
        "TERM": "dumb",
    }
    done = subprocess.run(
        ["sh", "-c", "\n".join(script)],
        cwd=start,
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        timeout=RUN_TIMEOUT,
    )
    outputs = {}
    for match in re.finditer(r"^@@BEGIN (\d+)\n(.*?)^@@END \1$", done.stdout, re.S | re.M):
        outputs[int(match.group(1))] = match.group(2)
    return outputs, done.stdout


class Format(unittest.TestCase):
    """The parser and the matcher, with no binary."""

    def test_a_test_block_and_the_output_after_it_pair_up(self):
        text = "x\n\n```sh test\nby ls\n```\n\n```text\nBRANCH\n```\n\n```sh\nnot run\n```\n"
        pairs = tested_blocks(text)
        self.assertEqual(len(pairs), 1)
        self.assertEqual(pairs[0][0].body, "by ls")
        self.assertEqual(pairs[0][1], "BRANCH")

    def test_output_separated_by_prose_does_not_pair(self):
        text = "```sh test\nby ls\n```\n\nand then\n\n```text\nBRANCH\n```\n"
        self.assertIsNone(tested_blocks(text)[0][1])

    def test_lines_match_in_order_with_spaces_collapsed_and_wildcards(self):
        output = "noise\nbranch      hello (by/hello)\nstatus  ready\n2026-10-25T00:22:46Z  turn 1 completed\ntail\n"
        self.assertIsNone(first_missing("branch hello (by/hello)\nstatus ready\n…Z turn 1 completed", output))
        self.assertIsNone(first_missing("…\nstatus ready\n…", output))

    def test_a_missing_or_out_of_order_line_is_reported(self):
        output = "a\nb\nc\n"
        self.assertEqual(first_missing("b\na", output), "a")
        self.assertEqual(first_missing("a\nd", output), "d")

    def test_a_wildcard_does_not_cross_lines(self):
        self.assertEqual(first_missing("a … c", "a b\nc\n"), "a … c")

    def test_the_first_line_of_a_block_is_the_commands(self):
        self.assertEqual(fences("```sh test\nx\ny\n```\n")[0], Fence("sh test", "x\ny", 1, 4))


class Walkthrough(unittest.TestCase):
    def test_getting_started_has_the_tested_blocks_it_promises(self):
        pairs = tested_blocks(GETTING_STARTED.read_text())
        commands = "\n".join(block.body for block, _ in pairs)
        for step in ("by run ", "by ls", "by show hello", "by diff hello", "by merge hello", "by rm hello", "by fan "):
            self.assertIn(step, commands)
        for block, expected in pairs:
            self.assertIsNotNone(expected, f"docs/getting-started.md:{block.first}: add the output after this block")

    def test_every_tested_block_runs_and_prints_what_the_page_shows(self):
        by, fake = need_binaries()
        pairs = tested_blocks(GETTING_STARTED.read_text())
        with tempfile.TemporaryDirectory(prefix="getting-started-") as base:
            outputs, everything = run_script([block.body for block, _ in pairs], by, fake, Path(base))
            for index, (block, expected) in enumerate(pairs):
                where = f"docs/getting-started.md:{block.first}"
                self.assertIn(
                    index, outputs, f"{where}: the block failed.\n{block.body}\n--- output so far ---\n{everything}"
                )
                if expected is not None:
                    missing = first_missing(expected, outputs[index])
                    self.assertIsNone(
                        missing,
                        f"{where}: the page shows a line the command did not print: {missing!r}"
                        f"\n--- printed ---\n{outputs[index]}",
                    )


def by_commands(text):
    """(line number, tokens after `by`) for each `by` command in the sh blocks and each `by …` code span."""
    found = []
    for block in fences(text):
        if not block.info.startswith("sh"):
            continue
        for number, line in enumerate(block.body.splitlines(), block.first + 1):
            found += [(number, argv) for argv in commands_in(line)]
    prose = re.sub(r"(?ms)^```.*?^```[^\n]*$", lambda m: "\n" * m.group(0).count("\n"), text)
    for number, line in enumerate(prose.splitlines(), 1):
        for span in re.findall(r"`([^`]+)`", line):
            if span.startswith("by ") and not re.match(r"by \d", span):  # `by 0.0.1` is output, not a command
                found += [(number, argv) for argv in commands_in(span)]
    return found


def commands_in(line):
    """The argument lists of each `by` command in one shell line."""
    lexer = shlex.shlex(line, posix=True, punctuation_chars=True)
    lexer.whitespace_split = True
    lexer.commenters = "#"
    try:
        tokens = list(lexer)
    except ValueError:
        return []
    commands, current = [], []
    for token in [*tokens, ";"]:
        if token in SEPARATORS:
            if current[:1] == ["by"]:
                commands.append(current[1:])
            current = []
        else:
            current.append(token)
    return commands


def split_global(argv):
    """(global flags, subcommand, the rest) of the arguments after `by`."""
    flags, i = [], 0
    while i < len(argv) and argv[i].startswith("-"):
        flags.append(argv[i].split("=")[0])
        i += 2 if argv[i] in GLOBAL_WITH_VALUE else 1
    return flags, (argv[i] if i < len(argv) else None), argv[i + 1 :]


class CommandsExist(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.by, _ = need_binaries()
        cls.cache = {}
        cls.top = cls.help_text([])

    @classmethod
    def help_text(cls, path):
        if tuple(path) not in cls.cache:
            done = subprocess.run(
                [str(cls.by), *path, "--help"], capture_output=True, text=True, timeout=60, stdin=subprocess.DEVNULL
            )
            cls.cache[tuple(path)] = done.stdout if done.returncode == 0 else None
        return cls.cache[tuple(path)]

    @staticmethod
    def subcommands(help_text):
        """The command names a `--help` lists: two spaces, a lower-case word, two or more spaces."""
        return re.findall(r"^  ([a-z][a-z0-9-]*)\s{2,}\S", help_text or "", re.M)

    def test_every_command_shown_exists_with_the_flags_shown(self):
        problems, checked, skipped = [], 0, set()
        top_commands = self.subcommands(self.top)
        for page in GUIDES:
            for number, argv in by_commands((DOCS / page).read_text()):
                flags, sub, rest = split_global(argv)
                where = f"docs/{page}:{number}: by {' '.join(argv)}"
                if sub is None:
                    if not set(flags) <= {"--version", "--help", "-V", "-h"} and not all(f in self.top for f in flags):
                        problems.append(f"{where}: no such option")
                    continue
                if sub == "setup" and "setup" not in top_commands:
                    skipped.add("setup")
                    continue
                if sub not in top_commands:
                    problems.append(f"{where}: `by {sub}` is not a command")
                    continue
                path = [sub]
                text = self.help_text(path) or ""
                nested = rest[0] if rest and rest[0] in self.subcommands(text) else None
                if nested:
                    path.append(nested)
                    text += self.help_text(path) or ""
                shown = flags + [m.group(1) for token in rest if (m := re.match(r"\[?(--[\w-]+)", token))]
                for name in shown:
                    name = name.split("=")[0]
                    if not re.search(rf"(?<![\w-]){re.escape(name)}(?![\w-])", text + self.top):
                        problems.append(f"{where}: {' '.join(path)} has no {name}")
                checked += 1
        self.assertEqual(problems, [])
        self.assertGreater(checked, 40, "the guides show fewer `by` commands than expected; did the parser break?")


class Messages(unittest.TestCase):
    """What the guides quote from `by` is in the code, and on the page."""

    FRAGMENTS: ClassVar[dict[str, list[str]]] = {
        "troubleshooting.md": [
            "has scripts you have not trusted",
            "and there is no terminal to ask on",
            "changed since you trusted it",
            "check timed out after",
            "cancelled by",
            "the prompt had been submitted and the turn's outcome is unknown",
            "the harness connection closed before the turn ended",
            "its version command timed out",
            "remote mode needs a token: pass --token-file or set BRANCHYARD_TOKEN_FILE",
            "or fix the URL, token or CA file",
            "this build has no PostgreSQL support",
            "this build has no Microsandbox support",
            "likely logged out",
        ],
        "upgrade.md": [
            "if you may go back to an older by, then run any",
            "was written by a newer by; this binary reads",
            "and upgrades the store in",
            "place the first time any `by` command opens it, after which an older `by`",
        ],
    }

    @classmethod
    def setUpClass(cls):
        cls.source = "\n".join(
            path.read_text(errors="replace")
            for path in sorted((ROOT / "crates").rglob("*.rs"))
            if "target" not in path.parts
        )

    def test_each_quoted_fragment_is_in_the_code_and_on_its_page(self):
        problems = []
        for page, fragments in self.FRAGMENTS.items():
            text = (DOCS / page).read_text()
            for fragment in fragments:
                if fragment not in text:
                    problems.append(f"docs/{page} does not quote {fragment!r}")
                if fragment not in self.source:
                    problems.append(f"{fragment!r} (quoted in docs/{page}) is not in crates/**/*.rs any more")
        self.assertEqual(problems, [])

    def test_each_symptom_heading_is_a_word_by_prints(self):
        problems = []
        for number, line in enumerate((DOCS / "troubleshooting.md").read_text().splitlines(), 1):
            if not line.startswith("### `"):
                continue
            for span in re.findall(r"`([^`]+)`", line):
                for part in re.split(r"…|<[^>]*>", span):
                    if len(part.strip()) >= 4 and part.strip() not in self.source:
                        problems.append(f"docs/troubleshooting.md:{number}: {part.strip()!r} is not in the code")
        self.assertEqual(problems, [])


class Sources(unittest.TestCase):
    """The build commands the pages show name real packages, binaries and paths."""

    def test_the_install_and_fake_agent_commands_name_real_targets(self):
        text = GETTING_STARTED.read_text()
        self.assertIn("cargo install --locked --path crates/branchyard-cli", text)
        manifest = (ROOT / "crates/branchyard-cli/Cargo.toml").read_text()
        self.assertRegex(manifest, r'(?m)^\[\[bin\]\]\nname = "by"')
        self.assertIn("cargo build -p branchyard-runtime --bin fake-acp-agent", text)
        self.assertTrue((ROOT / "crates/branchyard-runtime/src/bin/fake-acp-agent.rs").is_file())
        self.assertIn('name = "branchyard-runtime"', (ROOT / "crates/branchyard-runtime/Cargo.toml").read_text())

    def test_the_toolchain_the_page_names_is_the_pinned_one(self):
        pinned = re.search(r'channel = "([\d.]+)"', (ROOT / "rust-toolchain.toml").read_text())
        self.assertIsNotNone(pinned)
        assert pinned is not None
        self.assertIn(f"toolchain {pinned.group(1)}", GETTING_STARTED.read_text())


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
