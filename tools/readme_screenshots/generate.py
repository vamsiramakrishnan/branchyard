#!/usr/bin/env python3
"""Regenerate the README screenshots of the browser console from a scripted demo.

    python3 tools/readme_screenshots/generate.py --by BY --agent FAKE_ACP_AGENT [--out DIR]

It builds nothing. BY is a built `by`, and FAKE_ACP_AGENT the fake agent from
`cargo build -p branchyard-runtime --bin fake-acp-agent`. In a new temporary directory it makes
a small Rust repository and a server configuration that runs Claude Code's ACP profile as the
scripted agent (scripted_agent.py) in front of the fake agent. It starts `by serve --app
--knowledge`, plays story.py through the API (branches in every lane, a knowledge workspace with
two sessions and the tables they publish), and has shoot.js sign a desktop and a phone browser in
and take the screenshots. They replace the ones in DIR (default assets/screenshots) only once
shoot.js has taken all of them. It calls no model. A failed run keeps its temporary directory,
with the server's log, and prints where it is.

Playwright comes from PLAYWRIGHT (the module's directory) or `$(npm root -g)/playwright`, and
PLAYWRIGHT_BROWSERS_PATH goes to it unchanged. The table preview needs the isolated converter,
BRANCHYARD_PREVIEW_BWRAP and an artifact runtime interpreter in BRANCHYARD_PREVIEW_PYTHON
(docs/artifact-previews.md).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import secrets
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid
from collections.abc import Callable, Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from functools import partial
from pathlib import Path
from typing import Any

import story

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
# A profile ID: the fake agent speaks ACP, and plain `claude-code` picks the stream-json profile.
# Branches record and show the harness, `claude-code`.
HARNESS = "claude-code-acp"
PREVIEW = ("BRANCHYARD_PREVIEW_BWRAP", "BRANCHYARD_PREVIEW_PYTHON")
OUTPUT_CONTRACT = {
    "schema": {"type": "object", "required": ["summary"], "properties": {"summary": {"type": "string"}}},
    "required": True,
    "purpose": "deliverable",
    "capture": {"manifest": "outputs/artifacts.json"},
}
DEMO_IDENTITY = {
    "GIT_AUTHOR_NAME": "Demo",
    "GIT_AUTHOR_EMAIL": "demo@example.com",
    "GIT_COMMITTER_NAME": "Demo",
    "GIT_COMMITTER_EMAIL": "demo@example.com",
    "GIT_CONFIG_NOSYSTEM": "1",
}
# A branch on its way to a state passes through these; any other state means it went elsewhere.
MOVING = {None, "running", "waiting"}


class Failed(Exception):
    """The demo did not reach the state a screenshot needs."""


def until(what: str, check: Callable[[], Any], timeout: float = 90) -> Any:
    deadline = time.monotonic() + timeout
    while True:
        value = check()
        if value:
            return value
        if time.monotonic() > deadline:
            raise Failed(f"timed out waiting for {what}")
        time.sleep(0.25)


@dataclass
class Api:
    url: str
    token: str

    def call(
        self, method: str, path: str, body: Any = None, raw: bytes | None = None, headers: dict[str, str] | None = None
    ) -> Any:
        sent = {
            "Authorization": f"Bearer {self.token}",
            "Accept": "application/json",
            "Idempotency-Key": uuid.uuid4().hex,
            **(headers or {}),
        }
        if body is not None:
            raw = json.dumps(body).encode()
            sent["Content-Type"] = "application/json"
        request = urllib.request.Request(self.url + path, data=raw, method=method, headers=sent)
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                return json.loads(response.read() or b"null")
        except urllib.error.HTTPError as error:
            raise Failed(f"{method} {path}: {error.code} {error.read().decode(errors='replace')}") from None

    def finished(self, operation: dict[str, Any]) -> dict[str, Any]:
        def done() -> dict[str, Any] | None:
            op = self.call("GET", f"/v1/operations/{operation['id']}")
            return None if op["state"] in ("queued", "running") else op

        op = until(f"operation {operation['id']}", done)
        if op["state"] != "succeeded":
            raise Failed(f"operation {op['id']} {op['state']}: {op.get('error')}")
        return op


def build_repo(root: Path, env: dict[str, str]) -> Path:
    repo = root / story.REPO
    for path, text in story.REPO_FILES.items():
        (repo / path).parent.mkdir(parents=True, exist_ok=True)
        (repo / path).write_text(text, encoding="utf-8")
    for command in (
        ["init", "-q", "-b", "main"],
        ["add", "-A"],
        ["commit", "-q", "-m", "Parse and print logfmt lines"],
    ):
        subprocess.run(["git", *command], cwd=repo, env=env, check=True)
    return repo


def scene(turn: story.Turn, root: Path, number: int) -> dict[str, Any]:
    """The scripted agent's scene for one turn. Its outputs go under outputs/, with the manifest
    that the session's result contract asks for, and this writes its reply under root."""
    files = dict(turn.files)
    listed = []
    for name, text in turn.outputs.items():
        files[f"outputs/{name}"] = text
        listed.append({"path": name, "media_type": "text/csv", "sha256": hashlib.sha256(text.encode()).hexdigest()})
    if listed:
        files["outputs/artifacts.json"] = json.dumps({"schema_version": 1, "files": listed})
    reply = None
    if turn.reply is not None:
        reply = root / "replies" / f"{number}.txt"
        reply.parent.mkdir(parents=True, exist_ok=True)
        reply.write_text(turn.reply, encoding="utf-8")
    return {
        "prompt": turn.prompt,
        "say": turn.say,
        "tools": [{"kind": kind, "title": title} for kind, title in turn.tools],
        "files": files,
        "reply": str(reply) if reply else None,
    }


def write_script(root: Path) -> Path:
    turns = [b.turn for b in story.BRANCHES] + [t for s in story.SESSIONS for t in s.turns]
    script = root / "script.json"
    scenes = [scene(turn, root, i) for i, turn in enumerate(turns)]
    script.write_text(json.dumps({"scenes": scenes}, indent=1), encoding="utf-8")
    return script


def server_env(root: Path) -> dict[str, str]:
    """This environment without Branchyard's or Git's own variables or the user's configuration,
    so a run inside a Branchyard turn or a Git hook acts on the demo alone, with the demo's Git
    identity and a home of its own."""
    env = {k: v for k, v in os.environ.items() if not k.startswith(("BRANCHYARD_", "GIT_")) and k != "XDG_CONFIG_HOME"}
    env.update({name: os.environ[name] for name in PREVIEW})
    (root / "home").mkdir()
    return dict(env, HOME=str(root / "home"), GIT_CEILING_DIRECTORIES=str(root), **DEMO_IDENTITY)


@contextmanager
def serve(by: Path, agent: Path, root: Path, env: dict[str, str]) -> Iterator[Api]:
    """`by serve --app --knowledge` on the demo repository, with the scripted agent as its harness."""
    repo = build_repo(root, env)
    script = write_script(root)
    token = secrets.token_hex(16)
    config = root / "server.json"
    config.write_text(
        json.dumps(
            {
                "listen": "127.0.0.1:0",
                "data_dir": str(root / "server"),
                "repos": {story.REPO: str(repo)},
                "tokens": [{"name": "readme", "token": token}],
                "app": {"push": False},
                "shutdown_grace_seconds": 5,
                "harness_commands": {
                    HARNESS: [sys.executable, "-I", str(HERE / "scripted_agent.py"), str(script), "--", str(agent)]
                },
            }
        ),
        encoding="utf-8",
    )
    config.chmod(0o600)
    log = (root / "serve.log").open("wb")
    command = [str(by), "serve", "-c", str(config), "--app", "--knowledge", "--quiet"]
    server = subprocess.Popen(command, cwd=root, env=env, stdout=subprocess.PIPE, stderr=log)
    try:
        assert server.stdout is not None
        line = server.stdout.readline().decode()
        found = re.match(r"listening on (http://\S+)", line)
        if not found:
            raise Failed(f"by serve did not start: {line!r}")
        yield Api(found.group(1), token)
    finally:
        server.terminate()
        try:
            server.wait(timeout=30)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait()
        log.close()


def listed(api: Api, name: str) -> dict[str, Any] | None:
    branches = api.call("GET", f"/v1/repos/{story.REPO}/branches")["branches"]
    return next((b for b in branches if b["name"] == name), None)


def state_is(api: Api, name: str, state: str) -> Callable[[], bool]:
    passing = MOVING | ({"ready"} if state == "merged" else set())

    def check() -> bool:
        now = (listed(api, name) or {"status": {"state": None}})["status"]["state"]
        if now != state and now not in passing:
            raise Failed(f"branch {name} is {now}, not {state}")
        return now == state

    return check


def tools_recorded(api: Api, b: story.Branch) -> Callable[[], bool]:
    def check() -> bool:
        page = api.call("GET", f"/v1/repos/{story.REPO}/branches/{b.name}/event-page?limit=200")
        started = [e for e in page["events"] if e["activity"].get("harness", {}).get("type") == "tool_started"]
        return len(started) >= len(b.turn.tools)

    return check


def merge(api: Api, b: story.Branch) -> None:
    until(f"{b.name} to be ready", state_is(api, b.name, "ready"))
    api.finished(api.call("POST", f"/v1/repos/{story.REPO}/branches/{b.name}/merge", {}))
    until(f"{b.name} to be merged", state_is(api, b.name, "merged"))


def interrupt(api: Api, b: story.Branch) -> None:
    until(f"{b.name}'s tool calls", tools_recorded(api, b))
    api.call("POST", f"/v1/repos/{story.REPO}/branches/{b.name}/cancel", {})
    until(f"{b.name} to be interrupted", state_is(api, b.name, "interrupted"))


def keep_running(api: Api, b: story.Branch) -> None:
    until(f"{b.name}'s tool calls", tools_recorded(api, b))


def ready(api: Api, b: story.Branch) -> None:
    until(f"{b.name} to be ready", state_is(api, b.name, "ready"))


FATES: dict[story.Fate, Callable[[Api, story.Branch], None]] = {
    "merged": merge,
    "interrupted": interrupt,
    "running": keep_running,
    "ready": ready,
}


def play_branches(api: Api) -> None:
    for b in story.BRANCHES:
        api.call("POST", f"/v1/repos/{story.REPO}/tasks", {"prompt": b.turn.prompt, "harness": HARNESS, "name": b.name})
        until(f"{b.name} to start", partial(listed, api, b.name))
        FATES[b.fate](api, b)


def play_workspace(api: Api) -> dict[str, str]:
    """Run the knowledge project, and return what shoot.js opens: the workspace, the session and
    the output to preview."""
    project = api.call(
        "POST", "/v1/projects", {"name": story.PROJECT, "source": {"kind": "knowledge"}, "result_mode": "artifacts"}
    )
    created = api.finished(api.call("POST", "/v1/workspaces", {"project_id": project["id"], "title": story.WORKSPACE}))
    ws = f"/v1/workspaces/{created['workspace_id']}"
    sources = []
    for title, text in story.SOURCES.items():
        headers = {
            "Content-Type": "text/csv",
            "X-Source-Title": title,
            "X-Workspace-Revision": str(api.call("GET", ws)["revision"]),
        }
        sources.append(api.call("POST", f"{ws}/sources", raw=text.encode(), headers=headers)["id"])
    opened = ""
    for s in story.SESSIONS:
        first, *rest = s.turns
        body: dict[str, Any] = {
            "name": s.name,
            "prompt": first.prompt,
            "harness": HARNESS,
            "access": "writer",
            "sources": sources,
        }
        if s.collect:
            body["result_contract"] = OUTPUT_CONTRACT
        op = api.finished(api.call("POST", f"{ws}/sessions", body))
        for turn in rest:
            send = {"prompt": turn.prompt, "action": "send"}
            api.finished(api.call("POST", f"{ws}/sessions/{op['session_id']}/messages", send))
        if s.name == story.OPEN_SESSION:
            opened = op["session_id"]

    name, number = story.OPEN_OUTPUT

    def output() -> str | None:
        published = api.call("GET", f"{ws}/artifacts")["artifacts"]
        found = [a["id"] for a in published if a["name"] == name and a["turn"] == number]
        return found[0] if found else None

    artifact = until(f"{name} from turn {number}", output)
    return {"workspace": created["workspace_id"], "session": opened, "output": artifact}


def first_line(reply: str | None) -> str:
    return (reply or "").splitlines()[0]


def playwright(given: str | None) -> str:
    if given:
        return given
    root = subprocess.run(["npm", "root", "-g"], capture_output=True, text=True, check=True).stdout.strip()
    return str(Path(root) / "playwright")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--by", type=Path, default=os.environ.get("BY"), required="BY" not in os.environ)
    parser.add_argument(
        "--agent", type=Path, default=os.environ.get("FAKE_ACP_AGENT"), required="FAKE_ACP_AGENT" not in os.environ
    )
    parser.add_argument("--out", type=Path, default=ROOT / "assets" / "screenshots")
    parser.add_argument("--playwright", default=os.environ.get("PLAYWRIGHT"), help="Playwright's module directory")
    parser.add_argument("--work", type=Path, help="Where to make the temporary directory (default: the system's)")
    parser.add_argument("--keep", action="store_true", help="Keep the temporary directory and print it")
    args = parser.parse_args(argv)
    missing = [name for name in PREVIEW if name not in os.environ]
    if missing:
        parser.error(f"set {' and '.join(missing)}: the workspace screenshot shows a table preview")
    root = Path(tempfile.mkdtemp(prefix="by-readme-", dir=args.work))
    shots = root / "shots"
    shots.mkdir()
    try:
        env = server_env(root)
        with serve(args.by.resolve(), args.agent.resolve(), root, env) as api:
            play_branches(api)
            branch = next(b for b in story.BRANCHES if b.name == story.OPEN_BRANCH)
            session = next(s for s in story.SESSIONS if s.name == story.OPEN_SESSION)
            plan = dict(
                play_workspace(api),
                repo=story.REPO,
                branch=story.OPEN_BRANCH,
                branchSays=first_line(branch.turn.reply),
                sessionSays=first_line(session.turns[-1].reply),
            )
            # No screenshot may show the demo's paths, this machine's home or name, or the token.
            forbidden = [str(root), str(Path.home()), socket.gethostname(), api.token]
            shoot = dict(
                os.environ,
                PLAYWRIGHT=playwright(args.playwright),
                BY_URL=api.url,
                BY_TOKEN=api.token,
                BY_PLAN=json.dumps(plan),
                BY_FORBIDDEN=json.dumps(forbidden),
                BY_OUT=str(shots),
            )
            subprocess.run(["node", str(HERE / "shoot.js")], env=shoot, check=True)
    except (Failed, subprocess.CalledProcessError) as error:
        print(f"generate.py: {error}\ngenerate.py: kept {root}, with the server's log", file=sys.stderr)
        return 1
    args.out.mkdir(parents=True, exist_ok=True)
    for png in sorted(shots.glob("*.png")):
        shutil.copyfile(png, args.out / png.name)
    if args.keep:
        print(f"kept {root}", file=sys.stderr)
    else:
        shutil.rmtree(root, ignore_errors=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
