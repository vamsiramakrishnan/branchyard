#!/usr/bin/env python3
"""Skill-discovery eval: does an agent find and use the right skill, connector operation or helper through
the mounted tool tree (docs/tools-model.md section 3)? See docs/skill-eval.md.

    python3 tools/skill_eval.py list [--scenarios GLOB]             validate and list the scenarios
    python3 tools/skill_eval.py score LOGDIR [--out DIR]            score recorded logs, no model
    python3 tools/skill_eval.py run --harness ID [--model M] ...    run each scenario with `by run`, then score
    python3 tools/skill_eval.py sizes [--tree DIR]                  the static text a turn pays for
    python3 tools/skill_eval.py rank [--tree DIR] [--plain]         rank each prompt with bin/tools, no model

`run` starts a real harness with a real model unless `--command` points at a fake agent. A person runs it;
no test does. `score`, `list`, `sizes` and `rank` read files only.

A LOGDIR holds, per scenario ID, `ID.log.json` (the output of `by log BRANCH --json`: a JSON array, or one
object per line) and optionally `ID.meta.json` (`harness`, `model`, `wall_s`, `checks`), which `run` writes.
Python 3.11+, standard library only; `run` also needs the artifact toolkit's libraries (python-docx, openpyxl,
python-pptx, reportlab; plugins/branchyard/skills/artifact-tools/scripts/requirements.lock) to generate the
binary input files some scenarios name, and skips a scenario whose input it cannot generate.
"""

from __future__ import annotations

import argparse
import contextlib
import fnmatch
import importlib.util
import io
import json
import os
import re
import shlex
import statistics
import subprocess
import sys
import tempfile
import tomllib
import zipfile
from collections.abc import Iterable, Iterator
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
SCENARIOS = ROOT / "evals/skills/scenarios"
RECORDS = ROOT / "evals/skills/records"
SKILLS = ROOT / "plugins/branchyard/skills"
BUNDLES = ROOT / "tests/fixtures/anvil/bundles"
CATALOG = ROOT / "catalog/harnesses.toml"
LEAF = ROOT / "plugins/branchyard/leaf.md"
CAPABILITIES = ROOT / "plugins/branchyard/capabilities.json"
TOOLS_SCRIPT = SKILLS / "find-tools/scripts/tools.py"
ANVIL_MOCK = ROOT / "tests/fixtures/anvil/anvil"
#: What separates a skill from one of its references in an id: `pptx/references/layouts.md`.
REFERENCE = "/references/"

#: The keys a scenario may have; anything else is a typo and is refused.
KEYS = {
    "id",
    "area",
    "prompt",
    "files",
    "skills",
    "connectors",
    "helpers",
    "envelope",
    "expect_skills",
    "expect_operations",
    "expect_helpers",
    "must_not",
    "none",
    "check_files",
    "check_matches",
    "check_commands",
    "fixtures",
    "check_archive",
}
#: The discovery skill itself: reading it is how a turn learns to search, never a wrong open.
NEUTRAL_SKILLS = {"find-tools"}
#: Programs whose arguments are not a read of the file they name.
NOT_A_READ = {"ls", "find", "tree", "echo", "printf", "mkdir", "rm", "touch", "cp", "mv", "chmod", "test", "[", "stat"}
NOT_A_READ |= {"wc", "du", "git", "cd", "by"}
WRITE_TOOLS = ("write", "edit", "patch", "delete", "create", "move", "remove")
PATH_KEYS = {"file_path", "path", "paths", "file", "files", "filepath", "filename", "target", "uri", "notebook_path"}
COMMAND_KEYS = {"command", "cmd", "script", "shell", "commandline", "command_line"}
DOC_PATH = re.compile(r"[\w./~${}@:+-]*\b(?:SKILL|INDEX)\.md")
SEGMENTS = re.compile(r"\n|;|&&|\|\||\|")


@dataclass
class Scenario:
    """One task: a prompt, what the turn was given and what finding it looks like."""

    id: str
    area: str
    prompt: str
    files: dict[str, str] = field(default_factory=dict)
    skills: list[str] = field(default_factory=list)
    connectors: list[str] = field(default_factory=list)
    helpers: bool = False
    envelope: list[str] = field(default_factory=list)
    expect_skills: list[str] = field(default_factory=list)
    expect_operations: list[str] = field(default_factory=list)
    expect_helpers: list[str] = field(default_factory=list)
    must_not: list[str] = field(default_factory=list)
    none: bool = False
    check_files: list[str] = field(default_factory=list)
    check_matches: dict[str, str] = field(default_factory=dict)
    check_commands: list[str] = field(default_factory=list)
    fixtures: list[str] = field(default_factory=list)
    check_archive: dict[str, str] = field(default_factory=dict)
    source: str = ""

    @property
    def expected(self) -> set[str]:
        return {*self.expect_skills, *self.expect_operations, *self.expect_helpers}

    @property
    def has_checks(self) -> bool:
        return bool(self.check_files or self.check_matches or self.check_archive)


# ---------------------------------------------------------------------------------------------------------
# Scenarios and the universe they must stay inside


def load_scenarios(pattern: str = "*") -> list[Scenario]:
    """Every scenario of the files in evals/skills/scenarios whose file stem or scenario id matches `pattern`."""
    found: list[Scenario] = []
    for path in sorted(SCENARIOS.glob("*.toml")):
        data = tomllib.loads(path.read_text(encoding="utf-8"))
        extra = set(data) - {"scenario"}
        if extra:
            raise ValueError(f"{path.name}: unknown top-level keys {sorted(extra)}")
        for raw in data.get("scenario", []):
            unknown = set(raw) - KEYS
            if unknown:
                raise ValueError(f"{path.name}: scenario {raw.get('id')!r} has unknown keys {sorted(unknown)}")
            found.append(Scenario(source=path.name, **raw))
    return [s for s in found if match_scenario(s, pattern)]


def match_scenario(scenario: Scenario, pattern: str) -> bool:
    """True when `pattern` (comma-separated globs) matches the scenario id, its area or its file's stem."""
    stem = Path(scenario.source).stem
    return any(
        fnmatch.fnmatch(name, glob)
        for glob in pattern.split(",")
        for name in (scenario.id, scenario.area, stem, f"{stem}/{scenario.id}")
    )


def built_in_skills() -> set[str]:
    return {p.parent.name for p in SKILLS.glob("*/SKILL.md")}


def tree_skills() -> set[str]:
    """The skills a turn's tool tree lists: those the topic indexes link, plus find-tools and artifact-tools."""
    names = set()
    for index in (SKILLS / "topics").glob("*/INDEX.md"):
        names |= set(re.findall(r"\]\(\.\./\.\./([^/]+)/INDEX\.md\)", index.read_text(encoding="utf-8")))
    return names


def mock_operations() -> dict[str, set[str]]:
    """Connector id -> its operation ids, as the Anvil mock's bundles define them."""
    out: dict[str, set[str]] = {}
    for air in sorted(BUNDLES.glob("*/air.json")):
        data = json.loads(air.read_text(encoding="utf-8"))
        out[data["service"]["id"]] = {op["id"] for op in data.get("operations", [])}
    return out


def helper_ids() -> set[str]:
    data = tomllib.loads(CATALOG.read_text(encoding="utf-8"))
    return {h["id"] for h in data.get("harness", [])}


def grant_connector(grant: str) -> str:
    return grant.split(":", 1)[0].split("@", 1)[0]


def reference_skill(ident: str) -> str:
    """The skill an `expect_skills` id belongs to: itself, or the skill before `/references/`."""
    return ident.split(REFERENCE, 1)[0]


def reference_problem(ident: str) -> str | None:
    """Why a reference id (`SKILL/references/FILE.md`) names no shipped reference, or None when it does."""
    skill, _, file = ident.partition(REFERENCE)
    if not re.fullmatch(r"[A-Za-z0-9_-]+\.md", file):
        return f"reference {ident!r} must be SKILL/references/FILE.md"
    if not (SKILLS / skill / "references" / file).is_file():
        return f"reference {ident!r} is not a file of the skill"
    return None


def problems(scenarios: Iterable[Scenario]) -> list[str]:
    """Why a scenario cannot be run or scored: the rot check. Empty when every one is sound."""
    skills, ops, helpers = tree_skills(), mock_operations(), helper_ids()
    all_ops = {op for group in ops.values() for op in group}
    out: list[str] = []
    seen: set[str] = set()
    for s in scenarios:

        def bad(why: str, s: Scenario = s) -> None:
            out.append(f"{s.source}: {s.id}: {why}")

        if not re.fullmatch(r"[a-z0-9]+(-[a-z0-9]+)*", s.id):
            bad("id must be lower-case words joined by '-'")
        if s.id in seen:
            bad("duplicate id")
        seen.add(s.id)
        if not s.prompt.strip():
            bad("empty prompt")
        for name in (*s.expect_skills, *s.skills):
            if reference_skill(name) not in skills:
                bad(f"skill {reference_skill(name)!r} is not one a turn's tool tree lists (the topic indexes)")
            elif REFERENCE in name and (why := reference_problem(name)):
                bad(why)
        for op in (*s.expect_operations, *(m for m in s.must_not if "." in m)):
            if op not in all_ops:
                bad(f"operation {op!r} is not one the Anvil mock's bundles define")
        for helper in (*s.expect_helpers, *s.envelope):
            if helper not in helpers and helper != "own":
                bad(f"helper {helper!r} is not a harness id in catalog/harnesses.toml")
        for name in s.must_not:
            if "." not in name and name not in skills | helpers:
                bad(f"must_not {name!r} is neither a skill nor a helper")
        for grant in s.connectors:
            if grant_connector(grant) not in ops:
                bad(f"connector {grant!r} has no bundle under tests/fixtures/anvil/bundles")
        for op in s.expect_operations:
            if not any(grant_connector(g) == op.split(".", 1)[0] for g in s.connectors):
                bad(f"operation {op!r} needs its connector in `connectors`")
        for name in s.expect_skills:
            if s.skills and reference_skill(name) not in s.skills:
                bad(f"expected skill {reference_skill(name)!r} is not among the skills chosen for the turn")
        for helper in s.expect_helpers:
            if not s.helpers:
                bad(f"expected helper {helper!r} needs `helpers = true`")
        if s.none and (s.expected or s.must_not):
            bad("a `none` scenario expects nothing")
        if not s.none and not s.expected:
            bad("expects nothing: set an expectation or `none = true`")
        for rel in (*s.files, *s.fixtures, *s.check_files, *s.check_matches, *s.check_archive):
            if rel.startswith("/") or ".." in Path(rel).parts:
                bad(f"path {rel!r} must stay inside the working tree")
        for rel in s.fixtures:
            if Path(rel).suffix.lower() not in FIXTURE_SUFFIXES or rel in s.files:
                bad(f"fixture {rel!r} must be a docx, xlsx, pptx, pdf or csv name that `files` does not seed")
            elif rel not in FIXTURE_BUILDERS:
                bad(f"fixture {rel!r} has no generator in tools/skill_eval.py (FIXTURE_BUILDERS)")
        named = set(re.findall(r"[A-Za-z0-9_.-]+\.(?:docx|xlsx|pptx|pdf|csv)\b", s.prompt))
        for rel in sorted(named - {*s.files, *s.fixtures, *s.check_files}):
            bad(f"the prompt names {rel!r}, which is neither seeded, generated nor an expected output")
        for rel in s.check_archive:
            if rel not in (*s.fixtures, *s.files, *s.check_files):
                bad(f"check_archive file {rel!r} is neither an input nor an expected output")
        for rx in (*s.check_matches.values(), *s.check_archive.values(), *s.check_commands):
            try:
                re.compile(rx)
            except re.error as error:
                bad(f"regular expression {rx!r}: {error}")
    return out


# ---------------------------------------------------------------------------------------------------------
# Reading a log


def load_events(path: Path) -> list[dict[str, Any]]:
    """The events of `by log BRANCH --json`: a JSON array, or one object per line (`--follow --json`)."""
    text = path.read_text(encoding="utf-8")
    try:
        data = json.loads(text)
    except json.JSONDecodeError:
        data = [json.loads(line) for line in text.splitlines() if line.strip()]
    if isinstance(data, dict):
        data = data.get("events", [data])
    if not isinstance(data, list) or not all(isinstance(e, dict) for e in data):
        raise ValueError(f"{path}: not a list of events")
    return data


@dataclass
class Action:
    """One thing the agent did that the log shows."""

    kind: str  # command | read | skill | connector_call | delegation
    text: str
    source: str  # target (tool_started carries what it acted on) | tool (permission input) | narrated | engine
    at_ms: int = 0


def strings(value: Any, keys: set[str]) -> Iterator[str]:
    """Every string under a key in `keys`, anywhere in `value`, a list of strings included."""
    if isinstance(value, dict):
        for key, item in value.items():
            if key.lower() in keys:
                yield from flatten(item)
            else:
                yield from strings(item, keys)
    elif isinstance(value, list):
        for item in value:
            yield from strings(item, keys)


def flatten(value: Any) -> Iterator[str]:
    if isinstance(value, str):
        yield value
    elif isinstance(value, list):
        for item in value:
            yield from flatten(item)


def actions(events: list[dict[str, Any]]) -> tuple[list[Action], dict[str, int]]:
    """The agent's actions in log order, and how much of its work the log can show at all."""
    out: list[Action] = []
    seen = {"tool_calls": 0, "visible_inputs": 0, "narrated": 0, "targets": 0}
    pending: list[Action] | None = None  # the permission request a following decision may refuse
    text: list[str] = []
    text_at = 0

    def flush() -> None:
        nonlocal text
        for line in "".join(text).splitlines():
            stripped = line.strip()
            if stripped.startswith("$ "):
                out.append(Action("command", stripped[2:], "narrated", text_at))
                seen["narrated"] += 1
        text = []

    for event in events:
        at = int(event.get("at_ms") or 0)
        activity = event.get("activity")
        inner = event.get("event") if activity == "harness" else None
        kind = inner.get("type") if isinstance(inner, dict) else None
        if kind != "message_delta":
            flush()
        if kind == "message_delta" and isinstance(inner, dict):
            if not text:
                text_at = at
            text.append(str(inner.get("text", "")))
        elif kind == "tool_started" and isinstance(inner, dict):
            seen["tool_calls"] += 1
            pending = None
            found = target_actions(inner, at)
            seen["targets"] += 1 if isinstance(inner.get("target"), dict) else 0
            out.extend(found)
        elif kind == "permission_requested" and isinstance(inner, dict):
            pending = permission_actions(inner, at)
            seen["visible_inputs"] += 1 if pending else 0
            out.extend(pending)
        elif activity == "decision":
            if pending and event.get("allowed") is False:
                for action in pending:
                    out.remove(action)
                    # The same call's target, recorded when the tool started.
                    twin = [a for a in out if a.source == "target" and (a.kind, a.text) == (action.kind, action.text)]
                    if twin:
                        out.remove(twin[-1])
            pending = None
        elif activity == "connector_call":
            call = event.get("connector_call") or {}
            op = f"{call.get('connector', '')}.{call.get('operation', '')}"
            out.append(Action("connector_call", f"{op} {call.get('decision', '')}".strip(), "engine", at))
        elif activity == "delegation":
            out.append(Action("delegation", f"{event.get('tool', '')} {event.get('branch', '')}".strip(), "engine", at))
    flush()
    return out, seen


def target_actions(inner: dict[str, Any], at: int) -> list[Action]:
    """What a `tool_started` event's `target` says the call acted on: `{kind, value}` with kind file, command,
    mcp or skill, bounded and scrubbed by the engine. A file target is a read unless the tool's name says it
    writes. No target: nothing is known of the call's input."""
    target = inner.get("target")
    if not isinstance(target, dict) or not isinstance(target.get("value"), str) or not target["value"]:
        return []
    kind, value = target.get("kind"), target["value"]
    if kind == "command":
        return [Action("command", value, "target", at)]
    if kind == "skill":
        return [Action("skill", value, "target", at)]
    if kind == "file":
        if any(w in str(inner.get("name", "")).lower() for w in WRITE_TOOLS):
            return []
        return [Action("read", value, "target", at)]
    return [Action("tool", value, "target", at)]


def permission_actions(inner: dict[str, Any], at: int) -> list[Action]:
    tool = str(inner.get("tool", ""))
    data = inner.get("input")
    found: list[Action] = []
    for command in strings(data, COMMAND_KEYS):
        found.append(Action("command", command, "tool", at))
    if tool.lower() == "skill" and isinstance(data, dict):
        name = data.get("skill") or data.get("name")
        if isinstance(name, str):
            found.append(Action("skill", name, "tool", at))
    if not any(w in tool.lower() for w in WRITE_TOOLS):
        for path in strings(data, PATH_KEYS):
            found.append(Action("read", path.removeprefix("file://"), "tool", at))
    if not found and data is not None:
        found.append(Action("tool", tool, "tool", at))
    return found


def command_segments(command: str, depth: int = 0) -> Iterator[list[str]]:
    """The word lists of each simple command in a shell line, looking inside `sh -c '...'`."""
    for part in SEGMENTS.split(command):
        try:
            words = shlex.split(part)
        except ValueError:
            words = part.split()
        while words and re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*=.*", words[0]):
            words = words[1:]
        if not words:
            continue
        if depth < 3 and Path(words[0]).name in {"sh", "bash", "zsh"} and "-c" in words[1:]:
            inner = words[words.index("-c") + 1 :]
            if inner:
                yield from command_segments(inner[0], depth + 1)
                continue
        if depth < 3 and Path(words[0]).name == "sudo":
            words = words[1:]
        if words:
            yield words


@dataclass
class Evidence:
    """What the log shows of discovery, before it is held against a scenario."""

    searched_cmd: bool = False
    read_index: bool = False
    read_helpers_index: bool = False
    skills: set[str] = field(default_factory=set)
    routed: set[str] = field(default_factory=set)
    connectors: set[str] = field(default_factory=set)
    operations: set[str] = field(default_factory=set)
    helpers: set[str] = field(default_factory=set)
    used_operations: set[str] = field(default_factory=set)
    used_helpers: set[str] = field(default_factory=set)
    commands: list[str] = field(default_factory=list)


def classify(ident: str, universe: dict[str, Any]) -> tuple[str, str]:
    """What `tools show IDENT` named: (kind, id), kind one of skill, connector, operation, helper."""
    ident = ident.strip()
    prefix, _, rest = ident.partition(":")
    if rest and prefix in {"skill", "connector", "operation", "helper"}:
        return prefix, rest
    if ident in universe["operations"]:
        return "operation", ident
    if "." in ident:
        short = [o for o in universe["operations"] if o.split(".", 1)[1] == ident]
        return ("operation", short[0]) if len(short) == 1 else ("operation", ident)
    if ident in universe["connectors"]:
        return "connector", ident
    if ident in universe["skills"]:
        return "skill", ident
    if ident in universe["helpers"]:
        return "helper", ident
    return "skill", ident


def collect(acts: list[Action], universe: dict[str, Any]) -> Evidence:
    """Fold the actions into what was searched, opened and used."""
    ev = Evidence()

    def read_path(path: str) -> None:
        normal = path.replace("\\", "/")
        if re.search(r"(^|/)connectors/([^/]+)/(SKILL\.md|reference/|schemas/|examples/)", normal):
            ev.connectors.add(re.search(r"(?:^|/)connectors/([^/]+)/", normal).group(1))  # type: ignore[union-attr]
        elif normal.endswith("helpers/INDEX.md"):
            ev.read_helpers_index = True
            ev.read_index = True
        elif normal.endswith("/SKILL.md"):
            ev.skills.add(Path(normal).parent.name)
        elif normal.endswith("INDEX.md"):
            ev.read_index = True
            parent = Path(normal).parent.name
            if parent in universe["skills"]:
                ev.routed.add(parent)

    for action in acts:
        if action.kind == "read":
            read_path(action.text)
        elif action.kind == "skill":
            ev.skills.add(action.text)
        elif action.kind == "connector_call":
            op = action.text.split()[0]
            ev.operations.add(op)
            ev.connectors.add(op.split(".", 1)[0])
            if action.text.endswith("allowed"):
                ev.used_operations.add(op)
        elif action.kind == "delegation":
            ev.commands.append(action.text)
        elif action.kind == "command":
            ev.commands.append(action.text)
            for words in command_segments(action.text):
                program = Path(words[0]).name
                if program in {"python", "python3"} and len(words) > 1:
                    words = words[1:]
                    program = Path(words[0]).name
                if program in {"tools", "tools.py"} and len(words) > 1:
                    sub, rest = words[1], [w for w in words[2:] if not w.startswith("-")]
                    if sub == "search":
                        ev.searched_cmd = True
                    elif sub in {"show", "schema"} and rest:
                        kind, ident = classify(rest[0], universe)
                        {
                            "skill": ev.skills,
                            "connector": ev.connectors,
                            "operation": ev.operations,
                            "helper": ev.helpers,
                        }[kind].add(ident)
                        if kind == "operation":
                            ev.connectors.add(ident.split(".", 1)[0])
                    continue
                if program in NOT_A_READ:
                    continue
                for match in DOC_PATH.finditer(" ".join(words)):
                    read_path(match.group(0))
                for op, cli in universe["cli"].items():
                    if re.search(rf"\b{re.escape(cli)}\b", " ".join(words)):
                        ev.operations.add(op)
                        ev.connectors.add(op.split(".", 1)[0])
    # A helper was used when the agent started a branch of that harness.
    spawned = any(a.kind == "delegation" and a.text.startswith(("spawn", "delegate")) for a in acts)
    for command in ev.commands:
        if spawned or re.search(r"\bby\s+(spawn|run|fan)\b", command):
            ev.used_helpers |= {
                h for h in universe["helpers"] if re.search(rf"(?<![\w-]){re.escape(h)}(?![\w-])", command)
            }
    return ev


def build_universe() -> dict[str, Any]:
    ops = mock_operations()
    cli = {op: " ".join(op.split(".")) for group in ops.values() for op in group}
    return {
        "skills": built_in_skills(),
        "connectors": set(ops),
        "operations": {op for group in ops.values() for op in group},
        "helpers": helper_ids(),
        "cli": {op: text for op, text in cli.items() if text.count(" ") >= 2},
    }


# ---------------------------------------------------------------------------------------------------------
# Scoring


def usage_tokens(events: list[dict[str, Any]]) -> dict[str, int | None]:
    """Tokens from `usage_observed` events: per-call observations add up; a cumulative total replaces the last."""
    totals: dict[str, int] = {"input": 0, "output": 0, "cached": 0}
    cumulative: dict[str, int] = {"input": 0, "output": 0, "cached": 0}
    seen = False
    fields = {"input": "input_tokens", "output": "output_tokens", "cached": "cached_input_tokens"}
    for event in events:
        inner = event.get("event")
        if event.get("activity") != "harness" or not isinstance(inner, dict) or inner.get("type") != "usage_observed":
            continue
        seen = True
        target = cumulative if inner.get("cumulative") else totals
        for name, key in fields.items():
            value = inner.get(key)
            if isinstance(value, int):
                target[name] = value if inner.get("cumulative") else target[name] + value
    if not seen:
        return {"input": None, "output": None, "cached": None, "total": None}
    merged = {name: totals[name] + cumulative[name] for name in totals}
    return {**merged, "total": merged["input"] + merged["output"]}


def turn_seconds(events: list[dict[str, Any]]) -> float | None:
    start = next((e["at_ms"] for e in events if e.get("activity") == "prompt" and "at_ms" in e), None)
    ends = [
        e["at_ms"]
        for e in events
        if e.get("activity") == "harness" and (e.get("event") or {}).get("type") == "turn_ended" and "at_ms" in e
    ]
    if start is None or not ends:
        return None
    return round((max(ends) - start) / 1000, 3)


def turn_completed(events: list[dict[str, Any]]) -> bool:
    outcomes = [
        (e.get("event") or {}).get("outcome") or {}
        for e in events
        if e.get("activity") == "harness" and (e.get("event") or {}).get("type") == "turn_ended"
    ]
    return bool(outcomes) and outcomes[-1].get("kind") == "completed"


def opened(scenario: Scenario, ev: Evidence) -> dict[str, set[str]]:
    """What was opened, by kind, with the discovery skill and a connector's own parent left out."""
    skills = ev.skills - (NEUTRAL_SKILLS - set(scenario.expect_skills))
    ops = set(ev.operations)
    wanted_connectors = {op.split(".", 1)[0] for op in scenario.expect_operations}
    connectors = ev.connectors - wanted_connectors - {o.split(".", 1)[0] for o in ops}
    return {"skill": skills, "operation": ops, "connector": connectors, "helper": set(ev.helpers)}


def score_scenario(
    scenario: Scenario, events: list[dict[str, Any]], meta: dict[str, Any], universe: dict[str, Any]
) -> dict[str, Any]:
    acts, seen = actions(events)
    ev = collect(acts, universe)
    opens = opened(scenario, ev)
    all_opens = {(kind, ident) for kind, group in opens.items() for ident in group}
    wanted = {("skill", s) for s in scenario.expect_skills}
    wanted |= {("operation", o) for o in scenario.expect_operations}
    own = str(meta.get("harness", ""))
    expect_helpers = [own if h == "own" else h for h in scenario.expect_helpers]
    wanted |= {("helper", h) for h in expect_helpers}

    found = {item for item in wanted if item in all_opens}
    # Reading helpers/INDEX.md shows every helper; a turn that read it has found the one it needs.
    found |= {("helper", h) for h in expect_helpers if ev.read_helpers_index}
    # A connector call is use, and use implies the operation was found.
    found |= {("operation", o) for o in scenario.expect_operations if o in ev.used_operations}
    correct = {item for item in all_opens if item in wanted}
    wrong = sorted(f"{kind}:{ident}" for kind, ident in all_opens - wanted)
    forbidden = sorted(m for m in scenario.must_not if any(ident == m for _, ident in all_opens))
    searched = ev.searched_cmd or ev.read_index
    # Targets on the tool events (docs/skill-eval.md) show inputs wherever the harness reports them; only a
    # log with tool calls and no target, no permission input and no narrated command is blind.
    blind = seen["tool_calls"] > 0 and seen["targets"] == 0 and seen["visible_inputs"] == 0 and seen["narrated"] == 0

    checks, checks_ok = check_results(scenario, meta, ev)
    completed = turn_completed(events)
    discovered_ok = not all_opens if scenario.none else found == wanted and not forbidden
    success = None if checks_ok is None else completed and discovered_ok and checks_ok
    tokens = usage_tokens(events)
    seconds = turn_seconds(events)
    if seconds is None and isinstance(meta.get("wall_s"), (int, float)):
        seconds = float(meta["wall_s"])
    return {
        "id": scenario.id,
        "area": scenario.area,
        "harness": meta.get("harness", "unknown"),
        "model": meta.get("model"),
        "none": scenario.none,
        "searched": searched,
        "searched_command": ev.searched_cmd,
        "read_index": ev.read_index,
        "expected": sorted(f"{kind}:{ident}" for kind, ident in wanted),
        "found": sorted(f"{kind}:{ident}" for kind, ident in found),
        "missed": sorted(f"{kind}:{ident}" for kind, ident in wanted - found),
        "opened": sorted(f"{kind}:{ident}" for kind, ident in all_opens),
        "wrong_opens": wrong,
        "forbidden_opened": forbidden,
        "correct_opens": len(correct),
        "total_opens": len(all_opens),
        "used": sorted([*ev.used_operations, *ev.used_helpers]),
        "tokens": tokens,
        "seconds": seconds,
        "completed": completed,
        "checks": checks,
        "success": success,
        "blind": blind,
        "evidence": seen,
    }


def check_results(scenario: Scenario, meta: dict[str, Any], ev: Evidence) -> tuple[dict[str, bool], bool | None]:
    """The scenario's success checks: files and matches from the runner's meta, commands from the log."""
    results: dict[str, bool] = {}
    recorded = meta.get("checks") or {}
    unresolved = False
    for rel in scenario.check_files:
        value = (recorded.get("files") or {}).get(rel)
        if value is None:
            unresolved = True
        else:
            results[f"file:{rel}"] = bool(value)
    for rel in scenario.check_matches:
        value = (recorded.get("matches") or {}).get(rel)
        if value is None:
            unresolved = True
        else:
            results[f"match:{rel}"] = bool(value)
    for rel in scenario.check_archive:
        value = (recorded.get("archive") or {}).get(rel)
        if value is None:
            unresolved = True
        else:
            results[f"archive:{rel}"] = bool(value)
    for rx in scenario.check_commands:
        results[f"command:{rx}"] = any(re.search(rx, c) for c in ev.commands)
    if unresolved:
        return results, None
    return results, all(results.values())


def rate(num: float, den: float) -> float | None:
    return round(num / den, 4) if den else None


def aggregate(rows: list[dict[str, Any]]) -> dict[str, Any]:
    """Discovery precision and recall, searched rates and tokens per success over `rows`."""
    needing = [r for r in rows if not r["none"]]
    nones = [r for r in rows if r["none"]]
    scored = [r for r in rows if r["success"] is not None]
    wins = [r for r in scored if r["success"]]
    tokens = [r["tokens"]["total"] for r in rows if r["tokens"]["total"] is not None]
    seconds = [r["seconds"] for r in rows if r["seconds"] is not None]
    expected = sum(len(r["expected"]) for r in needing)
    found = sum(len(r["found"]) for r in needing)
    opens = sum(r["total_opens"] for r in rows)
    correct = sum(r["correct_opens"] for r in rows)
    return {
        "scenarios": len(rows),
        "scored": len(scored),
        "successes": len(wins),
        "success_rate": rate(len(wins), len(scored)),
        "searched_rate": rate(sum(r["searched"] for r in needing), len(needing)),
        "discovery_recall": rate(found, expected),
        "discovery_precision": rate(correct, opens),
        "wrong_opens": sum(len(r["wrong_opens"]) for r in rows),
        "needless_search_rate": rate(sum(r["searched"] for r in nones), len(nones)),
        "false_opens_on_none": sum(r["total_opens"] for r in nones),
        "tokens_total": sum(tokens) if tokens else None,
        "tokens_per_success": round(sum(tokens) / len(wins)) if tokens and wins else None,
        "median_seconds": round(statistics.median(seconds), 1) if seconds else None,
        "blind": sum(r["blind"] for r in rows),
        "tokens_missing": len(rows) - len(tokens),
    }


def build_report(rows: list[dict[str, Any]], notes: list[str] | None = None) -> dict[str, Any]:
    by_harness: dict[str, list[dict[str, Any]]] = {}
    for row in rows:
        by_harness.setdefault(str(row["harness"]), []).append(row)
    return {
        "schema": "branchyard.skill-eval/v1",
        "notes": notes or [],
        "overall": aggregate(rows),
        "by_harness": {name: aggregate(group) for name, group in sorted(by_harness.items())},
        "by_area": {
            area: aggregate([r for r in rows if r["area"] == area]) for area in sorted({r["area"] for r in rows})
        },
        "scenarios": rows,
    }


def pct(value: float | None) -> str:
    return "-" if value is None else f"{value * 100:.0f}%"


def num(value: float | int | None) -> str:
    return "-" if value is None else f"{value:,}"


def markdown(report: dict[str, Any]) -> str:
    """A short table per harness, then one line per scenario."""
    lines = ["# Skill-discovery eval", ""]
    lines += [f"- {note}" for note in report["notes"]]
    if report["notes"]:
        lines.append("")
    head = "| harness | n | success | searched | recall | precision | tokens/success | median s | blind |"
    lines += [head, "|---|---|---|---|---|---|---|---|---|"]
    groups = {**report["by_harness"], "all": report["overall"]}
    for name, a in groups.items():
        lines.append(
            f"| {name} | {a['scenarios']} | {pct(a['success_rate'])} | {pct(a['searched_rate'])} | "
            f"{pct(a['discovery_recall'])} | {pct(a['discovery_precision'])} | {num(a['tokens_per_success'])} | "
            f"{num(a['median_seconds'])} | {a['blind']} |"
        )
    lines += [
        "",
        "| scenario | harness | searched | found | wrong opens | tokens | s | ok |",
        "|---|---|---|---|---|---|---|---|",
    ]
    for r in report["scenarios"]:
        found = "none needed" if r["none"] else f"{len(r['found'])}/{len(r['expected'])}"
        ok = "-" if r["success"] is None else ("yes" if r["success"] else "no")
        lines.append(
            f"| {r['id']} | {r['harness']} | {'yes' if r['searched'] else 'no'} | {found} | "
            f"{', '.join(r['wrong_opens']) or '-'} | {num(r['tokens']['total'])} | {num(r['seconds'])} | {ok} |"
        )
    return "\n".join(lines) + "\n"


def score_dir(logdir: Path, scenarios: list[Scenario]) -> dict[str, Any]:
    """Score every scenario that has a log in `logdir`; scenarios without one are noted, never counted."""
    universe = build_universe()
    rows, missing = [], []
    for scenario in scenarios:
        log = logdir / f"{scenario.id}.log.json"
        if not log.is_file():
            missing.append(scenario.id)
            continue
        meta_path = logdir / f"{scenario.id}.meta.json"
        meta = json.loads(meta_path.read_text(encoding="utf-8")) if meta_path.is_file() else {}
        rows.append(score_scenario(scenario, load_events(log), meta, universe))
    notes = []
    if missing:
        notes.append(f"{len(missing)} scenarios have no log and are not counted: {', '.join(missing)}")
    skipped = logdir / "skipped.json"
    if skipped.is_file():
        for sid, why in json.loads(skipped.read_text(encoding="utf-8")).items():
            notes.append(f"skipped {sid}: {why}")
    if any(r["blind"] for r in rows):
        notes.append(
            "blind scenarios: tool calls with no target and no input in the log, so searched and found are lower bounds"
        )
    if (logdir / "run.json").is_file():
        info = json.loads((logdir / "run.json").read_text(encoding="utf-8"))
        notes.append(f"run: {json.dumps(info, sort_keys=True)}")
        if info.get("scripted"):
            notes.append(
                f"SCRIPTED: {len(info['scripted'])} scenarios ran a script, not a model; "
                "their numbers test the pipeline and measure nothing about an agent"
            )
    return build_report(rows, notes)


# ---------------------------------------------------------------------------------------------------------
# Running


@dataclass
class RunOptions:
    harness: str
    model: str | None = None
    by: str = "by"
    command: str | None = None
    max_minutes: str = "10"
    anvil: str | None = None
    bundles: str | None = None
    script: dict[str, str] = field(default_factory=dict)


def scenario_prompt(scenario: Scenario, options: RunOptions, repo: Path) -> str:
    """The prompt: the scenario's own, or the script's with `{repo}` and `{tools}` filled in.

    `{tools}` is the glob of the one tool tree a local run mounts for the repository, outside the branch's
    worktree (`REPO/.branchyard/capabilities/HASH/.branchyard/tools`)."""
    scripted = options.script.get(scenario.id)
    if scripted is None:
        return scenario.prompt
    tools = f"{repo}/.branchyard/capabilities/*/.branchyard/tools"
    return scripted.replace("{tools}", tools).replace("{repo}", str(repo))


def run_argv(scenario: Scenario, options: RunOptions, repo: Path) -> list[str]:
    """The `by run` command line for one scenario."""
    prompt = scenario_prompt(scenario, options, repo)
    argv = [options.by, "run", prompt, "--name", scenario.id, "--harness", options.harness]
    argv += ["--yes", "--max-minutes", options.max_minutes]
    if options.model:
        argv += ["--model", options.model]
    if options.command:
        argv += ["--command", options.command]
    for skill in scenario.skills:
        argv += ["--skill", skill]
    if scenario.helpers:
        argv += ["--delegate=1"]
        if scenario.envelope:
            # The envelope allows the run's own harness and the ones the scenario needs.
            helpers = [options.harness, *(h for h in scenario.envelope if h not in {"own", options.harness})]
            argv += ["--helpers", ",".join(helpers)]
    for grant in scenario.connectors:
        argv += ["--connector", grant]
    if scenario.connectors:
        argv += ["--isolated", "true"]
    return argv


def runnable(scenario: Scenario, options: RunOptions) -> str | None:
    """Why this scenario cannot run with these options, or None."""
    for name in scenario.fixtures:
        try:
            FIXTURE_BUILDERS[name]()
        except RuntimeError as error:
            return str(error)
    if scenario.connectors and not (options.anvil and options.bundles):
        return "needs a connector gateway: pass --anvil and --bundles (the Anvil mock cannot serve one)"
    return None


# ---------------------------------------------------------------------------------------------------------
# Input files


#: The one timestamp every generated file carries, so the same name always gives the same bytes.
FIXED_TIME = datetime(2026, 11, 7, 9, 0, 0)
FIXTURE_SUFFIXES = {".docx", ".xlsx", ".pptx", ".pdf", ".csv"}


def stable_zip(data: bytes) -> bytes:
    """`data`, an OOXML zip, rewritten with one timestamp and its members in order, so it is reproducible."""
    out = io.BytesIO()
    with zipfile.ZipFile(io.BytesIO(data)) as source, zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as target:
        for info in source.infolist():
            member = zipfile.ZipInfo(info.filename, FIXED_TIME.timetuple()[:6])
            member.compress_type = zipfile.ZIP_DEFLATED
            member.external_attr = 0o644 << 16
            target.writestr(member, source.read(info.filename))
    return out.getvalue()


def need(module: str) -> Any:
    """Import one of the artifact toolkit's libraries, or say which requirements file lists it."""
    try:
        return __import__(module, fromlist=["_"])
    except ImportError as error:
        raise RuntimeError(
            f"{module} is needed to generate input files: pip install -r "
            "plugins/branchyard/skills/artifact-tools/scripts/requirements.lock"
        ) from error


def docx_bytes(
    title: str, paragraphs: list[str], tables: list[list[list[str]]] | None = None, page_breaks: bool = False
) -> bytes:
    docx = need("docx")
    doc = docx.Document()
    doc.core_properties.created = doc.core_properties.modified = FIXED_TIME
    doc.core_properties.author = doc.core_properties.last_modified_by = "Skill Eval"
    doc.add_heading(title, level=1)
    for text in paragraphs:
        run = doc.add_paragraph().add_run(text)
        run.bold = text.endswith(":")
        if page_breaks:
            doc.add_page_break()
    for rows in tables or []:
        table = doc.add_table(rows=len(rows), cols=len(rows[0]))
        table.style = "Table Grid"
        for r, row in enumerate(rows):
            for c, value in enumerate(row):
                table.cell(r, c).text = value
        doc.add_paragraph()
    out = io.BytesIO()
    doc.save(out)
    return stable_zip(out.getvalue())


def xlsx_bytes(sheet: str, rows: list[list[Any]]) -> bytes:
    openpyxl = need("openpyxl")
    book = openpyxl.Workbook()
    book.properties.created = book.properties.modified = FIXED_TIME
    book.properties.creator = "Skill Eval"
    page = book.active
    page.title = sheet
    for row in rows:
        page.append(row)
    out = io.BytesIO()
    book.save(out)
    return stable_zip(out.getvalue())


def pptx_bytes(slides: list[tuple[str, str]]) -> bytes:
    pptx = need("pptx")
    deck = pptx.Presentation()
    deck.core_properties.created = deck.core_properties.modified = FIXED_TIME
    deck.core_properties.author = deck.core_properties.last_modified_by = "Skill Eval"
    for title, body in slides:
        slide = deck.slides.add_slide(deck.slide_layouts[1])
        slide.shapes.title.text = title
        slide.placeholders[1].text = body
    out = io.BytesIO()
    deck.save(out)
    return stable_zip(out.getvalue())


def pdf_bytes(pages: list[list[str]], form: list[str] | None = None) -> bytes:
    """A PDF with one page per list of lines, and a text field per name in `form` on its first page."""
    canvas = need("reportlab.pdfgen.canvas")
    out = io.BytesIO()
    page = canvas.Canvas(out, invariant=1, pageCompression=0)
    page.setTitle("Skill Eval fixture")
    for number, lines in enumerate(pages):
        y = 760
        for line in lines:
            page.drawString(72, y, line)
            y -= 18
        if number == 0:
            for name in form or []:
                page.acroForm.textfield(name=name, x=200, y=y, width=200, height=18, borderStyle="inset")
                y -= 30
        page.showPage()
    page.save()
    return out.getvalue()


def csv_bytes(rows: list[list[Any]]) -> bytes:
    return "".join(",".join(str(v) for v in row) + "\n" for row in rows).encode("utf-8")


def forecast_rows() -> list[list[Any]]:
    """Quarterly figures in B to D with the totals still summing B and C only, as after an inserted column."""
    rows: list[list[Any]] = [["Item", "Q1", "Q2", "Q3", "Total"]]
    for n, item in enumerate(["Licences", "Support", "Training"], start=2):
        rows.append([item, 100 * n, 110 * n, 120 * n, f"=SUM(B{n}:C{n})"])
    rows.append(["All", "=SUM(B2:B4)", "=SUM(C2:C4)", "=SUM(D2:D4)", "=SUM(E2:E4)"])
    return rows


def sales_rows() -> list[list[Any]]:
    regions, products = ["North", "South", "East"], ["Pens", "Ink"]
    rows: list[list[Any]] = [["region", "product", "amount"]]
    for i, region in enumerate(regions):
        for j, product in enumerate(products):
            rows.append([region, product, 10 * (i + 1) + j])
    return rows


#: The input file names a scenario may list in `fixtures`, each with a deterministic generator.
FIXTURE_BUILDERS: dict[str, Any] = {
    "contract.docx": lambda: docx_bytes(
        "Services Agreement",
        [
            "This agreement is between Acme Ltd and the Customer.",
            "Acme Ltd shall deliver the services described in Schedule A.",
            "Payment terms:",
            "Invoices are payable to Acme Ltd within thirty days.",
        ],
        [[["Party", "Role"], ["Acme Ltd", "Supplier"]]],
    ),
    "flyer.docx": lambda: docx_bytes(
        "Spring Open Day",
        ["Join us on Saturday, 14 March, from 10:00 to 16:00.", "Free entry. Tours every hour.", "Bring a friend."],
    ),
    "handbook.docx": lambda: docx_bytes(
        "Team Handbook",
        ["Welcome to the team.", "Working hours and holidays.", "Expenses and travel."],
        page_breaks=True,
    ),
    "spec.docx": lambda: docx_bytes(
        "Interface Specification",
        ["Three tables follow."],
        [
            [["Field", "Type"], ["id", "integer"], ["name", "text"]],
            [["Code", "Meaning"], ["200", "ok"], ["404", "missing"]],
            [["Limit", "Value"], ["rate", "10 per second"], ["size", "1 MB"]],
        ],
    ),
    "sales.xlsx": lambda: xlsx_bytes("Sales", sales_rows()),
    "forecast.xlsx": lambda: xlsx_bytes("Forecast", forecast_rows()),
    "pitch.pptx": lambda: pptx_bytes(
        [
            ("Pitch", "Our company"),
            ("Why Contoso", "Contoso needs faster reports"),
            ("Product", "What we built"),
            ("Pricing", "Per seat"),
            ("Next steps", "Contoso pilot in March"),
        ]
    ),
    "keynote.pptx": lambda: pptx_bytes(
        [("Opening", "Welcome and agenda"), ("Results", "Revenue grew"), ("Close", "Questions")]
    ),
    "part1.pdf": lambda: pdf_bytes([["Part one, page one"], ["Part one, page two"]]),
    "part2.pdf": lambda: pdf_bytes([["Part two, page one"]]),
    "report.pdf": lambda: pdf_bytes([[f"Quarterly report, page {n}", "Revenue and costs."] for n in (1, 2, 3)]),
    "application.pdf": lambda: pdf_bytes([["Application form", "Name:", "Date:"]], form=["name", "date"]),
    "filing.pdf": lambda: pdf_bytes([["Annual filing", "Registered office: 1 High Street."]]),
    "vendor.pdf": lambda: pdf_bytes(
        [["Pricing", "Item | Unit | Price", "Pen | each | 1.20"], ["Pricing (continued)", "Ink | bottle | 4.50"]]
    ),
}


def seed(repo: Path, scenario: Scenario) -> None:
    """Write the scenario's text files and generate its binary inputs."""
    for rel, content in scenario.files.items():
        target = repo / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8")
    for rel in scenario.fixtures:
        target = repo / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(FIXTURE_BUILDERS[rel]())


def git(repo: Path, *args: str, env: dict[str, str] | None = None) -> None:
    """Run git in `repo`, with `env` (the scenario's private HOME and identity) when given."""
    subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True, text=True, env=env)


def sandbox_env(base: Path) -> dict[str, str]:
    """A private HOME and git identity, so a run touches nothing of the person's."""
    home = base / "home"
    home.mkdir(parents=True, exist_ok=True)
    env = {k: v for k, v in os.environ.items() if k in {"PATH", "LANG", "LC_ALL", "TERM"} or k.endswith("_API_KEY")}
    env.update(
        {
            "HOME": str(home),
            "XDG_CONFIG_HOME": str(home / ".config"),
            "XDG_DATA_HOME": str(home / ".local/share"),
            "XDG_CACHE_HOME": str(home / ".cache"),
            "TMPDIR": str(base),
            "GIT_AUTHOR_NAME": "Skill Eval",
            "GIT_AUTHOR_EMAIL": "skill-eval@example.com",
            "GIT_COMMITTER_NAME": "Skill Eval",
            "GIT_COMMITTER_EMAIL": "skill-eval@example.com",
            "GIT_CONFIG_NOSYSTEM": "1",
            "NO_COLOR": "1",
        }
    )
    return env


def evaluate_checks(scenario: Scenario, tree: Path) -> dict[str, dict[str, bool]]:
    """The file checks, run against the finished branch's worktree."""
    files = {rel: (tree / rel).is_file() for rel in scenario.check_files}
    matches = {}
    for rel, rx in scenario.check_matches.items():
        path = tree / rel
        matches[rel] = path.is_file() and re.search(rx, path.read_text(encoding="utf-8", errors="replace")) is not None
    archive = {rel: archive_matches(tree / rel, rx) for rel, rx in scenario.check_archive.items()}
    return {"files": files, "matches": matches, "archive": archive}


def archive_matches(path: Path, pattern: str) -> bool:
    """Whether `pattern` matches a member name or the text of a member of the zip (docx, xlsx, pptx) at `path`."""
    try:
        with zipfile.ZipFile(path) as bundle:
            for name in bundle.namelist():
                if re.search(pattern, name) or re.search(pattern, bundle.read(name).decode("utf-8", "replace")):
                    return True
    except (OSError, zipfile.BadZipFile):
        return False
    return False


def worktree_of(by: str, name: str, env: dict[str, str], repo: Path) -> Path | None:
    done = subprocess.run([by, "show", name, "--json"], cwd=repo, env=env, capture_output=True, text=True, check=False)
    if done.returncode != 0:
        return None
    try:
        data = json.loads(done.stdout)
    except json.JSONDecodeError:
        return None
    for key in ("worktree", "path", "dir"):
        if isinstance(data.get(key), str):
            return Path(data[key])
    return None


def run_scenario(scenario: Scenario, options: RunOptions, out: Path, base: Path) -> dict[str, Any]:
    """Run one scenario in a fresh repository and write its log and meta to `out`."""
    work = base / scenario.id
    repo = work / "repo"
    repo.mkdir(parents=True)
    env = sandbox_env(work)
    git(repo, "init", "-q", "-b", "main", env=env)
    (repo / "README.md").write_text("# scratch\n", encoding="utf-8")
    seed(repo, scenario)
    if scenario.connectors and options.bundles and options.anvil:
        (repo / "branchyard.toml").write_text(
            f'[connectors]\nbundles = "{options.bundles}"\nanvil = "{options.anvil}"\n', encoding="utf-8"
        )
    git(repo, "add", "-A", env=env)
    git(repo, "commit", "-q", "-m", "scratch", env=env)
    started = datetime.now(UTC)
    clock = os.times().elapsed
    if scenario.connectors:
        subprocess.run([options.by, "gateway", "start"], cwd=repo, env=env, check=False, capture_output=True)
    done = subprocess.run(
        run_argv(scenario, options, repo), cwd=repo, env=env, capture_output=True, text=True, check=False, timeout=3600
    )
    wall = round(os.times().elapsed - clock, 2)
    log = subprocess.run(
        [options.by, "log", scenario.id, "--json"], cwd=repo, env=env, capture_output=True, text=True, check=False
    )
    (out / f"{scenario.id}.log.json").write_text(log.stdout or "[]", encoding="utf-8")
    tree = worktree_of(options.by, scenario.id, env, repo)
    meta = {
        "scenario": scenario.id,
        "harness": options.harness,
        "model": options.model,
        "started": started.isoformat(timespec="seconds"),
        "wall_s": wall,
        "exit_status": done.returncode,
        "scripted": scenario.id in options.script,
        "skills_chosen_applied": bool(scenario.skills),
        "checks": evaluate_checks(scenario, tree) if tree else {},
        "stderr_tail": done.stderr[-2000:],
    }
    (out / f"{scenario.id}.meta.json").write_text(json.dumps(meta, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    if scenario.connectors:
        subprocess.run([options.by, "gateway", "stop"], cwd=repo, env=env, check=False, capture_output=True)
    return meta


def run_all(scenarios: list[Scenario], options: RunOptions, out: Path) -> dict[str, Any]:
    out.mkdir(parents=True, exist_ok=True)
    skipped: dict[str, str] = {}
    with tempfile.TemporaryDirectory(prefix="skill-eval-") as scratch:
        for scenario in scenarios:
            why = runnable(scenario, options)
            if why:
                skipped[scenario.id] = why
                continue
            print(f"skill-eval: {scenario.id}", file=sys.stderr)
            run_scenario(scenario, options, out, Path(scratch))
    (out / "skipped.json").write_text(json.dumps(skipped, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    info = {
        "harness": options.harness,
        "model": options.model,
        "at": datetime.now(UTC).isoformat(timespec="seconds"),
        "command": options.command,
        "scripted": sorted(options.script),
    }
    (out / "run.json").write_text(json.dumps(info, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return info


# ---------------------------------------------------------------------------------------------------------
# Static sizes


def file_size(path: Path) -> dict[str, int] | None:
    if not path.is_file():
        return None
    data = path.read_bytes()
    return {"bytes": len(data), "chars": len(data.decode("utf-8", errors="replace"))}


def sizes(tree: Path | None = None) -> dict[str, Any]:
    """The static text a turn can pay for: the tree's INDEX.md files, find-tools, the delegate skill, leaf.md."""
    out: dict[str, Any] = {
        "find-tools SKILL.md": file_size(SKILLS / "find-tools/SKILL.md"),
        "find-tools scripts/tools.py (run, never read)": file_size(SKILLS / "find-tools/scripts/tools.py"),
        "delegate SKILL.md": file_size(SKILLS / "delegate/SKILL.md"),
        "leaf.md": file_size(LEAF),
        "skills/INDEX.md (shipped)": file_size(SKILLS / "INDEX.md"),
    }
    for topic in sorted((SKILLS / "topics").glob("*/INDEX.md")):
        out[f"skills/topics/{topic.parent.name}/INDEX.md (shipped)"] = file_size(topic)
    for reference in sorted((SKILLS / "delegate/references").glob("*.md")):
        out[f"delegate references/{reference.name} (read on demand)"] = file_size(reference)
    if tree is not None:
        out["tools/INDEX.md (mounted)"] = file_size(tree / "INDEX.md")
        out["tools/helpers/INDEX.md (mounted)"] = file_size(tree / "helpers/INDEX.md")
    return {name: value for name, value in out.items() if value is not None}


# ---------------------------------------------------------------------------------------------------------
# Ranking without a model: each scenario's prompt as the query to bin/tools


def load_tools(script: Path = TOOLS_SCRIPT) -> Any:
    """The `tools` script as a module: the ranker a sandbox runs, from this checkout or another commit's copy."""
    spec = importlib.util.spec_from_file_location("skill_eval_tools", script)
    if spec is None or spec.loader is None:
        raise ValueError(f"{script} cannot be loaded as a module")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def shipped_tree(base: Path) -> dict[str, Any]:
    """A tool tree under `base` as `by run` mounts one today: every skill `capabilities.json` lists, chosen, and
    the Anvil mock's packages as connectors. Returns what it holds; the tree is `base/tools`."""
    tools, capabilities, connectors = base / "tools", base / "capabilities", base / "connectors"
    for directory in (tools, capabilities, connectors):
        directory.mkdir(parents=True, exist_ok=True)
    skills = json.loads(CAPABILITIES.read_text(encoding="utf-8"))["skills"]
    lines = ["# Your tools\n", "\n## Skills (skills/NAME/SKILL.md)\n"]
    for skill in skills:
        (capabilities / skill["id"]).symlink_to(SKILLS / skill["id"])
        lines.append(f"- {skill['id']}: {skill['description']}\n")
    (tools / "skills").symlink_to("../capabilities")
    (tools / "connectors").symlink_to("../connectors")
    packaged: list[str] = []
    notes: list[str] = []
    for bundle in sorted(p for p in BUNDLES.iterdir() if p.is_dir()):
        out = str(connectors / bundle.name)
        done = subprocess.run(
            [str(ANVIL_MOCK), "package", "harness", str(bundle), "--out", out, "--connector", bundle.name],
            capture_output=True,
            text=True,
            check=False,
        )
        if done.returncode == 0:
            packaged.append(bundle.name)
        else:
            notes.append(f"connector {bundle.name} not packaged: {done.stderr.strip() or done.returncode}")
    if packaged:
        lines.append("\n## Connectors (connectors/ID/SKILL.md)\n")
        lines += [f"- {name}\n" for name in packaged]
    (tools / "INDEX.md").write_text("".join(lines), encoding="utf-8")
    return {"skills": [s["id"] for s in skills], "connectors": packaged, "notes": notes}


def expected_documents(scenario: Scenario) -> list[tuple[str, str]]:
    """(kind, id) of each thing a scenario expects, in the document kinds `tools` ranks."""
    out = [("reference" if REFERENCE in name else "skill", name) for name in scenario.expect_skills]
    out += [("operation", op) for op in scenario.expect_operations]
    return out + [("helper", helper) for helper in scenario.expect_helpers if helper != "own"]


def rank_scenarios(
    scenarios: list[Scenario], tree: Path, plain: bool = False, script: Path = TOOLS_SCRIPT
) -> dict[str, Any]:
    """Each scenario's prompt ranked by `tools search` over `tree`: the rank of every expected id, and overall
    the top-1 and top-3 rates and the mean reciprocal rank. An expected id with no document in the tree is
    `absent` and not counted; one with a document that scored nothing is unranked and counts as a miss."""
    tools = load_tools(script)
    refused: list[str] = []
    docs = tools.documents(tree, refused)
    present = {(d.kind, d.id) for d in docs}
    chosen = frozenset() if plain or not hasattr(tools, "chosen_tools") else tools.chosen_tools(tree)
    rows: list[dict[str, Any]] = []
    reciprocals: list[float] = []
    absent = 0
    for scenario in scenarios:
        if scenario.none:
            continue
        ranked = tools.rank(docs, [scenario.prompt], chosen) if chosen else tools.rank(docs, [scenario.prompt])
        position = {(d.kind, d.id): at for at, (_, d) in enumerate(ranked, start=1)}
        expected: list[dict[str, Any]] = []
        for kind, ident in expected_documents(scenario):
            if (kind, ident) not in present:
                absent += 1
                expected.append({"kind": kind, "id": ident, "rank": None, "absent": True})
                continue
            at = position.get((kind, ident))
            reciprocals.append(1 / at if at else 0.0)
            expected.append({"kind": kind, "id": ident, "rank": at, "absent": False})
        rows.append(
            {
                "id": scenario.id,
                "area": scenario.area,
                "expected": expected,
                "top": [f"{d.kind}:{d.id}" for _, d in ranked[:3]],
            }
        )
    ranks = [e["rank"] for r in rows for e in r["expected"] if not e["absent"]]
    return {
        "schema": "branchyard.skill-eval-rank/v1",
        "tree": str(tree),
        "plain": plain,
        "tools": str(script),
        "documents": len(docs),
        "refused": refused,
        "overall": {
            "scenarios": len(rows),
            "expected": len(ranks),
            "absent": absent,
            "top1": sum(1 for at in ranks if at == 1),
            "top3": sum(1 for at in ranks if at is not None and at <= 3),
            "unranked": sum(1 for at in ranks if at is None),
            "top1_rate": rate(sum(1 for at in ranks if at == 1), len(ranks)),
            "top3_rate": rate(sum(1 for at in ranks if at is not None and at <= 3), len(ranks)),
            "mrr": round(sum(reciprocals) / len(reciprocals), 4) if reciprocals else None,
        },
        "scenarios": rows,
    }


def rank_markdown(report: dict[str, Any]) -> str:
    """The overall rates, then one line per expected id with the three documents ranked first."""
    a = report["overall"]
    lines = [
        "# Skill-discovery eval: ranking",
        "",
        f"- tree `{report['tree']}`: {report['documents']} documents; "
        f"{'BM25 alone (--plain)' if report['plain'] else 'BM25 with the chosen factor'}",
        f"- {a['scenarios']} scenarios, {a['expected']} expected ids ranked"
        + (f", {a['absent']} absent from the tree" if a["absent"] else ""),
        f"- top-1 {pct(a['top1_rate'])} ({a['top1']}), top-3 {pct(a['top3_rate'])} ({a['top3']}), "
        f"MRR {'-' if a['mrr'] is None else format(a['mrr'], '.3f')}, unranked {a['unranked']}",
    ]
    lines += [f"- refused: {why}" for why in report["refused"]]
    lines += ["", "| scenario | expected | rank | top 3 |", "|---|---|---|---|"]
    for row in report["scenarios"]:
        for e in row["expected"]:
            at = "absent" if e["absent"] else ("-" if e["rank"] is None else str(e["rank"]))
            lines.append(f"| {row['id']} | {e['kind']}:{e['id']} | {at} | {', '.join(row['top']) or '-'} |")
    return "\n".join(lines) + "\n"


# ---------------------------------------------------------------------------------------------------------
# Command line


def parse(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(prog="skill_eval", description=(__doc__ or "").split("\n\n")[0])
    sub = parser.add_subparsers(dest="action", required=True)
    lister = sub.add_parser("list", help="validate and list the scenarios")
    lister.add_argument("--scenarios", default="*", help="comma-separated globs over scenario id, area or file stem")
    scorer = sub.add_parser("score", help="score recorded logs")
    scorer.add_argument("logdir", type=Path)
    scorer.add_argument("--scenarios", default="*")
    scorer.add_argument("--out", type=Path, help="write report.json and report.md here")
    scorer.add_argument("--json", action="store_true", help="print the report as JSON")
    runner = sub.add_parser("run", help="run each scenario with `by run`, then score")
    runner.add_argument("--harness", required=True)
    runner.add_argument("--model")
    runner.add_argument("--scenarios", default="*")
    runner.add_argument("--out", type=Path, help="where logs and the report go (default: a dated directory)")
    runner.add_argument("--by", default="by", help="the by binary")
    runner.add_argument("--command", help="launch this instead of the harness (a fake agent, for tests)")
    runner.add_argument("--script", type=Path, help="JSON {scenario id: prompt} replacing prompts (fake agents)")
    runner.add_argument("--max-minutes", default="10")
    runner.add_argument("--anvil", help="Anvil command that can serve a gateway (connector scenarios)")
    runner.add_argument("--bundles", help="bundle root for --anvil")
    runner.add_argument("--record", action="store_true", help="also write evals/skills/records/DATE-HARNESS.json")
    size = sub.add_parser("sizes", help="print the static text sizes a turn pays for")
    size.add_argument("--tree", type=Path, help="a mounted tool tree (.branchyard/tools) to measure too")
    ranker = sub.add_parser("rank", help="rank each scenario's prompt with bin/tools over a tool tree; no model")
    ranker.add_argument("--tree", type=Path, help="a mounted tool tree (default: the shipped skills and the mock)")
    ranker.add_argument("--scenarios", default="*")
    ranker.add_argument("--plain", action="store_true", help="BM25 alone, without the chosen factor")
    ranker.add_argument("--tools", type=Path, default=TOOLS_SCRIPT, help="the tools script to rank with")
    ranker.add_argument("--json", action="store_true", help="print the report as JSON")
    return parser.parse_args(argv)


def emit(report: dict[str, Any], out: Path | None, as_json: bool = False) -> None:
    if out is not None:
        out.mkdir(parents=True, exist_ok=True)
        (out / "report.json").write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        (out / "report.md").write_text(markdown(report), encoding="utf-8")
    print(json.dumps(report, indent=2, sort_keys=True) if as_json else markdown(report), end="")


def main(argv: list[str] | None = None) -> int:
    args = parse(argv)
    if args.action == "sizes":
        print(json.dumps(sizes(args.tree), indent=2))
        return 0
    scenarios = load_scenarios(args.scenarios)
    bad = problems(scenarios)
    if bad:
        print("\n".join(bad), file=sys.stderr)
        return 2
    if not scenarios:
        print(f"no scenario matches {args.scenarios!r}", file=sys.stderr)
        return 2
    if args.action == "list":
        for s in scenarios:
            want = [*s.expect_skills, *s.expect_operations, *s.expect_helpers] or ["(none needed)"]
            print(f"{s.id:32} {s.area:11} {', '.join(want)}")
        print(f"{len(scenarios)} scenarios")
        return 0
    if args.action == "score":
        emit(score_dir(args.logdir, scenarios), args.out, args.json)
        return 0
    if args.action == "rank":
        with tempfile.TemporaryDirectory() as tmp:
            tree = args.tree
            if tree is None:
                shipped_tree(Path(tmp))
                tree = Path(tmp) / "tools"
            report = rank_scenarios(scenarios, tree, plain=args.plain, script=args.tools)
        print(json.dumps(report, indent=2, sort_keys=True) if args.json else rank_markdown(report), end="")
        return 0
    script = json.loads(args.script.read_text(encoding="utf-8")) if args.script else {}
    options = RunOptions(
        harness=args.harness,
        model=args.model,
        by=args.by,
        command=args.command,
        max_minutes=args.max_minutes,
        anvil=args.anvil,
        bundles=args.bundles,
        script=script,
    )
    out = args.out or Path("skill-eval-" + datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ"))
    run_all(scenarios, options, out)
    report = score_dir(out, scenarios)
    emit(report, out)
    if args.record:
        RECORDS.mkdir(parents=True, exist_ok=True)
        stamp = datetime.now(UTC).strftime("%Y-%m-%d")
        record = RECORDS / f"{stamp}-{args.harness}{'-' + args.model if args.model else ''}.json"
        record.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        print(f"skill-eval: recorded {record}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    with contextlib.suppress(BrokenPipeError):
        sys.exit(main())
