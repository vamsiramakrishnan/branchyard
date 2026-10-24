#!/usr/bin/env python3
"""The Pi/Oh My Pi, Amp and OpenCode plugins under plugins/branchyard/harnesses/, run by node
against a fake host API and a stub `by` on PATH: the right `by hook --harness generic` and
`by statusline --harness generic` arguments and stdin, the context injected where each host
takes it, and nothing (no throw) when `by` is missing, silent, prints garbage or hangs past
the plugins' 3 s timeout. Also parses the packaging: package.json, the install manifests and
their targets. Skips the node runs when no node is found. No network, no npm installs."""

import json
import os
import shutil
import subprocess
import tempfile
import time
import tomllib
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PLUGIN = ROOT / "plugins" / "branchyard"
HARNESSES = PLUGIN / "harnesses"
INSTALL = PLUGIN / "install"
SOURCES = {
    "pi": HARNESSES / "pi" / "branchyard.ts",
    "amp": HARNESSES / "amp" / "branchyard.ts",
    "opencode": HARNESSES / "opencode" / "branchyard.js",
}
MANIFESTS = ("pi", "oh-my-pi", "amp", "opencode")


def find_node():
    for candidate in (os.environ.get("BY_TEST_NODE"), "/opt/node22/bin/node", shutil.which("node")):
        if candidate and Path(candidate).is_file() and os.access(candidate, os.X_OK):
            return candidate
    return None


NODE = find_node()

# Logs each call's arguments and stdin (one line each), then answers by STUB_MODE.
STUB = r"""#!/bin/sh
PATH=/usr/bin:/bin
input=$(cat)
printf '%s\n' "$*" >> "$STUB_LOG.args"
printf '%s\n' "$input" >> "$STUB_LOG.stdin"
case "$STUB_MODE" in
  hang) exec sleep 30 ;;
  silent) exit 0 ;;
  garbage) [ "$1" = hook ] && { echo 'not json {'; exit 0; } ;;
  fail) echo '{"context": "should not be used"}'; exit 3 ;;
esac
case "$1" in
  hook) printf '{"context": "CTX %s"}\n' "$4" ;;
  statusline) echo "STATUS line"; echo "second line ignored" ;;
esac
exit 0
"""

# Loads one plugin into a fake host, runs a scenario, prints what the host saw as JSON.
DRIVER = r"""
const [kind, pluginPath, cwd, scenario] = process.argv.slice(2);
const mod = await import(pluginPath);
const out = { kind, results: [], status: [], threw: [], elapsed: [] };
const timed = async (label, fn) => {
  const start = Date.now();
  try {
    out.results.push(await fn());
  } catch (error) {
    out.threw.push(label + ": " + String(error && error.stack || error));
    out.results.push("THREW");
  }
  out.elapsed.push(Date.now() - start);
};
const settle = () => new Promise((resolve) => setTimeout(resolve, 50));

if (kind === "pi") {
  out.factory = typeof mod.default;
  const handlers = {};
  const pi = { on: (name, fn) => { handlers[name] = fn; } };
  mod.default(pi);
  out.events = Object.keys(handlers).sort();
  const ctx = (id) => ({
    cwd,
    hasUI: true,
    sessionManager: { getSessionId: () => id },
    ui: { setStatus: (key, text) => out.status.push([key, text === undefined ? null : text]) },
  });
  const prompt = (text, id) => handlers.before_agent_start({ type: "before_agent_start", prompt: text }, ctx(id));
  if (scenario === "full") {
    await timed("session_start", () => handlers.session_start({ type: "session_start" }, ctx("sess-1")));
    await timed("first", () => prompt("hi", "sess-1"));
    await timed("second", () => prompt("again", "sess-1"));
    await timed("other", () => prompt("new", "sess-2"));
    await timed("agent_end", () => handlers.agent_end({ type: "agent_end" }, ctx("sess-1")));
    await timed("bare ctx", () => handlers.before_agent_start({ type: "before_agent_start" }, {}));
  } else {
    await timed("first", () => prompt("hi", "sess-1"));
  }
} else if (kind === "amp") {
  out.factory = typeof mod.default;
  out.description = mod.description;
  const handlers = {};
  const items = [];
  const amp = {
    on: (name, fn) => { handlers[name] = fn; return { unsubscribe() {} }; },
    system: { workspaceRoot: { toString: () => "file://" + cwd } },
    helpers: { filePathFromURI: (uri) => String(uri).replace(/^file:\/\//, "") },
    createStatusItem: () => {
      const item = { update: (value) => out.status.push(value.text), unsubscribe() {} };
      items.push(item);
      return item;
    },
  };
  mod.default(amp);
  out.events = Object.keys(handlers).sort();
  if (scenario === "full") {
    await timed("session.start", () => handlers["session.start"]({ thread: { id: "T-1" } }, {}));
    await timed("first", () => handlers["agent.start"]({ thread: { id: "T-1" }, message: "hi", id: 1 }, {}));
    await timed("second", () => handlers["agent.start"]({ thread: { id: "T-1" }, message: "again", id: 2 }, {}));
    await timed("other", () => handlers["agent.start"]({ thread: { id: "T-2" }, message: "new", id: 1 }, {}));
    const end = { thread: { id: "T-1" }, message: "hi", id: 1, status: "done" };
    await timed("agent.end", () => handlers["agent.end"](end, {}));
  } else {
    await timed("first", () => handlers["agent.start"]({ thread: { id: "T-1" }, message: "hi", id: 1 }, {}));
  }
  out.items = items.length;
} else if (kind === "opencode") {
  out.factory = typeof mod.default;
  out.id = mod.default && mod.default.id;
  out.exports = Object.keys(mod).sort();
  const hooks = await mod.default.server({ directory: cwd, worktree: cwd, project: {}, client: {} }, {});
  out.events = Object.keys(hooks).sort();
  const send = async (sessionID, messageID) => {
    const output = {
      message: { id: messageID, sessionID, role: "user" },
      parts: [{ id: "prt_user", type: "text", text: "hello" }],
    };
    await hooks["chat.message"]({ sessionID, messageID, agent: "build" }, output);
    return output.parts;
  };
  if (scenario === "full") {
    await timed("first", () => send("ses_1", "msg_1"));
    await timed("second", () => send("ses_1", "msg_2"));
    await timed("other", () => send("ses_2", "msg_1"));
    await timed("no parts", async () => {
      await hooks["chat.message"]({ sessionID: "ses_3" }, {});
      return "ok";
    });
  } else {
    await timed("first", () => send("ses_1", "msg_1"));
  }
}
await settle();
process.stdout.write(JSON.stringify(out));
"""


class Packaging(unittest.TestCase):
    def test_package_json_files_parse_and_name_their_entry_points(self):
        pi = json.loads((HARNESSES / "pi" / "package.json").read_text())
        for key in ("pi", "omp"):
            for entry in pi[key]["extensions"]:
                self.assertTrue((HARNESSES / "pi" / entry).is_file(), entry)
        self.assertIn("pi-package", pi["keywords"])
        opencode = json.loads((HARNESSES / "opencode" / "package.json").read_text())
        self.assertEqual(opencode["type"], "module")
        self.assertTrue((HARNESSES / "opencode" / opencode["main"]).is_file())
        self.assertTrue((HARNESSES / "opencode" / opencode["exports"]["./server"]).is_file())
        for manifest in (pi, opencode):
            self.assertNotIn("dependencies", manifest)

    def test_sources_import_only_node_builtins_and_call_the_generic_interface(self):
        for name, path in SOURCES.items():
            text = path.read_text()
            imports = [line for line in text.splitlines() if line.startswith("import ")]
            self.assertTrue(imports, name)
            for line in imports:
                self.assertRegex(line, r'from "node:[a-z_]+";$', f"{name}: {line}")
            self.assertIn('"--harness", "generic"', text, name)
            self.assertIn("TIMEOUT_MS = 3000", text, name)

    def test_install_manifests_parse_and_point_at_packaged_files(self):
        catalog = tomllib.loads((ROOT / "catalog" / "harnesses.toml").read_text())
        binaries = {h["id"]: h.get("binaries", []) for h in catalog["harness"]}
        for name in MANIFESTS:
            with self.subTest(name=name):
                manifest = tomllib.loads((INSTALL / f"{name}.toml").read_text())
                self.assertEqual(manifest["harness"], name)
                self.assertEqual(manifest["binaries"], binaries[name])
                self.assertIn("hooks", manifest["provides"])
                self.assertTrue(manifest["file"])
                for entry in manifest["file"]:
                    self.assertTrue((PLUGIN / entry["from"]).is_file(), entry["from"])
                    self.assertTrue(entry["to"].startswith(("~/", "{project}/")), entry["to"])
                    self.assertEqual(entry["how"], "write")
                    self.assertEqual(Path(entry["to"]).suffix, Path(entry["from"]).suffix)
                self.assertTrue(all(note["text"] for note in manifest.get("note", [])))

    def test_docs_have_a_section_for_each_harness(self):
        text = (ROOT / "docs" / "harness-hooks.md").read_text()
        for heading in ("## Pi", "## Oh My Pi", "## Amp", "## OpenCode"):
            self.assertIn(f"\n{heading}\n", text)


@unittest.skipIf(NODE is None, "node not found (set BY_TEST_NODE)")
class Plugins(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        stub = self.bin / "by"
        stub.write_text(STUB)
        stub.chmod(0o755)
        self.empty = self.tmp / "empty"
        self.empty.mkdir()
        self.cwd = self.tmp / "repo"
        self.cwd.mkdir()
        self.log = self.tmp / "by"
        (self.tmp / "package.json").write_text('{"type": "module"}')
        (self.tmp / "driver.mjs").write_text(DRIVER)

    def run_plugin(self, kind, mode="ok", scenario="full", path=None):
        for suffix in (".args", ".stdin"):
            Path(f"{self.log}{suffix}").unlink(missing_ok=True)
        plugin = self.tmp / f"{kind}.mjs"
        shutil.copyfile(SOURCES[kind], plugin)
        env = {
            "PATH": str(path or self.bin),
            "STUB_LOG": str(self.log),
            "STUB_MODE": mode,
            "HOME": str(self.tmp),
        }
        argv = [NODE, str(self.tmp / "driver.mjs"), kind, plugin.as_uri(), str(self.cwd), scenario]
        start = time.monotonic()
        done = subprocess.run(argv, env=env, capture_output=True, text=True, timeout=30)
        self.elapsed = time.monotonic() - start
        self.assertEqual(done.returncode, 0, done.stderr)
        out = json.loads(done.stdout)
        self.assertEqual(out["threw"], [], out["threw"])
        return out

    def calls(self):
        args_file = Path(f"{self.log}.args")
        if not args_file.exists():
            return []
        args = args_file.read_text().splitlines()
        stdin = Path(f"{self.log}.stdin").read_text().splitlines()
        return [(a, json.loads(s)) for a, s in zip(args, stdin, strict=True)]

    def hook_calls(self):
        return [(a, s) for a, s in self.calls() if a.startswith("hook ")]

    def assert_generic_calls(self, first_session, other_session):
        hooks = self.hook_calls()
        self.assertEqual(
            [a for a, _ in hooks][:3],
            [
                "hook --harness generic session-start",
                "hook --harness generic user-prompt-submit",
                "hook --harness generic session-start",
            ],
        )
        self.assertEqual(hooks[0][1], {"cwd": str(self.cwd), "session_id": first_session})
        self.assertEqual(hooks[2][1], {"cwd": str(self.cwd), "session_id": other_session})
        status = [s for a, s in self.calls() if a == "statusline --harness generic"]
        return status

    # Pi and Oh My Pi share this source.
    def test_pi_injects_a_message_and_sets_the_status(self):
        out = self.run_plugin("pi")
        self.assertEqual(out["factory"], "function")
        self.assertEqual(out["events"], ["agent_end", "before_agent_start", "session_start"])
        _start, first, second, other, _end, bare = out["results"]
        self.assertEqual(
            first,
            {"message": {"customType": "branchyard", "content": "CTX session-start", "display": True}},
        )
        self.assertEqual(second["message"]["content"], "CTX user-prompt-submit")
        self.assertEqual(other["message"]["content"], "CTX session-start")
        self.assertEqual(bare["message"]["content"], "CTX session-start")
        status = self.assert_generic_calls("sess-1", "sess-2")
        self.assertTrue(status)
        self.assertTrue(all(s == {"cwd": str(self.cwd)} for s in status))
        self.assertIn(["branchyard", "STATUS line"], out["status"])

    def test_amp_returns_a_message_and_updates_a_status_item(self):
        out = self.run_plugin("amp")
        self.assertEqual(out["factory"], "function")
        self.assertLessEqual(len(out["description"]), 300)
        self.assertEqual(out["events"], ["agent.end", "agent.start", "session.start"])
        _start, first, second, other, _end = out["results"]
        self.assertEqual(first, {"message": {"content": "CTX session-start", "display": True}})
        self.assertEqual(second["message"]["content"], "CTX user-prompt-submit")
        self.assertEqual(other["message"]["content"], "CTX session-start")
        status = self.assert_generic_calls("T-1", "T-2")
        self.assertTrue(all(s == {"cwd": str(self.cwd)} for s in status))
        self.assertEqual(out["status"], ["STATUS line"])
        self.assertEqual(out["items"], 1)

    def test_opencode_adds_a_synthetic_text_part(self):
        out = self.run_plugin("opencode")
        self.assertEqual(out["exports"], ["default"])
        self.assertEqual(out["id"], "branchyard")
        self.assertEqual(out["events"], ["chat.message"])
        first, second, other, no_parts = out["results"]
        self.assertEqual(no_parts, "ok")
        added = first[1]
        self.assertEqual(len(first), 2)
        self.assertRegex(added["id"], r"^prt_[0-9a-f]{12}[0-9A-Za-z]{14}$")
        self.assertEqual(
            {k: v for k, v in added.items() if k != "id"},
            {
                "sessionID": "ses_1",
                "messageID": "msg_1",
                "type": "text",
                "text": "CTX session-start",
                "synthetic": True,
            },
        )
        self.assertEqual(second[1]["text"], "CTX user-prompt-submit")
        self.assertEqual(second[1]["messageID"], "msg_2")
        self.assertEqual(other[1]["text"], "CTX session-start")
        self.assert_generic_calls("ses_1", "ses_2")

    def assert_nothing_added(self, kind, out, status=True):
        if kind == "opencode":
            self.assertTrue(all(len(parts) == 1 for parts in out["results"] if parts != "ok"))
        else:
            self.assertTrue(all(r is None for r in out["results"]), out["results"])
        if not status:
            return
        shown = [entry[1] if isinstance(entry, list) else entry for entry in out["status"]]
        self.assertTrue(all(text in (None, "") for text in shown), out["status"])

    def test_a_missing_by_adds_nothing(self):
        for kind in SOURCES:
            with self.subTest(kind=kind):
                out = self.run_plugin(kind, path=self.empty)
                self.assert_nothing_added(kind, out)
                self.assertEqual(self.calls(), [])

    def test_silent_garbage_or_failing_by_adds_nothing(self):
        for mode in ("silent", "garbage", "fail"):
            for kind in SOURCES:
                with self.subTest(kind=kind, mode=mode):
                    out = self.run_plugin(kind, mode=mode)
                    # The status line is plain text, so only the hook's garbage is refused.
                    self.assert_nothing_added(kind, out, status=mode != "garbage")
                    self.assertTrue(self.hook_calls())

    def test_a_hanging_by_times_out_after_three_seconds(self):
        for kind in SOURCES:
            with self.subTest(kind=kind):
                out = self.run_plugin(kind, mode="hang", scenario="one")
                self.assert_nothing_added(kind, out)
                self.assertGreaterEqual(out["elapsed"][0], 2900)
                self.assertLess(out["elapsed"][0], 4500)
                self.assertLess(self.elapsed, 10)


if __name__ == "__main__":
    unittest.main()
