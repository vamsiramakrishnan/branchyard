#!/usr/bin/env python3
"""The packaged Branchyard files for Hermes, Cursor, Antigravity and Kiro: each parses, each
manifest has the fields its harness requires, every hook command is silent with an empty PATH,
and against a stub `by` each command passes the right --harness and event. No cargo, no
network; the harnesses themselves are not run."""

import json
import os
import shlex
import shutil
import stat
import subprocess
import tempfile
import tomllib
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PLUGIN = ROOT / "plugins" / "branchyard"
HARNESSES = ("hermes", "cursor", "antigravity", "kiro")
SH = shutil.which("sh") or "/bin/sh"
GUARD = "command -v by >/dev/null 2>&1 && by "

try:
    import yaml  # type: ignore[import-untyped]
except ImportError:  # CI's Python has no PyYAML; the fallback below reads this one file's shape.
    yaml = None


def load_json(relative):
    return json.loads((PLUGIN / relative).read_text())


def hermes_hooks(use_yaml=True):
    """The `hooks:` block of harnesses/hermes/hooks.yaml as {event: [entry, ...]}."""
    text = (PLUGIN / "harnesses/hermes/hooks.yaml").read_text()
    if use_yaml and yaml is not None:
        return yaml.safe_load(text)["hooks"]
    # Without PyYAML: a two-level mapping of lists of `key: value` entries, as the file is written.
    hooks: dict = {}
    event = None
    for line in text.splitlines():
        if not line.strip() or line.lstrip().startswith("#") or line == "hooks:":
            continue
        indent = len(line) - len(line.lstrip())
        body = line.strip()
        if indent == 2 and body.endswith(":"):
            event = body[:-1]
            hooks[event] = []
            continue
        assert event is not None and indent in (4, 6), f"unexpected line: {line!r}"
        if body.startswith("- "):
            hooks[event].append({})
            body = body[2:]
        key, _, value = body.partition(": ")
        if value.startswith('"'):
            assert value.endswith('"') and "\\" not in value, value
            parsed: object = value[1:-1]
        else:
            parsed = int(value)
        hooks[event][-1][key] = parsed
    return hooks


def commands():
    """(harness, label, argv) for every packaged hook and status-line command, as each harness
    runs it: Hermes splits its command with shlex and runs it without a shell; the others hand it
    to a shell."""
    found = []
    for event, entries in hermes_hooks().items():
        for entry in entries:
            found.append(("hermes", event, shlex.split(entry["command"])))
    for event, entries in load_json("harnesses/cursor/hooks.json")["hooks"].items():
        for entry in entries:
            found.append(("cursor", event, [SH, "-c", entry["command"]]))
    status = load_json("harnesses/cursor/cli-config.json")["statusLine"]["command"]
    found.append(("cursor", "statusLine", [SH, "-c", status]))
    for event, entries in load_json("harnesses/antigravity/hooks.json")["branchyard"].items():
        for entry in entries:
            found.append(("antigravity", event, [SH, "-c", entry["command"]]))
    status = load_json("harnesses/antigravity/settings.json")["statusLine"]["command"]
    found.append(("antigravity", "statusLine", [SH, "-c", status]))
    for hook in load_json("harnesses/kiro/branchyard.json")["hooks"]:
        found.append(("kiro", hook["trigger"], [SH, "-c", hook["action"]["command"]]))
    return found


# What each native event must ask `by` for.
EXPECTED = {
    ("hermes", "pre_llm_call"): "hook --harness hermes user-prompt-submit",
    ("cursor", "sessionStart"): "hook --harness cursor session-start",
    ("cursor", "statusLine"): "statusline --harness cursor",
    ("antigravity", "SessionStart"): "hook --harness antigravity session-start",
    ("antigravity", "PreInvocation"): "hook --harness antigravity user-prompt-submit",
    ("antigravity", "statusLine"): "statusline --harness antigravity",
    ("kiro", "SessionStart"): "hook --harness kiro session-start",
    ("kiro", "UserPromptSubmit"): "hook --harness kiro user-prompt-submit",
}


def run(argv, path):
    # Hermes's argv[0] is `sh`, found on the harness's PATH; resolve it here so an empty PATH
    # tests the guard, not whether sh can be found.
    if argv[0] == "sh":
        argv = [SH, *argv[1:]]
    return subprocess.run(
        argv,
        input='{"session_id": "s", "cwd": "/nowhere"}',
        capture_output=True,
        text=True,
        env={"PATH": path, "HOME": os.environ.get("HOME", "/")},
        timeout=10,
        check=False,
    )


class Files(unittest.TestCase):
    def test_every_command_is_one_the_table_expects(self):
        self.assertEqual(sorted((h, e) for h, e, _ in commands()), sorted(EXPECTED))

    def test_every_command_is_guarded(self):
        for harness, event, argv in commands():
            with self.subTest(harness=harness, event=event):
                self.assertTrue(argv[-1].startswith(GUARD), argv[-1])
                self.assertTrue(argv[-1].endswith(" || true"), argv[-1])

    def test_hermes_runs_a_shell_itself_with_a_timeout(self):
        for entries in hermes_hooks().values():
            for entry in entries:
                self.assertEqual(shlex.split(entry["command"])[:2], ["sh", "-c"])
                self.assertTrue(0 < entry["timeout"] <= 300)
        # on_session_start's output is ignored; running `by` there would spend the summary.
        self.assertNotIn("on_session_start", hermes_hooks())

    @unittest.skipIf(yaml is None, "PyYAML is not installed")
    def test_the_fallback_reader_agrees_with_pyyaml(self):
        self.assertEqual(hermes_hooks(use_yaml=False), hermes_hooks())

    def test_cursor_hooks_and_plugin(self):
        hooks = load_json("harnesses/cursor/hooks.json")
        self.assertEqual(hooks["version"], 1)
        # beforeSubmitPrompt can only allow or block a prompt, not add context.
        self.assertEqual(set(hooks["hooks"]), {"sessionStart"})
        manifest = load_json(".cursor-plugin/plugin.json")
        self.assertEqual(manifest["name"], "branchyard")
        for field in ("skills", "commands", "hooks"):
            self.assertTrue((PLUGIN / manifest[field]).exists(), field)
        self.assertEqual(manifest["hooks"], "./harnesses/cursor/hooks.json")
        market = json.loads((ROOT / ".cursor-plugin/marketplace.json").read_text())
        self.assertTrue(market["owner"]["name"])
        (entry,) = market["plugins"]
        self.assertEqual(entry["name"], manifest["name"])
        self.assertEqual((ROOT / entry["source"]).resolve(), PLUGIN.resolve())
        self.assertEqual(load_json("harnesses/cursor/cli-config.json")["statusLine"]["type"], "command")

    def test_antigravity_plugin(self):
        manifest = load_json("harnesses/antigravity/plugin.json")
        # The schema allows no other keys.
        self.assertLessEqual(set(manifest), {"$schema", "name", "description"})
        self.assertRegex(manifest["name"], r"^[a-zA-Z0-9-_]+$")
        hooks = load_json("harnesses/antigravity/hooks.json")
        self.assertEqual(set(hooks), {"branchyard"})
        # Both events take a flat list of handlers (agy 1.4.7): no matcher group.
        self.assertEqual(set(hooks["branchyard"]), {"SessionStart", "PreInvocation"})
        for entries in hooks["branchyard"].values():
            for entry in entries:
                self.assertNotIn("hooks", entry)
                self.assertNotIn("matcher", entry)
                self.assertEqual(entry["type"], "command")
                self.assertIsInstance(entry["timeout"], int)
        status = load_json("harnesses/antigravity/settings.json")["statusLine"]
        self.assertEqual(status["type"], "command")
        self.assertIs(status["stack_with_default"], True)

    def test_kiro_hook_file(self):
        doc = load_json("harnesses/kiro/branchyard.json")
        self.assertEqual(doc["version"], "v1")
        for hook in doc["hooks"]:
            self.assertTrue(hook["name"])
            self.assertEqual(hook["action"]["type"], "command")
            self.assertIsInstance(hook["timeout"], int)


class InstallManifests(unittest.TestCase):
    def test_each_manifest_names_files_that_exist(self):
        for harness in HARNESSES:
            with self.subTest(harness=harness):
                manifest = tomllib.loads((PLUGIN / "install" / f"{harness}.toml").read_text())
                self.assertEqual(manifest["harness"], harness)
                self.assertTrue(manifest["binaries"])
                self.assertTrue(set(manifest["provides"]) <= {"hooks", "commands", "skills", "statusline"})
                self.assertTrue(manifest["file"])
                for entry in manifest["file"]:
                    source = PLUGIN / entry["from"]
                    self.assertTrue(source.resolve().is_relative_to(PLUGIN.resolve()), entry["from"])
                    self.assertFalse(source.is_symlink(), entry["from"])
                    if source.is_dir():
                        self.assertEqual(entry["how"], "write")
                        payload = list(source.rglob("*"))
                        self.assertTrue(any(path.is_file() for path in payload), entry["from"])
                        for path in payload:
                            self.assertFalse(path.is_symlink(), str(path))
                            self.assertTrue(path.is_file() or path.is_dir(), str(path))
                            self.assertTrue(path.resolve().is_relative_to(source.resolve()), str(path))
                    else:
                        self.assertTrue(source.is_file(), entry["from"])
                    self.assertTrue(entry["to"].startswith(("~/", "{project}/")), entry["to"])
                    self.assertIn(entry["how"], {"write", "merge-json", "merge-toml", "merge-yaml"})
                    suffix = {"merge-json": ".json", "merge-yaml": ".yaml", "merge-toml": ".toml"}
                    if entry["how"] in suffix:
                        self.assertTrue(entry["from"].endswith(suffix[entry["how"]]), entry["from"])
                        self.assertTrue(entry["to"].endswith(suffix[entry["how"]]), entry["to"])
                for note in manifest.get("note", []):
                    self.assertTrue(note["text"].strip())

    def test_hermes_merges_into_its_config(self):
        manifest = tomllib.loads((PLUGIN / "install/hermes.toml").read_text())
        (entry,) = manifest["file"]
        self.assertEqual((entry["to"], entry["how"]), ("~/.hermes/config.yaml", "merge-yaml"))
        self.assertTrue(any("HERMES_ACCEPT_HOOKS" in n["text"] for n in manifest["note"]))
        # Hermes 0.19.0 skips an unapproved hook in `hermes chat -q` without a terminal too.
        self.assertTrue(any("hermes chat -q" in n["text"] for n in manifest["note"]))


class Commands(unittest.TestCase):
    def setUp(self):
        self.bin = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.bin)
        stub = self.bin / "by"
        stub.write_text('#!/bin/sh\necho "$@"\n')
        stub.chmod(stub.stat().st_mode | stat.S_IXUSR)

    def test_without_by_every_command_is_silent(self):
        for harness, event, argv in commands():
            with self.subTest(harness=harness, event=event):
                result = run(argv, "")
                self.assertEqual((result.returncode, result.stdout, result.stderr), (0, "", ""))

    def test_each_command_asks_by_for_its_harness_and_event(self):
        for harness, event, argv in commands():
            with self.subTest(harness=harness, event=event):
                result = run(argv, f"{self.bin}{os.pathsep}/usr/bin{os.pathsep}/bin")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), EXPECTED[(harness, event)])


if __name__ == "__main__":
    unittest.main()
