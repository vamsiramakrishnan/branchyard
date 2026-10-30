#!/usr/bin/env python3
"""The scripted agent behind the README screenshots, a filter on the stdio of the fake ACP agent
(crates/branchyard-runtime/src/bin/fake-acp-agent.rs).

    scripted_agent.py SCRIPT.json -- AGENT [ARG...]

SCRIPT.json lists scenes, each keyed by a prompt as the person typed it. For a `session/prompt`
whose text contains a scene's prompt (after whatever instructions Branchyard put around it), the
filter writes the scene's files into its working directory, which is the branch's worktree or the
session's copy. It then reports the scene's text and tool calls as `session/update`
notifications and sends the agent one directive in place of the prompt. `REPLY_FILE <reply>`
makes the fake agent answer with the reply's text and do nothing else, which is why the filter
does the file work itself. A scene without a reply sends `HANG`, so its turn runs until someone
cancels it. Every other message passes through untouched, so the server records the prompt as
typed and the fake agent answers the rest of the protocol. The filter exits with the agent.

A scene, in SCRIPT.json's `scenes` list:

    {"prompt": "...", "say": "text before the tool calls",
     "tools": [{"kind": "read", "title": "Read src/record.rs"}],
     "files": {"src/record.rs": "..."}, "reply": "/abs/reply.txt"}
"""

from __future__ import annotations

import contextlib
import json
import os
import subprocess
import sys
import threading
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, BinaryIO

TOOL_KINDS = {"read", "edit", "delete", "move", "search", "execute", "think", "fetch", "other"}


@dataclass(frozen=True)
class Tool:
    kind: str
    title: str


@dataclass(frozen=True)
class Scene:
    prompt: str
    say: str | None = None
    tools: tuple[Tool, ...] = ()
    files: tuple[tuple[str, str], ...] = ()
    reply: str | None = None


def scene(value: dict[str, Any]) -> Scene:
    """One scene from SCRIPT.json, refusing what the filter could not act on."""
    prompt = value["prompt"]
    if not isinstance(prompt, str) or not prompt.strip():
        raise ValueError("a scene needs a prompt")
    tools = tuple(Tool(t["kind"], t["title"]) for t in value.get("tools", []))
    for tool in tools:
        if tool.kind not in TOOL_KINDS:
            raise ValueError(f"tool kind {tool.kind!r} is not one of ACP's")
    files = tuple(value.get("files", {}).items())
    for path, text in files:
        pure = PurePosixPath(path)
        if pure.is_absolute() or ".." in pure.parts:
            raise ValueError(f"file {path} is not inside the working directory")
        if not isinstance(text, str):
            raise ValueError(f"file {path} needs its text")
    return Scene(prompt, value.get("say"), tools, files, value.get("reply"))


def load(path: Path) -> list[Scene]:
    scenes = [scene(value) for value in json.loads(path.read_text(encoding="utf-8"))["scenes"]]
    prompts = [s.prompt for s in scenes]
    if len(set(prompts)) != len(prompts):
        raise ValueError("two scenes share a prompt")
    return scenes


def prompt_text(message: Any) -> str | None:
    """The text of a `session/prompt` request, or None for any other message."""
    if not isinstance(message, dict) or message.get("method") != "session/prompt" or "id" not in message:
        return None
    blocks = message.get("params", {}).get("prompt", [])
    return "\n".join(b.get("text", "") for b in blocks if b.get("type") == "text")


def match(scenes: list[Scene], text: str) -> Scene | None:
    """The scene whose prompt the text contains, or the longest when one prompt holds another."""
    found = [s for s in scenes if s.prompt in text]
    return max(found, key=lambda s: len(s.prompt)) if found else None


def directive(scene: Scene) -> str:
    return f"REPLY_FILE {scene.reply}" if scene.reply else "HANG"


def rewrite(message: dict[str, Any], scene: Scene) -> dict[str, Any]:
    """The prompt request with its text replaced by the scene's one directive."""
    params = dict(message["params"], prompt=[{"type": "text", "text": directive(scene)}])
    return dict(message, params=params)


def updates(scene: Scene, message: dict[str, Any]) -> list[dict[str, Any]]:
    """What the agent says before the reply to this prompt: its text, then each tool call. A scene
    that hangs leaves its last call in progress, as a turn that is still working would."""
    said = []
    if scene.say:
        said.append({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": scene.say}})
    for i, tool in enumerate(scene.tools, 1):
        running = scene.reply is None and i == len(scene.tools)
        said.append(
            {
                "sessionUpdate": "tool_call",
                "toolCallId": f"scripted-{message['id']}-{i}",
                "title": tool.title,
                "kind": tool.kind,
                "status": "in_progress" if running else "completed",
            }
        )
    session = message["params"]["sessionId"]
    return [
        {"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": update}}
        for update in said
    ]


def write_files(scene: Scene, root: Path) -> None:
    for path, text in scene.files:
        target = root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8")


class Filter:
    """Both directions of the agent's stdio. Both write to `out`, so every write takes the lock."""

    def __init__(self, scenes: list[Scene], agent: subprocess.Popen[bytes], out: BinaryIO) -> None:
        assert agent.stdin is not None and agent.stdout is not None
        self.scenes = scenes
        self.agent = agent
        self.out = out
        self.lock = threading.Lock()

    def send(self, line: bytes) -> None:
        with self.lock:
            self.out.write(line)
            self.out.flush()

    def from_agent(self) -> None:
        for line in self.agent.stdout or ():
            self.send(line)

    def from_client(self, line: bytes) -> bytes:
        """What the agent gets for this line from the client, after acting out a scene's prompt."""
        try:
            message = json.loads(line)
        except ValueError:
            return line
        text = prompt_text(message)
        found = match(self.scenes, text) if text is not None else None
        if found is None:
            return line
        write_files(found, Path.cwd())
        for update in updates(found, message):
            self.send(json.dumps(update).encode() + b"\n")
        return json.dumps(rewrite(message, found)).encode() + b"\n"

    def from_clients(self, lines: BinaryIO) -> None:
        stdin = self.agent.stdin
        assert stdin is not None
        with contextlib.suppress(BrokenPipeError):
            for line in lines:
                stdin.write(self.from_client(line))
                stdin.flush()
            stdin.close()


def main(argv: list[str]) -> int:
    if len(argv) < 3 or argv[1] != "--":
        print("usage: scripted_agent.py SCRIPT.json -- AGENT [ARG...]", file=sys.stderr)
        return 2
    scenes = load(Path(argv[0]))
    agent = subprocess.Popen(argv[2:], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    relay = Filter(scenes, agent, sys.stdout.buffer)
    threading.Thread(target=relay.from_clients, args=(sys.stdin.buffer,), daemon=True).start()
    relay.from_agent()
    return agent.wait()


if __name__ == "__main__":
    status = main(sys.argv[1:])
    sys.stderr.flush()
    # The client's reader may still hold stdin's lock, which a normal interpreter exit waits on.
    os._exit(status)
