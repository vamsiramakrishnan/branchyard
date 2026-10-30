#!/usr/bin/env python3
"""tools/readme_screenshots/, which makes the README screenshots: the filter that plays the demo
in front of the fake agent (which prompt picks which scene, the directive the agent gets instead,
what the client hears first, the files a scene writes, both directions of the stdio relay and its
exit), and the script generate.py writes from story.py, whose every turn must find its own scene.
No server, no browser."""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
TOOL = REPO / "tools" / "readme_screenshots"
FILTER = TOOL / "scripted_agent.py"
sys.path.insert(0, str(TOOL))
import generate  # noqa: E402
import scripted_agent as agent  # noqa: E402
import story  # noqa: E402

PREAMBLE = "<branchyard-instructions>\nRead INDEX.md first.\n</branchyard-instructions>\n\nSkills are available.\n\n"
HINT = "\nRead `.console-inputs/w/s/INDEX.md` first for this attempt's selected immutable inputs."
# Echoes each line, saying whether the scene's file was already there when the line arrived.
STUB = [
    sys.executable,
    "-c",
    "import json, os, sys\n"
    "for line in sys.stdin:\n"
    "    message = json.loads(line)\n"
    "    message['file_there'] = os.path.exists('src/lib.rs')\n"
    "    print(json.dumps(message), flush=True)\n",
]


def prompt(text, request=7, session="fake-session-1"):
    params = {"sessionId": session, "prompt": [{"type": "text", "text": text}]}
    return {"jsonrpc": "2.0", "id": request, "method": "session/prompt", "params": params}


def scene(**value):
    return agent.scene({"prompt": "Fix the flaky test.", **value})


def relay(script, sent, cwd):
    return subprocess.run(
        [sys.executable, "-I", str(FILTER), str(script), "--", *STUB],
        input="".join(json.dumps(m) + "\n" for m in sent),
        capture_output=True,
        text=True,
        cwd=cwd,
        timeout=60,
        check=True,
    )


class Matching(unittest.TestCase):
    def test_a_prompt_inside_branchyards_instructions_finds_its_scene(self):
        fix = scene(reply="/r/1.txt")
        text = agent.prompt_text(prompt(PREAMBLE + "Fix the flaky test." + HINT))
        self.assertIs(agent.match([fix], text or ""), fix)

    def test_an_unscripted_prompt_finds_none(self):
        self.assertIsNone(agent.match([scene(reply="/r/1.txt")], "Fix the build."))

    def test_the_longest_prompt_wins_when_one_holds_another(self):
        short = agent.scene({"prompt": "Fix it."})
        long = agent.scene({"prompt": "Fix it. Add a test."})
        self.assertIs(agent.match([short, long], PREAMBLE + "Fix it. Add a test."), long)

    def test_only_a_prompt_request_has_prompt_text(self):
        self.assertIsNone(agent.prompt_text({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}))
        self.assertIsNone(agent.prompt_text(["not", "a", "message"]))
        notification = prompt("Fix the flaky test.")
        del notification["id"]
        self.assertIsNone(agent.prompt_text(notification))


class Rewriting(unittest.TestCase):
    def test_a_scene_with_a_reply_becomes_one_reply_file_line(self):
        sent = prompt(PREAMBLE + "Fix the flaky test.")
        rewritten = agent.rewrite(sent, scene(reply="/r/1.txt"))
        self.assertEqual(rewritten["params"]["prompt"], [{"type": "text", "text": "REPLY_FILE /r/1.txt"}])
        self.assertEqual((rewritten["id"], rewritten["params"]["sessionId"]), (7, "fake-session-1"))
        self.assertIn("Fix the flaky test.", sent["params"]["prompt"][0]["text"], "the original is left alone")

    def test_a_scene_without_a_reply_hangs(self):
        rewritten = agent.rewrite(prompt("Fix the flaky test."), scene())
        self.assertEqual(rewritten["params"]["prompt"][0]["text"], "HANG")


class Updates(unittest.TestCase):
    def test_the_text_comes_first_then_each_tool_call_for_the_session(self):
        tools = [{"kind": "read", "title": "Read src/lib.rs"}, {"kind": "execute", "title": "Run cargo test"}]
        sent = agent.updates(scene(say="Looking first.", tools=tools, reply="/r/1.txt"), prompt("x", session="s-9"))
        self.assertEqual({u["params"]["sessionId"] for u in sent}, {"s-9"})
        said = [u["params"]["update"] for u in sent]
        self.assertEqual(
            said[0], {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Looking first."}}
        )
        self.assertEqual(
            [(u["title"], u["kind"], u["status"]) for u in said[1:]],
            [("Read src/lib.rs", "read", "completed"), ("Run cargo test", "execute", "completed")],
        )

    def test_tool_call_ids_differ_within_a_turn_and_between_turns(self):
        tools = [{"kind": "read", "title": "Read a"}, {"kind": "read", "title": "Read b"}]
        busy = scene(tools=tools, reply="/r/1.txt")
        ids = [
            u["params"]["update"]["toolCallId"]
            for request in (7, 8)
            for u in agent.updates(busy, prompt("x", request=request))
        ]
        self.assertEqual(len(set(ids)), 4)

    def test_a_hanging_scene_leaves_its_last_call_in_progress(self):
        tools = [{"kind": "edit", "title": "Edit Cargo.toml"}, {"kind": "execute", "title": "Run cargo bench"}]
        said = [u["params"]["update"] for u in agent.updates(scene(tools=tools), prompt("x"))]
        self.assertEqual([u["status"] for u in said], ["completed", "in_progress"])


class Scenes(unittest.TestCase):
    def test_a_file_outside_the_working_directory_is_refused(self):
        for path in ("/etc/passwd", "../outside.txt", "src/../../outside.txt"):
            with self.assertRaises(ValueError, msg=path):
                scene(files={path: "x"})

    def test_a_file_needs_its_text(self):
        with self.assertRaises(ValueError):
            scene(files={"a.txt": {"copy": "/elsewhere"}})

    def test_a_tool_kind_must_be_one_acp_has(self):
        with self.assertRaises(ValueError):
            scene(tools=[{"kind": "browse", "title": "Open the docs"}])

    def test_two_scenes_may_not_share_a_prompt(self):
        with tempfile.TemporaryDirectory() as tmp:
            script = Path(tmp) / "script.json"
            script.write_text(json.dumps({"scenes": [{"prompt": "Same."}, {"prompt": "Same."}]}))
            with self.assertRaises(ValueError):
                agent.load(script)

    def test_files_are_written_under_the_working_directory(self):
        with tempfile.TemporaryDirectory() as tmp:
            work = Path(tmp) / "work"
            agent.write_files(scene(files={"src/record.rs": "pub struct Record;\n"}), work)
            self.assertEqual((work / "src" / "record.rs").read_text(), "pub struct Record;\n")


class Relay(unittest.TestCase):
    def test_the_scene_happens_before_the_agent_gets_its_directive(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            script = root / "script.json"
            fix = {
                "prompt": "Fix the flaky test.",
                "say": "Looking first.",
                "tools": [{"kind": "read", "title": "Read src/lib.rs"}],
                "files": {"src/lib.rs": "// fixed\n"},
                "reply": "/r/1.txt",
            }
            script.write_text(json.dumps({"scenes": [fix]}))
            initialize = {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}}
            other = prompt("Fix the build.", request=8)
            work = root / "work"
            work.mkdir()
            done = relay(script, [initialize, prompt(PREAMBLE + "Fix the flaky test."), other], work)
            heard = [json.loads(line) for line in done.stdout.splitlines()]
            echoed = [m for m in heard if "id" in m]
            self.assertTrue(echoed[1].pop("file_there"), "the agent got its directive before the file was written")
            for message in echoed:
                message.pop("file_there", None)
            self.assertEqual([echoed[0], echoed[2]], [initialize, other], "an unscripted message passes unchanged")
            self.assertEqual(echoed[1]["params"]["prompt"][0]["text"], "REPLY_FILE /r/1.txt")
            told = [m for m in heard[: heard.index(echoed[1])] if m.get("method") == "session/update"]
            self.assertEqual(
                [m["params"]["update"]["sessionUpdate"] for m in told], ["agent_message_chunk", "tool_call"]
            )
            self.assertEqual({m["params"]["sessionId"] for m in told}, {"fake-session-1"})
            self.assertEqual((work / "src" / "lib.rs").read_text(), "// fixed\n")

    def test_it_exits_with_the_agent_while_its_client_stays_connected(self):
        with tempfile.TemporaryDirectory() as tmp:
            script = Path(tmp) / "script.json"
            script.write_text(json.dumps({"scenes": []}))
            dying = [sys.executable, "-c", "import sys; sys.exit(3)"]
            with subprocess.Popen(
                [sys.executable, "-I", str(FILTER), str(script), "--", *dying], stdin=subprocess.PIPE
            ) as running:
                self.assertEqual(running.wait(timeout=60), 3)

    def test_it_exits_with_the_agents_status_when_its_client_leaves(self):
        with tempfile.TemporaryDirectory() as tmp:
            script = Path(tmp) / "script.json"
            script.write_text(json.dumps({"scenes": []}))
            failing = [sys.executable, "-c", "import sys; sys.stdin.read(); sys.exit(3)"]
            done = subprocess.run(
                [sys.executable, "-I", str(FILTER), str(script), "--", *failing], input="", timeout=60
            )
            self.assertEqual(done.returncode, 3)


class Story(unittest.TestCase):
    def test_every_turn_of_the_story_finds_its_own_scene(self):
        turns = [b.turn for b in story.BRANCHES] + [t for s in story.SESSIONS for t in s.turns]
        with tempfile.TemporaryDirectory() as tmp:
            scenes = agent.load(generate.write_script(Path(tmp)))
            for turn in turns:
                found = agent.match(scenes, PREAMBLE + turn.prompt + HINT)
                self.assertIsNotNone(found, turn.prompt)
                assert found is not None
                self.assertEqual(found.prompt, turn.prompt)
                if turn.reply is None:
                    self.assertEqual(agent.directive(found), "HANG")
                else:
                    self.assertEqual(Path(found.reply or "").read_text(), turn.reply)

    def test_a_turn_with_outputs_lists_each_in_the_manifest_its_contract_reads(self):
        turn = next(t for s in story.SESSIONS for t in s.turns if t.outputs)
        with tempfile.TemporaryDirectory() as tmp:
            files = generate.scene(turn, Path(tmp), 0)["files"]
        manifest = json.loads(files["outputs/artifacts.json"])
        self.assertEqual([f["path"] for f in manifest["files"]], list(turn.outputs))
        self.assertTrue(all(f"outputs/{name}" in files for name in turn.outputs))

    def test_what_the_screenshots_open_exists(self):
        self.assertIn(story.OPEN_BRANCH, [b.name for b in story.BRANCHES])
        session = next(s for s in story.SESSIONS if s.name == story.OPEN_SESSION)
        name, number = story.OPEN_OUTPUT
        self.assertIn(name, session.turns[number - 1].outputs)
        self.assertTrue(session.collect, "only a session that collects outputs publishes them")


if __name__ == "__main__":
    unittest.main()
