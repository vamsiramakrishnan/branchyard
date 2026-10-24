"""The packaging for the harnesses that take Claude-style hook files or a
plugin directory: Codex, Gemini CLI, Qwen Code, GitHub Copilot CLI, Droid and
DeepSeek Harness (plugins/branchyard/harnesses/<id>/, install/<id>.toml, the
manifests and marketplaces). Every file parses; manifests carry their
required fields; every hook and status-line command is silent with no `by` on
PATH and, against a stub `by` that records its arguments, passes the right
`--harness` and event. See docs/harness-hooks.md.

Nothing here runs a real harness: the formats were written from each
harness's documentation."""

import json
import re
import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PLUGIN = ROOT / "plugins" / "branchyard"
HARNESSES = PLUGIN / "harnesses"
INSTALL = PLUGIN / "install"
WORKSPACE_VERSION = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]

# A hook command must be exactly this, so a missing `by` is silent.
GUARD = re.compile(
    r"^command -v by >/dev/null 2>&1 && by (hook --harness [\w-]+ [\w-]+|statusline --harness [\w-]+) \|\| true$"
)

# Each hook file: (harness id, the event map's place in the file, native event -> canonical event,
# timeout key, the longest timeout allowed in that file's unit).
HOOK_FILES = {
    "codex/hooks.json": (
        "codex",
        ("hooks",),
        {"SessionStart": "session-start", "UserPromptSubmit": "user-prompt-submit"},
        "timeout",
        10,
    ),
    "gemini-cli/hooks/hooks.json": (
        "gemini-cli",
        ("hooks",),
        {"SessionStart": "session-start", "BeforeAgent": "user-prompt-submit"},
        "timeout",
        10_000,
    ),
    "qwen-code/settings.json": (
        "qwen-code",
        ("hooks",),
        {"SessionStart": "session-start", "UserPromptSubmit": "user-prompt-submit"},
        "timeout",
        10,
    ),
    "github-copilot/hooks.json": ("github-copilot", ("hooks",), {"sessionStart": "session-start"}, "timeoutSec", 10),
    "droid/hooks.json": (
        "droid",
        (),
        {"SessionStart": "session-start", "UserPromptSubmit": "user-prompt-submit"},
        "timeout",
        10,
    ),
    "deepseek-harness/hooks.json": (
        "deepseek-harness",
        ("hooks",),
        {"SessionStart": "session-start", "UserPromptSubmit": "user-prompt-submit"},
        "timeout",
        10,
    ),
}
# Each status-line settings fragment: (harness id, the path to the statusLine object).
STATUS_FILES = {
    "qwen-code/settings.json": ("qwen-code", ("ui", "statusLine")),
    "github-copilot/settings.json": ("github-copilot", ("statusLine",)),
    "droid/settings.json": ("droid", ("statusLine",)),
}
# What each harness's install manifest must provide at least, and its binary.
INSTALLS = {
    "codex": (["codex"], {"hooks"}),
    "gemini-cli": (["gemini"], {"hooks", "commands"}),
    "qwen-code": (["qwen"], {"hooks", "statusline"}),
    "github-copilot": (["copilot"], {"hooks", "statusline"}),
    "droid": (["droid"], {"hooks", "statusline"}),
    "deepseek-harness": (["dsh"], {"hooks"}),
}
HARNESS_NAMES = {
    "codex": "Codex",
    "gemini-cli": "Gemini CLI",
    "qwen-code": "Qwen Code",
    "github-copilot": "GitHub Copilot CLI",
    "droid": "Droid",
    "deepseek-harness": "DeepSeek Harness",
}
# A hook payload as each harness documents it, for the stub runs.
PAYLOADS = {
    "github-copilot": {"sessionId": "s-1", "timestamp": 0, "cwd": "/tmp", "source": "startup"},
}
# Where a Gemini command's prompt departs from the Markdown command's after its
# first paragraph: Gemini CLI has no AskUserQuestion tool.
GEMINI_WORDING = {"setup": [("every batch with\nAskUserQuestion,", "every batch of\nquestions yourself,")]}
DEFAULT_PAYLOAD = {"session_id": "s-1", "cwd": "/tmp", "hook_event_name": "SessionStart"}


def load_yaml(text):
    """The YAML, with DeepSeek Harness's `!!js` expressions kept as strings,
    or None when PyYAML is not installed."""
    try:
        import yaml
    except ImportError:
        return None

    class Loader(yaml.SafeLoader):
        pass

    Loader.add_constructor("tag:yaml.org,2002:js", lambda loader, node: {"__js": loader.construct_scalar(node)})
    return yaml.load(text, Loader=Loader)


def dig(value, path):
    for key in path:
        value = value[key]
    return value


def commands(value):
    """Every command string in a parsed hook or settings file."""
    if isinstance(value, dict):
        for key, item in value.items():
            if key in ("command", "bash") and isinstance(item, str):
                yield item
            else:
                yield from commands(item)
    elif isinstance(value, list):
        for item in value:
            yield from commands(item)


def frontmatter_description(markdown):
    match = re.search(r"^description: (.*)$", markdown, re.MULTILINE)
    assert match is not None
    return match.group(1)


class Stub:
    """A directory holding a `by` that records its arguments and stdin."""

    def __init__(self, root):
        self.dir = Path(root) / "bin"
        self.dir.mkdir()
        self.args = Path(root) / "args"
        self.stdin = Path(root) / "stdin"
        script = self.dir / "by"
        script.write_text(f'#!/bin/sh\nprintf "%s\\n" "$@" > "{self.args}"\ncat > "{self.stdin}"\n')
        script.chmod(0o755)

    def run(self, command, payload):
        for path in (self.args, self.stdin):
            path.unlink(missing_ok=True)
        result = subprocess.run(
            ["/bin/sh", "-c", command],
            input=json.dumps(payload),
            capture_output=True,
            text=True,
            env={"PATH": f"{self.dir}:/usr/bin:/bin"},
            timeout=10,
        )
        args = self.args.read_text().split("\n")[:-1] if self.args.exists() else None
        return result, args


def run_without_by(command, cwd):
    """`command` with an empty PATH (which searches only `cwd`, an empty directory)."""
    return subprocess.run(
        ["/bin/sh", "-c", command],
        input=json.dumps(DEFAULT_PAYLOAD),
        capture_output=True,
        text=True,
        env={"PATH": ""},
        cwd=cwd,
        timeout=10,
    )


class HookFileTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.stub = Stub(self.temp.name)
        self.empty = Path(self.temp.name) / "empty"
        self.empty.mkdir()

    def test_each_hook_maps_its_native_event_to_the_canonical_one(self):
        for name, (harness, place, events, timeout_key, longest) in HOOK_FILES.items():
            with self.subTest(file=name):
                events_map = dig(json.loads((HARNESSES / name).read_text()), place)
                self.assertEqual(set(events_map), set(events))
                for native, canonical in events.items():
                    groups = events_map[native]
                    if harness == "github-copilot":
                        handlers = groups
                    else:
                        self.assertEqual(len(groups), 1)
                        handlers = groups[0]["hooks"]
                    self.assertEqual(len(handlers), 1)
                    handler = handlers[0]
                    self.assertEqual(handler["type"], "command")
                    self.assertLessEqual(handler[timeout_key], longest)
                    command = handler["bash"] if harness == "github-copilot" else handler["command"]
                    self.assertRegex(command, GUARD)
                    result, args = self.stub.run(command, PAYLOADS.get(harness, DEFAULT_PAYLOAD))
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout, "")
                    self.assertEqual(args, ["hook", "--harness", harness, canonical])
                    self.assertEqual(json.loads(self.stub.stdin.read_text()), PAYLOADS.get(harness, DEFAULT_PAYLOAD))

    def test_each_status_line_names_its_harness(self):
        for name, (harness, place) in STATUS_FILES.items():
            with self.subTest(file=name):
                status = dig(json.loads((HARNESSES / name).read_text()), place)
                if harness != "droid":
                    self.assertEqual(status["type"], "command")
                self.assertRegex(status["command"], GUARD)
                result, args = self.stub.run(status["command"], {"cwd": "/tmp"})
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(args, ["statusline", "--harness", harness])

    def test_every_packaged_command_is_silent_without_by(self):
        found = 0
        for path in sorted(HARNESSES.rglob("*.json")):
            if path.parent.name == "harnesses" or path.relative_to(HARNESSES).parts[0] not in INSTALLS:
                continue
            for command in commands(json.loads(path.read_text())):
                found += 1
                with self.subTest(file=str(path.relative_to(HARNESSES)), command=command):
                    self.assertRegex(command, GUARD)
                    result = run_without_by(command, self.empty)
                    self.assertEqual((result.returncode, result.stdout, result.stderr), (0, "", ""))
        # Two hooks each for five harnesses, one for Copilot, three status lines.
        self.assertEqual(found, 14)


class ManifestTests(unittest.TestCase):
    def test_the_codex_manifest_names_its_own_hooks_file(self):
        manifest = json.loads((PLUGIN / ".codex-plugin/plugin.json").read_text())
        self.assertEqual((manifest["name"], manifest["version"]), ("branchyard", WORKSPACE_VERSION))
        # An explicit `hooks` replaces Codex's default hooks/hooks.json, which is Claude Code's.
        # Codex 0.164 honours it: its trust key names harnesses/codex/hooks.json.
        self.assertEqual(manifest["hooks"], "./harnesses/codex/hooks.json")
        target = (PLUGIN / manifest["hooks"]).resolve()
        self.assertTrue(target.is_file())
        self.assertTrue(target.is_relative_to(PLUGIN.resolve()))

    def test_the_codex_install_says_how_hooks_get_trusted(self):
        # `codex exec` skips an untrusted hook without a word (Codex 0.164), so the
        # note must name both the interactive trust and the unattended flag.
        import tomllib

        notes = [n["text"] for n in tomllib.loads((PLUGIN / "install/codex.toml").read_text())["note"]]
        text = " ".join(notes)
        self.assertIn("Trust all and continue", text)
        self.assertIn("codex exec --dangerously-bypass-hook-trust", text)

    def test_the_codex_marketplace_lists_the_plugin(self):
        marketplace = json.loads((ROOT / ".agents/plugins/marketplace.json").read_text())
        self.assertEqual(marketplace["name"], "branchyard")
        [entry] = marketplace["plugins"]
        self.assertEqual(entry["name"], "branchyard")
        self.assertEqual(entry["source"], {"source": "local", "path": "./plugins/branchyard"})
        self.assertIn(entry["policy"]["installation"], {"AVAILABLE", "INSTALLED_BY_DEFAULT", "NOT_AVAILABLE"})
        self.assertEqual(entry["policy"]["authentication"], "ON_INSTALL")
        self.assertTrue(entry["category"])

    def test_the_copilot_manifest_names_its_own_hooks_file(self):
        manifest = json.loads((PLUGIN / ".github/plugin/plugin.json").read_text())
        self.assertEqual((manifest["name"], manifest["version"]), ("branchyard", WORKSPACE_VERSION))
        self.assertRegex(manifest["name"], r"^[a-z0-9-]{1,64}$")
        self.assertLessEqual(len(manifest["description"]), 1024)
        self.assertEqual(manifest["hooks"], "harnesses/github-copilot/hooks.json")
        for field in ("skills", "commands", "hooks"):
            self.assertTrue((PLUGIN / manifest[field]).exists(), field)
        claude = json.loads((PLUGIN / ".claude-plugin/plugin.json").read_text())
        self.assertEqual(manifest["description"], claude["description"])

    def test_the_gemini_extension_and_its_commands_mirror_the_plugin(self):
        root = HARNESSES / "gemini-cli"
        extension = json.loads((root / "gemini-extension.json").read_text())
        self.assertEqual((extension["name"], extension["version"]), ("branchyard", WORKSPACE_VERSION))
        self.assertRegex(extension["name"], r"^[a-z0-9-]+$")
        self.assertTrue(extension["description"])
        markdown = sorted(path.stem for path in (PLUGIN / "commands").glob("*.md"))
        toml = sorted(path.stem for path in (root / "commands").glob("*.toml"))
        self.assertEqual(toml, markdown)
        for name in markdown:
            with self.subTest(command=name):
                command = tomllib.loads((root / "commands" / f"{name}.toml").read_text())
                self.assertEqual(set(command), {"description", "prompt"})
                source = (PLUGIN / "commands" / f"{name}.md").read_text()
                self.assertEqual(command["description"], frontmatter_description(source).strip('"'))
                self.assertIn("{{args}}", command["prompt"])
                self.assertNotIn("$ARGUMENTS", command["prompt"])
                self.assertNotIn("AskUserQuestion", command["prompt"])
                # The steps after the first paragraph are the Markdown command's, word for word.
                body = source.split("---", 2)[2].strip()
                for claude_words, gemini_words in GEMINI_WORDING.get(name, []):
                    body = body.replace(claude_words, gemini_words)
                self.assertEqual(command["prompt"].strip().split("\n\n")[1:], body.split("\n\n")[1:])

    def test_the_deepseek_patch_mounts_the_bridge_on_the_installed_hooks(self):
        text = (HARNESSES / "deepseek-harness/cordis.patch.yml").read_text()
        self.assertIn("name: '@deepseek-ai/dsh-hooks-claude-code'", text)
        self.assertIn("'.dsh', 'branchyard-hooks.json'", text)
        install = tomllib.loads((INSTALL / "deepseek-harness.toml").read_text())
        hooks = next(f for f in install["file"] if f["from"] == "harnesses/deepseek-harness/hooks.json")
        self.assertEqual(hooks["to"], "~/.dsh/branchyard-hooks.json")
        patch = load_yaml(text)
        if patch is None:
            return
        [operation] = patch
        [row] = operation["insert"]
        self.assertEqual(row["id"], "branchyard-hooks")
        self.assertEqual(row["name"], "@deepseek-ai/dsh-hooks-claude-code")
        self.assertIn("homedir()", row["config"]["configPath"]["__js"])

    def test_the_package_checks_every_host_manifest_version(self):
        sys.path.insert(0, str(ROOT / "tools"))
        try:
            import package
        finally:
            sys.path.pop(0)
        for manifest in (".github/plugin/plugin.json", "harnesses/gemini-cli/gemini-extension.json"):
            self.assertIn(manifest, package.HOST_MANIFESTS)


class InstallManifestTests(unittest.TestCase):
    def test_each_install_manifest_is_complete(self):
        for harness, (binaries, provides) in INSTALLS.items():
            with self.subTest(harness=harness):
                manifest = tomllib.loads((INSTALL / f"{harness}.toml").read_text())
                self.assertEqual(set(manifest), {"harness", "binaries", "provides", "file", "note"})
                self.assertEqual(manifest["harness"], harness)
                self.assertEqual(manifest["binaries"], binaries)
                self.assertTrue(provides <= set(manifest["provides"]))
                self.assertTrue(set(manifest["provides"]) <= {"hooks", "commands", "skills", "statusline"})
                for note in manifest["note"]:
                    self.assertEqual(set(note), {"text"})
                    self.assertTrue(note["text"].strip())
                for entry in manifest["file"]:
                    self.check_file(entry)

    def check_file(self, entry):
        self.assertIn(entry["how"], {"write", "merge-json", "merge-toml", "merge-yaml"})
        self.assertTrue(entry["to"].startswith(("~/", "{project}/")), entry["to"])
        source = PLUGIN / entry["from"]
        self.assertTrue(source.is_file(), entry["from"])
        self.assertTrue(source.resolve().is_relative_to(PLUGIN.resolve()))
        text = source.read_text()
        if entry["how"] == "write":
            self.assertNotIn("at", entry)
            self.assertEqual(Path(entry["to"]).suffix, source.suffix)
            return
        self.assertIsInstance(entry["at"], str)
        kind = entry["how"].removeprefix("merge-")
        self.assertEqual(Path(entry["to"]).suffix, {"json": ".json", "toml": ".toml", "yaml": ".yml"}[kind])
        if kind == "json":
            self.assertIsInstance(json.loads(text), dict)
        elif kind == "toml":
            tomllib.loads(text)
        else:
            parsed = load_yaml(text)
            if parsed is not None:
                self.assertIsInstance(parsed, (dict, list))

    def test_every_harness_file_is_installed_or_named_by_a_manifest(self):
        named = {
            f["from"] for harness in INSTALLS for f in tomllib.loads((INSTALL / f"{harness}.toml").read_text())["file"]
        }
        named |= {"harnesses/gemini-cli/gemini-extension.json"}
        for harness in INSTALLS:
            for path in sorted((HARNESSES / harness).rglob("*")):
                if path.is_file():
                    self.assertIn(path.relative_to(PLUGIN).as_posix(), named)


class DocsTests(unittest.TestCase):
    def test_each_harness_has_a_section(self):
        text = (ROOT / "docs/harness-hooks.md").read_text()
        for harness, name in HARNESS_NAMES.items():
            with self.subTest(harness=harness):
                self.assertRegex(text, rf"(?m)^## {re.escape(name)}$")
                section = text.split(f"## {name}\n", 1)[1].split("\n## ", 1)[0]
                self.assertIn("by init plugin", section)
                self.assertIn("Try it", section)
                self.assertIn("Not done:", section)


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
