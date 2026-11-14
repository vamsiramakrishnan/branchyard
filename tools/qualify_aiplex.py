#!/usr/bin/env python3
"""Run the real `by` against a real `aiplex dev` and record what each side saw.

    python3 tools/qualify_aiplex.py --aiplex /path/to/aiplex --by /path/to/by \\
        --fake-agent /path/to/fake-acp-agent --branchyard-commit <sha> \\
        --out docs/evidence/aiplex-control-plane-2026-11-14.json

Scenario 1 (the stored token; docs/model-gateway.md, "AIPlex as the backend"): `aiplex dev` serves
AIPlex's mock providers on loopback; `aiplex connect local --client branchyard --write` writes the
Branchyard configuration; the script adds a second AIPlex backend for the model AIPlex will refuse and
a stand-in "direct provider" (a plain backend served by this script) beside them; one turn of the fake
agent runs a Python harness that calls the turn's gateway (both wires, one streamed, one refused by
AIPlex, one with forged attribution headers, one for the stand-in's model, one for a model no route
takes); the script reads `by log --json`, the stand-in's request count and AIPlex's usage records and
checks that the two systems' records join; and finally stops AIPlex and makes one more call.
There is no model, no real coding agent and no cloud call. Scenario 2 (a Branchyard-signed token,
`--federated`) is not implemented here yet.

Both programs run with a private HOME in a scratch directory and the hermetic variables the test kit
sets. The developer token is held in a `Secret` that prints as `<redacted>`. It is given to `by` (and
so to what `by` starts) and nowhere else: `aiplex` commands use the stored context of the private HOME,
and git gets neither. Everything the script captures or prints is scrubbed, and the scratch directory
and the evidence record are scanned for the token before the record is written. Whatever happens (a
failed check, an exception, Ctrl-C) `aiplex dev stop` runs, and a process of the run that is still
alive afterwards (found by its HOME, by descent from this script and by its process group) is a failed
check, then stopped.

Exit status: 0 every check passed, 1 a check failed (the record says which), 2 the run could not be
set up (a wrong path, a busy port, a dirty or different AIPlex build; nothing is written), 130
interrupted. Standard library only. Not part of CI (CI has no `aiplex` binary);
tests/test_qualify_aiplex.py covers the pure parts, with the mutations each check must catch.
"""

import argparse
import contextlib
import ctypes
import datetime
import hashlib
import http.server
import inspect
import json
import os
import platform
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from collections.abc import Callable, Iterable, Mapping, Sequence
from dataclasses import dataclass, field
from functools import partial
from pathlib import Path
from typing import Any, NoReturn

REDACTED = "<redacted>"
BRANCH = "qualify"
HARNESS_ID = "gemini-cli"
TURN_TIMEOUT = 120
STEP_TIMEOUT = 60
RECORD_WAIT = 30.0
MODEL_ABSENT = "mock-absent"
MODEL_DIRECT = "direct-claude"
MODEL_UNROUTED = "unrouted-claude"
BACKEND_ANTHROPIC = "aiplex-local-anthropic"
BACKEND_SECOND = "aiplex-local-anthropic-2"
BACKEND_DIRECT = "direct-anthropic"
REFUSED_BACKENDS = (BACKEND_ANTHROPIC, BACKEND_SECOND)
FORGED_ID = "forged-id"
FORGED_TAGS = "by_branch=forged"
FORBIDDEN_RESPONSE_HEADERS = ("www-authenticate", "set-cookie", "location", "link", "content-location", "via", "server")
# The response headers Branchyard's gateway relays from an AIPlex backend: `CONTROL_PLANE_RESPONSE_HEADERS` and
# `CONTROL_PLANE_RESPONSE_PREFIXES` in crates/branchyard/src/models/gateway.rs, as docs/model-gateway.md lists them.
RELAYED_RESPONSE_HEADERS = (
    "content-type",
    "content-encoding",
    "cache-control",
    "retry-after",
    "retry-after-ms",
    "x-should-retry",
    "request-id",
    "x-request-id",
)
RELAYED_RESPONSE_PREFIXES = ("anthropic-ratelimit-", "x-ratelimit-", "x-codex-")
# Added by Branchyard's own HTTP server to every answer it writes, not relayed from AIPlex: the date, the message
# framing (length or chunking) and the connection management. None names AIPlex or points the harness anywhere.
FRAMING_RESPONSE_HEADERS = ("date", "content-length", "transfer-encoding", "connection", "keep-alive")
CALL_ID = re.compile(r"[A-Za-z0-9_-]{22}")
REQUEST_ID = re.compile(r"[A-Za-z0-9._:-]{1,128}")
JWT_LIKE = re.compile(r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]*")
VALIDATE_OK = re.compile(r"^ok\s+\S*branchyard\.toml\s*$", re.MULTILINE)
HERMETIC_UNSET = (
    "AGY_ADC_AUTH",
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "CLAUDE_CODE_USE_VERTEX",
    "CLOUDSDK_CONFIG",
    "CLOUD_ML_REGION",
    "CLOUD_SHELL",
    "GCLOUD_PROJECT",
    "GCP_PROJECT",
    "GEMINI_CLI_USE_COMPUTE_ADC",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_CLOUD_LOCATION",
    "GOOGLE_CLOUD_PROJECT",
    "GOOGLE_GENAI_USE_VERTEXAI",
    "VERTEX_PROJECT_ID",
    "VERTEX_REGION",
)
# Variables a harness could pick up a provider credential from; none is inherited from the machine.
CREDENTIAL_VARIABLES = ("ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GEMINI_API_KEY", "GOOGLE_API_KEY")

SCOPE = (
    "real `by` and real `aiplex dev` on one machine over loopback, AIPlex's built-in mock providers, "
    "a stand-in direct provider served by the script, Branchyard's fake agent running a Python harness; "
    "no model, no real coding agent, no cloud call"
)
NOT_DONE = (
    'scenario 2, a Branchyard-signed token (`auth = "turn-token"`, contract section 7, `--federated`): '
    "the Branchyard side of the signed token does not exist yet",
    "a real model or a real coding agent: the providers are AIPlex's mock providers and the harness is a script",
    "`traceparent`: a local turn has no trace context, so none was sent and AIPlex made its own trace ids",
    "an AIPlex that is not `aiplex dev` (cloud deployment, real quota, a person's token, an agent's token)",
    "a Branchyard server (`by serve`): only a local yard ran",
    "a run on another operating system",
    "the Branchyard commit: `by --version` prints none, so the one given on the command line is stated, not verified",
)
# What the token check cannot see, said in the record next to the check.
TOKEN_CHECK_BLIND = (
    "an encoded or otherwise transformed copy of the token (base64, percent-encoding, reversed, split)",
    "a copy held somewhere other than a variable of the harness's environment (a file, a socket, a child's memory)",
)
# What a failing check means, attached to the record only when the check fails.
FINDINGS = {
    "harness-holds-no-token": (
        "Branchyard: the harness inherits the environment of the `by` process and the gateway removes only a "
        "fixed list of variables, so the variable that holds an AIPlex backend's key reaches the harness; "
        "the contract says the harness never holds an AIPlex credential"
    ),
    "harness-response-headers": (
        "Branchyard: the gateway relays response headers of AIPlex's answer that name AIPlex or redirect the "
        "harness; only an allow-list of headers should reach the harness"
    ),
    "record-tags-d": (
        "AIPlex: the ledger record of a call AIPlex refused after admission (MODEL_NOT_FOUND) has "
        "`tags: null`, though it has `client_request_id`; the caller's tags belong on refusal records too"
    ),
}

# (label, api, model, flags, kind): what the harness calls, in order. The kind is the script's own
# decision of what must happen to the call: `allowed` and `refused` go to AIPlex (the second is refused
# by AIPlex itself), `direct` goes to the stand-in plain backend.
CALLS: tuple[tuple[str, str, str, tuple[str, ...], str], ...] = (
    ("a", "anthropic", "mock-claude", (), "allowed"),
    ("b", "openai", "mock-gpt", (), "allowed"),
    ("c", "anthropic", "mock-claude", ("stream",), "allowed"),
    ("d", "anthropic", MODEL_ABSENT, (), "refused"),
    ("e", "openai", "mock-gpt", ("forge",), "allowed"),
    ("f", "anthropic", MODEL_DIRECT, (), "direct"),
    ("g", "anthropic", MODEL_UNROUTED, (), "direct"),
)
KIND = {label: kind for label, _api, _model, _flags, kind in CALLS}
OPERATION = {"anthropic": "messages", "openai": "chat-completions"}


def window_hits(value: str, digests: Iterable[tuple[str, str]]) -> bool:
    """Whether `value` holds, as a substring, text whose SHA-256 is one of `digests` ((length, hex) pairs)."""
    for length, digest in digests:
        size = int(length)
        for start in range(len(value) - size + 1):
            if hashlib.sha256(value[start : start + size].encode()).hexdigest() == digest:
                return True
    return False


# The harness the fake agent runs. It takes a results file and specs `label:api,model[,stream][,forge]`,
# calls the turn's gateway with the credentials the gateway gave it, and writes one JSON document. It
# is given digests of the token (never the token): QUALIFY_TOKEN_DIGESTS is `length:sha256` pairs for the
# whole token and for its signature segment, searched for in every variable of its own environment.
HARNESS = (
    r"""
from collections.abc import Iterable
import hashlib, json, os, re, sys, urllib.error, urllib.request

opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
digests = [tuple(p.split(":", 1)) for p in os.environ.get("QUALIFY_TOKEN_DIGESTS", "").split(",") if ":" in p]
port = os.environ.get("QUALIFY_AIPLEX_PORT", "")
hosts = r"(127\.0\.0\.1|localhost|\[::1\]|0\.0\.0\.0)"
names_aiplex = re.compile(hosts + ":" + re.escape(port) + r"(?!\d)") if port else None
"""
    + inspect.getsource(window_hits)
    + r"""

mine = {k: v for k, v in os.environ.items() if not k.startswith("QUALIFY_")}
env = {
    "helpers_received": {"token_digests": len(digests), "aiplex_port": bool(port)},
    "aiplex_token_variable_set": "AIPLEX_TOKEN" in os.environ,
    "variables_holding_the_token": sorted(k for k, v in mine.items() if window_hits(v, digests)),
    "variables_naming_aiplex": sorted(k for k, v in mine.items() if names_aiplex and names_aiplex.search(v)),
    "anthropic_base_url": os.environ.get("ANTHROPIC_BASE_URL"),
    "openai_base_url": os.environ.get("OPENAI_BASE_URL"),
}


def call(label, api, model, stream, forge):
    if api == "anthropic":
        url = os.environ["ANTHROPIC_BASE_URL"] + "/v1/messages"
        headers = {
            "x-api-key": os.environ["ANTHROPIC_API_KEY"],
            "anthropic-version": "2023-06-01",
            "content-type": "application/json",
        }
        body = {"model": model, "max_tokens": 16, "messages": [{"role": "user", "content": "hi"}]}
    else:
        url = os.environ["OPENAI_BASE_URL"] + "/chat/completions"
        headers = {"authorization": "Bearer " + os.environ["OPENAI_API_KEY"], "content-type": "application/json"}
        body = {"model": model, "messages": [{"role": "user", "content": "hi"}]}
    if stream:
        body["stream"] = True
    if forge:
        headers["X-AIPlex-Tags"] = "by_branch=forged"
        headers["X-Client-Request-Id"] = "forged-id"
    request = urllib.request.Request(url, data=json.dumps(body).encode(), headers=headers, method="POST")
    sent = {k.lower(): v for k, v in request.header_items()}
    for secret_header in ("x-api-key", "authorization"):
        if secret_header in sent:
            sent[secret_header] = "<omitted>"
    result = {"label": label, "api": api, "model": model, "stream": stream, "forged": forge, "sent_headers": sent}
    try:
        with opener.open(request, timeout=60) as response:
            data, status, got = response.read(), response.status, response.headers
    except urllib.error.HTTPError as error:
        data, status, got = error.read(), error.code, error.headers
    except OSError as error:
        data, status, got = b"", None, {}
        result["transport_error"] = type(error).__name__
    result.update(
        status=status,
        bytes=len(data),
        aiplex_request_id=got.get("X-Aiplex-Request-Id"),
        aiplex_error_code=got.get("X-Aiplex-Error-Code"),
        response_header_names=sorted({k.lower() for k in got.keys()}),
        body_head=data[:200].decode("utf-8", "replace"),
        stream_complete=(b"message_stop" in data or b"[DONE]" in data) if stream else None,
    )
    return result


calls = []
for spec in sys.argv[2:]:
    label, rest = spec.split(":", 1)
    api, model, *flags = rest.split(",")
    calls.append(call(label, api, model, "stream" in flags, "forge" in flags))
    print("call", label, calls[-1]["status"], calls[-1]["aiplex_error_code"])
with open(sys.argv[1], "w") as out:
    json.dump({"env": env, "calls": calls}, out)
"""
)


class SetupError(Exception):
    """The run cannot start or continue for a reason outside the checks (exit 2, nothing written)."""


class StepError(Exception):
    """A step could not do its work; the run records it as a failed check and goes on."""


class Interrupted(KeyboardInterrupt):
    """SIGTERM or SIGHUP, handled like Ctrl-C."""


class Secret:
    """A value that never prints: `str()`, `repr()` and f-strings give `<redacted>`."""

    __slots__ = ("_value",)

    def __init__(self, value: str) -> None:
        self._value = value

    def reveal(self) -> str:
        """The value, for the one place that must hand it to a process or search for it."""
        return self._value

    def __repr__(self) -> str:
        return REDACTED

    __str__ = __repr__


def redact(text: str, secrets: Iterable[Secret] = ()) -> str:
    """`text` with every secret's value, and anything shaped like a signed token, replaced."""
    for secret in secrets:
        value = secret.reveal()
        if value:
            text = text.replace(value, REDACTED)
    return JWT_LIKE.sub(REDACTED, text)


def leaks(text: str, secrets: Iterable[Secret]) -> bool:
    """Whether `text` holds a secret's value or anything shaped like a signed token."""
    return any(s.reveal() and s.reveal() in text for s in secrets) or JWT_LIKE.search(text) is not None


def token_digests(token: str) -> str:
    """`length:sha256` pairs, comma separated, for the whole token and its signature segment."""
    parts = [token]
    tail = token.rsplit(".", 1)[-1]
    if token.count(".") == 2 and len(tail) >= 20:
        parts.append(tail)
    return ",".join(f"{len(p)}:{hashlib.sha256(p.encode()).hexdigest()}" for p in parts)


def parse_dev_env(text: str) -> dict[str, str]:
    """The plain `export NAME='value'` lines of `aiplex dev env` (a line that refers to another variable is skipped)."""
    found: dict[str, str] = {}
    for line in text.splitlines():
        match = re.fullmatch(r"export ([A-Z][A-Z0-9_]*)=(?:'([^']*)'|([^\s\"'$]*))", line.strip())
        if match:
            found[match.group(1)] = match.group(2) if match.group(2) is not None else match.group(3)
    return found


def parse_events(text: str) -> list[dict[str, Any]]:
    """The events `by log --json` prints: a JSON array, or one object per line."""
    text = text.strip()
    if not text:
        return []
    try:
        data = json.loads(text)
    except json.JSONDecodeError:
        data = [json.loads(line) for line in text.splitlines() if line.strip()]
    if isinstance(data, dict):
        data = [data]
    return [e for e in data if isinstance(e, dict)]


def call_events(events: Iterable[Mapping[str, Any]]) -> list[dict[str, Any]]:
    """The `model` events that record one call (not the gateway announcement), in order."""
    found = []
    for event in events:
        model = event.get("model")
        if event.get("activity") == "model" and isinstance(model, dict) and model.get("kind") == "call":
            found.append(model)
    return found


def parse_records(text: str) -> list[dict[str, Any]]:
    """The `records` of `aiplex usage records -o json`."""
    data = json.loads(text)
    records = data.get("records") if isinstance(data, dict) else data
    return [r for r in records or [] if isinstance(r, dict)]


def expected_tags(show: Mapping[str, Any]) -> dict[str, str]:
    """The tags Branchyard sends, from its own record of the branch (`by show --json`)."""
    return {"by_branch": str(show["name"]), "by_harness": str(show["harness"]), "by_turn": str(show["turns"])}


def parse_aiplex_version(lines: list[str]) -> tuple[str, bool]:
    """The commit and whether the build is dirty, from `aiplex version`'s lines."""
    commit = ""
    for line in lines:
        match = re.match(r"\s*commit:\s*(\S+)", line)
        if match:
            commit = match.group(1)
    return commit, "dirty" in (lines[0] if lines else "") or commit.endswith("-dirty")


def aiplex_commit_problem(lines: list[str], stated: str | None) -> str | None:
    """Why this `aiplex` build cannot be recorded (no commit, dirty, or not the one stated), or None."""
    commit, dirty = parse_aiplex_version(lines)
    if not commit:
        return "`aiplex version` prints no `commit:` line"
    if dirty:
        return f"the aiplex build is dirty ({lines[0].strip()}); build from a clean commit"
    if stated and not (commit.startswith(stated) or stated.startswith(commit)):
        return f"--aiplex-commit {stated} is not the commit `aiplex version` reports ({commit})"
    return None


def sha256_file(path: Path) -> str:
    """The SHA-256 of a file."""
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def sanitize(text: str, replacements: Mapping[str, str]) -> str:
    """`text` with each machine-specific path replaced by its placeholder, longest path first."""
    for path in sorted(replacements, key=len, reverse=True):
        if path:
            text = text.replace(path, replacements[path])
    return text


# -- the checks' conditions, pure so that the mutations each must catch can be tested ------------


def validate_ok(result_code: int, out: str) -> bool:
    """Whether `by config validate` accepted the file: exit 0 and its `ok <path>/branchyard.toml` line."""
    return result_code == 0 and VALIDATE_OK.search(out) is not None


def harness_token_problems(env: Mapping[str, Any]) -> list[str]:
    """Why the harness's environment cannot be called free of the token; a missing helper is a problem, never a pass."""
    problems = []
    helpers = env.get("helpers_received") or {}
    if not helpers.get("token_digests"):
        problems.append("the harness received no token digest, so it could not look for the token")
    if env.get("variables_holding_the_token"):
        problems.append(f"the token is in {', '.join(env['variables_holding_the_token'])}")
    if env.get("aiplex_token_variable_set") is not False:
        problems.append("AIPLEX_TOKEN is set in the harness's environment")
    return problems


def gateway_only_problems(env: Mapping[str, Any]) -> list[str]:
    """Why the harness cannot be said to see only the gateway; a missing helper is a problem, never a pass."""
    problems = []
    if not (env.get("helpers_received") or {}).get("aiplex_port"):
        problems.append("the harness received no AIPlex port, so it could not look for AIPlex's address")
    if env.get("variables_naming_aiplex"):
        problems.append(f"{', '.join(env['variables_naming_aiplex'])} names AIPlex")
    for key in ("anthropic_base_url", "openai_base_url"):
        if not str(env.get(key)).startswith("http://127.0.0.1:"):
            problems.append(f"{key} is not a loopback gateway URL")
    return problems


def response_header_problems(calls: Iterable[Mapping[str, Any]]) -> list[str]:
    """The response headers of an AIPlex answer that reached the harness without being on the gateway's allow-list
    (or being the server's own framing), the named ones among them, and any `X-Aiplex-*`."""
    problems = []
    for call in calls:
        if KIND.get(str(call.get("label"))) not in ("allowed", "refused"):
            continue
        extra = [
            h
            for h in call.get("response_header_names") or []
            if h not in RELAYED_RESPONSE_HEADERS
            and h not in FRAMING_RESPONSE_HEADERS
            and not h.startswith(RELAYED_RESPONSE_PREFIXES)
        ]
        if extra:
            named = [h for h in extra if h in FORBIDDEN_RESPONSE_HEADERS or h.startswith("x-aiplex-")]
            problems.append(
                f"{call['label']}: {', '.join(extra)}" + (f" (including {', '.join(named)})" if named else "")
            )
    return problems


def compare_event(label: str, event: Mapping[str, Any], seen: Mapping[str, Any], kind: str) -> list[str]:
    """What is wrong with one call's `model` event, given what the harness saw and what the call must be."""
    problems = []
    plane = event.get("control_plane") or {}
    if event.get("model") != seen.get("model") or event.get("api") != seen.get("api"):
        problems.append(f"{label}: the event is for another model or API than the call")
    if event.get("status") != seen.get("status"):
        problems.append(f"{label}: the event's status differs from the harness's")
    if kind == "direct":
        if event.get("backend") != BACKEND_DIRECT:
            problems.append(f"{label}: answered by {event.get('backend')!r}, not the direct provider")
        if event.get("decision") != "allowed" or event.get("status") != 200:
            problems.append(f"{label}: the direct call was not allowed and answered 200")
        if plane.get("plane") == "aiplex" or plane.get("request_id") or plane.get("error_code"):
            problems.append(f"{label}: a call to a plain backend carries AIPlex's control plane")
        if seen.get("aiplex_request_id"):
            problems.append(f"{label}: the answer of a plain backend carries an AIPlex request id")
        return problems
    if plane.get("plane") != "aiplex":
        problems.append(f"{label}: no control_plane aiplex on the event")
    if not REQUEST_ID.fullmatch(str(plane.get("request_id") or "")):
        problems.append(f"{label}: no usable aiplex request id on the event")
    if not CALL_ID.fullmatch(str(plane.get("call_id") or "")):
        problems.append(f"{label}: the event's call id is not 22 characters of A-Z a-z 0-9 - _")
    if kind == "refused":
        if event.get("decision") != "refused":
            problems.append(f"{label}: decision is {event.get('decision')!r}, not 'refused'")
        if plane.get("error_code") != "MODEL_NOT_FOUND":
            problems.append(f"{label}: the event's error code is {plane.get('error_code')!r}, not MODEL_NOT_FOUND")
        if "refused by aiplex" not in str(event.get("reason")):
            problems.append(f"{label}: the reason does not say AIPlex refused it")
        if event.get("failed_over"):
            problems.append(f"{label}: another backend was tried after the refusal")
        if event.get("backend") not in REFUSED_BACKENDS:
            problems.append(f"{label}: the refusal names backend {event.get('backend')!r}")
    elif event.get("decision") != "allowed" or plane.get("error_code"):
        problems.append(f"{label}: an allowed call carries a refusal")
    return problems


def event_problems(
    table: Iterable[tuple[str, str, str, tuple[str, ...], str]],
    harness_calls: Sequence[Mapping[str, Any]],
    events: Sequence[Mapping[str, Any]],
) -> list[str]:
    """What is wrong with Branchyard's events as a whole. Which call is refused comes from the script's table,
    never from what the harness reports, and exactly that call, and no other, must be a refusal by AIPlex."""
    table = list(table)
    problems = []
    if len(harness_calls) != len(table) or len(events) != len(table):
        problems.append(f"{len(harness_calls)} answers and {len(events)} events for {len(table)} calls")
        return problems
    for (label, api, model, _flags, kind), seen, event in zip(table, harness_calls, events, strict=True):
        if (seen.get("label"), seen.get("api"), seen.get("model")) != (label, api, model):
            problems.append(f"{label}: the harness reports another call than the one the script made")
        problems += compare_event(label, event, {**seen, "model": model, "api": api}, kind)
    refused = [
        t[0]
        for t, e in zip(table, events, strict=True)
        if e.get("decision") == "refused" or (e.get("control_plane") or {}).get("error_code")
    ]
    expected = [t[0] for t in table if t[4] == "refused"]
    if refused != expected:
        problems.append(f"refused calls are {refused}, expected exactly {expected}")
    ids = [
        str((e.get("control_plane") or {}).get("call_id"))
        for t, e in zip(table, events, strict=True)
        if t[4] != "direct"
    ]
    if len(set(ids)) != len(ids):
        problems.append("two calls share a call id")
    return problems


def refusal_final_problems(event: Mapping[str, Any], records_for_the_call_id: Sequence[Mapping[str, Any]]) -> list[str]:
    """Why a refusal cannot be called final: the event names one backend, and AIPlex holds one record for the
    call's id (a call has one id on every attempt, so a second attempt would make a second record)."""
    problems = []
    if event.get("decision") != "refused":
        problems.append("the call was not refused")
    if event.get("failed_over"):
        problems.append(f"the event lists backends tried before the answer: {event.get('failed_over')}")
    if event.get("backend") not in REFUSED_BACKENDS:
        problems.append(f"the event names backend {event.get('backend')!r}")
    if len(records_for_the_call_id) != 1:
        problems.append(f"AIPlex holds {len(records_for_the_call_id)} records for the call's id, not 1")
    return problems


def forged_problems(
    call: Mapping[str, Any], record: Mapping[str, Any], found_for_forged_id: list[Any], call_id: str
) -> list[str]:
    """Why the forged-header check cannot pass: the harness must have sent both forged headers, and neither
    may have reached AIPlex as an id or a tag."""
    problems = []
    sent = call.get("sent_headers") or {}
    if sent.get("x-client-request-id") != FORGED_ID:
        problems.append("the harness did not send the forged X-Client-Request-Id")
    if sent.get("x-aiplex-tags") != FORGED_TAGS:
        problems.append("the harness did not send the forged X-AIPlex-Tags")
    text = json.dumps(record)
    if found_for_forged_id:
        problems.append("AIPlex has a record under the forged client request id")
    if FORGED_ID in text or '"forged"' in text:
        problems.append("a forged value is in AIPlex's record")
    if record.get("client_request_id") != call_id:
        problems.append("the record's client_request_id is not Branchyard's call id")
    return problems


def compare_record(
    label: str, record: Mapping[str, Any], event: Mapping[str, Any], seen: Mapping[str, Any], kind: str
) -> list[str]:
    """What is wrong with the AIPlex record of one call as a join with Branchyard's event; empty when nothing is."""
    problems = []
    plane = event.get("control_plane") or {}
    if record.get("request_id") != plane.get("request_id"):
        problems.append(f"{label}: record request_id is not the event's")
    if record.get("client_request_id") != plane.get("call_id"):
        problems.append(f"{label}: record client_request_id is not Branchyard's call id")
    if record.get("model_id") != seen.get("model"):
        problems.append(f"{label}: record model is {record.get('model_id')!r}")
    if kind == "refused":
        if not record.get("operation"):
            problems.append(f"{label}: the refusal's record names no operation")
        if record.get("status") == "ok" or not record.get("error_class"):
            problems.append(
                f"{label}: the record is not a refusal (status {record.get('status')!r}, "
                f"error_class {record.get('error_class')!r})"
            )
    elif OPERATION[str(seen.get("api"))] not in str(record.get("operation")):
        problems.append(f"{label}: record operation {record.get('operation')!r} is not the {seen.get('api')} wire's")
    if bool(record.get("streaming")) != bool(seen.get("stream")):
        problems.append(f"{label}: record streaming flag is wrong")
    if record.get("http_status") != seen.get("status"):
        problems.append(f"{label}: record http_status is {record.get('http_status')!r}")
    return problems


def tags_of(record: Mapping[str, Any]) -> dict[str, str] | None:
    """A record's tags, or None when it has none."""
    tags = record.get("tags")
    return {str(k): str(v) for k, v in tags.items()} if isinstance(tags, dict) and tags else None


def direct_problems(
    calls: Sequence[Mapping[str, Any]],
    stand_in: Sequence[Mapping[str, Any]],
    aiplex_records: Sequence[Mapping[str, Any]],
    aiplex_bound: int,
) -> list[str]:
    """Why the plain backend cannot be said to work beside the AIPlex ones: the stand-in answered exactly the direct
    calls, never saw the AIPlex token, and AIPlex holds records for the AIPlex-bound calls only."""
    problems = []
    direct = [c for c in calls if KIND.get(str(c.get("label"))) == "direct"]
    if [r.get("model") for r in stand_in] != [c.get("model") for c in direct]:
        problems.append(f"the stand-in saw models {[r.get('model') for r in stand_in]}")
    if any(r.get("token_seen") for r in stand_in):
        problems.append("the AIPlex token reached the stand-in direct provider")
    if any(c.get("status") != 200 or "stand-in" not in str(c.get("body_head")) for c in direct):
        problems.append("a direct call was not answered by the stand-in")
    if len(aiplex_records) != aiplex_bound:
        problems.append(f"AIPlex holds {len(aiplex_records)} records, {aiplex_bound} calls went to it")
    if any(r.get("model_id") in (MODEL_DIRECT, MODEL_UNROUTED) for r in aiplex_records):
        problems.append("AIPlex holds a record for a call that went to the stand-in")
    return problems


def outage_problems(
    call: Mapping[str, Any], events: Sequence[Mapping[str, Any]], stand_in_before: int, stand_in_after: int
) -> list[str]:
    """Why an outage cannot be called fail-closed: Branchyard's own 502, no AIPlex header, only the AIPlex backend
    named, and not one request more at the stand-in direct provider."""
    problems = []
    event = events[0] if events else {}
    plane = event.get("control_plane") or {}
    names = set(re.findall(r"[A-Za-z0-9_-]*(?:aiplex|direct)[A-Za-z0-9_-]*", json.dumps(events))) - {"aiplex"}
    if len(events) != 1:
        problems.append(f"{len(events)} events for the call")
    if call.get("status") != 502 or "Branchyard" not in str(call.get("body_head")):
        problems.append("the harness did not get Branchyard's own 502")
    if call.get("aiplex_request_id") or call.get("aiplex_error_code"):
        problems.append("the answer carries an AIPlex header")
    if event.get("decision") == "allowed" or event.get("status") != 502 or plane.get("request_id"):
        problems.append("the event does not record a failure of the AIPlex backend")
    if names != {BACKEND_ANTHROPIC}:
        problems.append(f"the event names backends {sorted(names)}")
    if stand_in_after != stand_in_before:
        problems.append(f"the direct provider received {stand_in_after - stand_in_before} request(s) during the outage")
    return problems


def cleanup_problems(report: Mapping[str, Any]) -> list[str]:
    """Why the cleanup cannot be called clean: `aiplex dev stop` must have worked, and nothing may have been
    left for the sweep to kill (the sweep is a safety net; what it kills is a failure, named)."""
    problems = []
    if 0 not in (report.get("aiplex_dev_stop_exits") or []):
        problems.append("`aiplex dev stop` never exited 0")
    for killed in report.get("killed_by_the_sweep") or []:
        problems.append(f"the sweep had to kill {killed.get('name')} (pid {killed.get('pid')})")
    if report.get("processes_left"):
        problems.append(f"{report['processes_left']} process(es) could not be stopped")
    return problems


def scan_for_secret(root: Path, secret: Secret) -> list[str]:
    """The files under `root` (relative paths) whose bytes hold the secret or its signature segment, as plain text."""
    value = secret.reveal()
    needles = [value.encode()] if value else []
    tail = value.rsplit(".", 1)[-1]
    if value.count(".") == 2 and len(tail) >= 20:
        needles.append(tail.encode())
    hits = []
    for path in sorted(root.rglob("*")):
        if path.is_symlink() or not path.is_file():
            continue
        try:
            data = path.read_bytes()
        except OSError:
            hits.append(f"{path.relative_to(root)} (unreadable)")
            continue
        if any(n in data for n in needles):
            hits.append(str(path.relative_to(root)))
    return hits


def hermetic_env(base: Mapping[str, str], home: Path, user_config: Path, ceiling: Path) -> dict[str, str]:
    """`base` made hermetic as the test kit does, with a private HOME, and with no AIPlex or provider credential."""
    env = {
        k: v
        for k, v in base.items()
        if not k.startswith(("BRANCHYARD_", "AIPLEX_", "QUALIFY_"))
        and k not in HERMETIC_UNSET
        and k not in CREDENTIAL_VARIABLES
    }
    env.update(
        HOME=str(home),
        XDG_CONFIG_HOME=str(home / ".config"),
        BRANCHYARD_USER_CONFIG=str(user_config),
        GIT_CEILING_DIRECTORIES=str(ceiling),
        GIT_CONFIG_GLOBAL="/dev/null",
        GIT_CONFIG_NOSYSTEM="1",
        NO_COLOR="1",
        PAGER="cat",
        NO_PROXY="127.0.0.1,localhost",
        no_proxy="127.0.0.1,localhost",
    )
    return env


def environ_has_home(environ: bytes, home: str) -> bool:
    """Whether a /proc/PID/environ blob carries `HOME=<home>`."""
    return f"HOME={home}".encode() in environ.split(b"\0")


def stat_fields(stat: str) -> tuple[str, int, int, int, str]:
    """(state, ppid, process group, session, command name) of a /proc/PID/stat line ('Z' is a zombie)."""
    name = stat[stat.index("(") + 1 : stat.rindex(")")]
    rest = stat[stat.rindex(")") + 1 :].split()
    return rest[0], int(rest[1]), int(rest[2]), int(rest[3]), name


def stat_state(stat: str) -> str:
    """The state letter of a /proc/PID/stat line."""
    return stat_fields(stat)[0]


def select_processes(
    table: Iterable[Mapping[str, Any]], home_match: Callable[[Mapping[str, Any]], bool], root: int, groups: set[int]
) -> list[dict[str, Any]]:
    """The live processes that belong to the run: carrying the private HOME, descended from `root` (this script),
    or in a process group or session that the script started. `table` rows have pid, state, ppid, pgrp, sid, name."""
    rows = [dict(r) for r in table if r["state"] != "Z" and r["pid"] != root]
    children: dict[int, list[int]] = {}
    for row in rows:
        children.setdefault(row["ppid"], []).append(row["pid"])
    descended: set[int] = set()
    stack = [root]
    while stack:
        for child in children.get(stack.pop(), []):
            if child not in descended:
                descended.add(child)
                stack.append(child)
    return [r for r in rows if r["pid"] in descended or r["pgrp"] in groups or r["sid"] in groups or home_match(r)]


def process_table() -> list[dict[str, Any]]:
    """Every process of the machine as rows for `select_processes`, with the HOME of each."""
    table = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            state, ppid, pgrp, sid, name = stat_fields((entry / "stat").read_text())
        except (OSError, ValueError):
            continue
        try:
            environ = (entry / "environ").read_bytes()
        except OSError:
            environ = b""
        table.append(
            {
                "pid": int(entry.name),
                "state": state,
                "ppid": ppid,
                "pgrp": pgrp,
                "sid": sid,
                "name": name,
                "environ": environ,
            }
        )
    return table


def processes_with_home(home: Path, groups: set[int] | None = None) -> list[dict[str, Any]]:
    """The run's live processes (see `select_processes`), other than this one."""
    found = select_processes(
        process_table(), lambda r: environ_has_home(r["environ"], str(home)), os.getpid(), groups or set()
    )
    return [{k: v for k, v in r.items() if k != "environ"} for r in found]


def wait_until(
    probe: Callable[[], Any],
    timeout: float,
    interval: float = 0.2,
    clock: Callable[[], float] = time.monotonic,
    pause: Callable[[float], None] = time.sleep,
) -> Any:
    """The first truthy answer of `probe`, polled until `timeout` seconds have passed; the last answer otherwise."""
    deadline = clock() + timeout
    while True:
        answer = probe()
        if answer or clock() >= deadline:
            return answer
        pause(interval)


def free_port() -> int:
    """A loopback port nothing listens on now (another process may take it before it is used)."""
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return int(probe.getsockname()[1])


class StandIn:
    """A stand-in direct provider on loopback: answers the Anthropic Messages shape and keeps what it was asked."""

    def __init__(self, secrets: list[Secret]) -> None:
        self.requests: list[dict[str, Any]] = []
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self) -> None:
                body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
                text = body.decode("utf-8", "replace") + "".join(f"{k}: {v}\n" for k, v in self.headers.items())
                try:
                    model = json.loads(body).get("model")
                except (ValueError, AttributeError):
                    model = None
                outer.requests.append(
                    {
                        "path": self.path,
                        "model": model,
                        "header_names": sorted({k.lower() for k in self.headers}),
                        "token_seen": leaks(text, secrets),
                    }
                )
                answer = json.dumps(
                    {
                        "id": "msg_stand_in",
                        "type": "message",
                        "role": "assistant",
                        "model": model,
                        "content": [{"type": "text", "text": "stand-in"}],
                        "stop_reason": "end_turn",
                        "usage": {"input_tokens": 1, "output_tokens": 1},
                    }
                ).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(answer)))
                self.end_headers()
                self.wfile.write(answer)

            def log_message(self, format: str, *args: Any) -> None:
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = int(self.server.server_address[1])
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self) -> None:
        """Stop serving."""
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(5)


@dataclass
class Result:
    """A finished command, its output already scrubbed."""

    code: int
    out: str
    err: str


@dataclass
class Check:
    """One claim and what was observed."""

    id: str
    claim: str
    passed: bool
    observed: dict[str, Any] = field(default_factory=dict)


class Run:
    """One run's state: the scratch directory, the processes, the checks and everything captured."""

    def __init__(self, args: argparse.Namespace, scratch: Path) -> None:
        self.args = args
        self.scratch = scratch
        self.home = scratch / "home"
        self.repo = scratch / "repo"
        self.secrets: list[Secret] = []
        self.env: dict[str, str] = {}
        self.by_env: dict[str, str] = {}
        self.checks: list[Check] = []
        self.versions: dict[str, Any] = {}
        self.observations: dict[str, Any] = {}
        self.captured_leaks = 0
        self.dev_started = False
        self.cleaned = False
        self.stop_exits: list[int] = []
        self.groups: set[int] = set()
        self.stand_in: StandIn | None = None
        self.url = ""
        self.port = 0

    # -- output -----------------------------------------------------------------------------

    def say(self, message: str) -> None:
        """Print one line, scrubbed."""
        print(redact(message, self.secrets), flush=True)

    def check(self, check_id: str, claim: str, passed: bool, **observed: Any) -> bool:
        """Record one check and print its verdict."""
        self.checks.append(Check(check_id, claim, bool(passed), observed))
        self.say(f"{'PASS' if passed else 'FAIL'}  {check_id}: {claim}")
        return bool(passed)

    # -- processes --------------------------------------------------------------------------

    def exec(
        self,
        argv: list[str],
        timeout: float = STEP_TIMEOUT,
        cwd: Path | None = None,
        raw: bool = False,
        env: Mapping[str, str] | None = None,
    ) -> Result:
        """Run a command in its own process group; kill the group on a timeout or an interrupt.

        The command gets `env` (default: the hermetic environment, which holds no token). The output is
        scrubbed before anyone sees it, and a secret found in it is counted. `raw` hands back the
        unscrubbed output, for the one command that prints the token on purpose."""
        process = subprocess.Popen(
            argv,
            cwd=cwd or self.scratch,
            env=dict(env if env is not None else self.env),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            errors="replace",
            start_new_session=True,
        )
        self.groups.add(process.pid)
        try:
            out, err = process.communicate(timeout=timeout)
        except BaseException:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            raise
        if raw:
            return Result(process.returncode, out, err)
        if leaks(out + err, self.secrets):
            self.captured_leaks += 1
        return Result(process.returncode, redact(out, self.secrets), redact(err, self.secrets))

    def aiplex(self, *args: str, timeout: float = STEP_TIMEOUT, raw: bool = False) -> Result:
        """Run the `aiplex` CLI (with the private HOME's stored context, and no token in its environment)."""
        return self.exec([self.args.aiplex, *args], timeout=timeout, raw=raw)

    def by(self, *args: str, timeout: float = STEP_TIMEOUT) -> Result:
        """Run `by` in the yard, the one program that is given the token."""
        return self.exec([self.args.by, *args], timeout=timeout, cwd=self.repo, env=self.by_env or self.env)

    # -- the steps --------------------------------------------------------------------------

    def start_aiplex(self) -> None:
        """Start `aiplex dev` detached; on a taken port retry on another unless one was asked for."""
        attempts = 1 if self.args.port else 4
        for _attempt in range(attempts):
            port = self.args.port or free_port()
            self.dev_started = True
            started = self.aiplex("dev", "--detach", "--no-console", "--port", str(port), timeout=90)
            if started.code == 0:
                self.port = port
                self.url = f"http://127.0.0.1:{port}"
                break
            busy = "already in use" in started.out + started.err
            self.aiplex("dev", "stop")
            if not busy:
                tail = (started.err or started.out)[-300:]
                raise SetupError(f"aiplex dev did not start (exit {started.code}): {tail}")
            self.say(f"port {port} is in use")
        else:
            raise SetupError(
                f"port {self.args.port} on 127.0.0.1 is already in use; pass another --port or none"
                if self.args.port
                else "no free port after several tries"
            )

        def ready() -> bool:
            status = self.aiplex("dev", "status", timeout=15)
            return status.code == 0 and "answering" in status.out

        if not wait_until(ready, 30):
            raise SetupError("aiplex dev did not answer `aiplex dev status` within 30 seconds")
        token = parse_dev_env(self.aiplex("dev", "env", raw=True).out).get("AIPLEX_TOKEN")
        if not token:
            raise SetupError("`aiplex dev env` printed no AIPLEX_TOKEN")
        self.secrets.append(Secret(token))
        self.say(f"aiplex dev is answering on {self.url}")

    def make_yard(self) -> None:
        """A git repository with one commit and a small configuration with something for the next step to preserve."""
        self.repo.mkdir(parents=True)
        for argv in (
            ["git", "init", "-q", "-b", "main"],
            ["git", "config", "user.name", "Test"],
            ["git", "config", "user.email", "test@localhost"],
        ):
            self.exec(argv, cwd=self.repo)
        (self.repo / "a.txt").write_text("one\n")
        (self.repo / "branchyard.toml").write_text(
            'version = 1\n\n# qualification project: keep this comment\n[models]\nallow = ["*"]\n'
        )
        (self.scratch / "harness.py").write_text(HARNESS)

    def add_routes(self, text: str) -> str:
        """The file `aiplex connect` wrote, plus what this run adds by hand: a second AIPlex backend in the route of
        the model AIPlex refuses (so that a retry would be visible) and a plain stand-in backend with its own route."""
        if self.stand_in is None:
            raise StepError("the stand-in provider is not running")
        marker = 'aiplex = "AIPLEX_TOKEN"\n'
        if marker not in text:
            raise StepError(
                'the written configuration has no `aiplex = "AIPLEX_TOKEN"` secret to put a second one beside'
            )
        text = text.replace(marker, marker + 'direct = "QUALIFY_DIRECT_KEY"\n', 1)
        url = f"{self.url}/ap/local"
        return (
            text
            + f'\n[models.backends.{BACKEND_SECOND}]\napi = "anthropic"\nurl = "{url}"\nkey = "aiplex"\n'
            + 'control_plane = "aiplex"\n'
            + f'\n[[models.routes]]\nmodel = "{MODEL_ABSENT}"\nbackends = ["{BACKEND_ANTHROPIC}", "{BACKEND_SECOND}"]\n'
            + f'\n[models.backends.{BACKEND_DIRECT}]\napi = "anthropic"\nurl = "http://127.0.0.1:{self.stand_in.port}"\n'
            + 'key = "direct"\n'
            + f'\n[[models.routes]]\nmodel = "{MODEL_DIRECT}"\nbackends = ["{BACKEND_DIRECT}"]\n'
        )

    def connect_and_validate(self) -> bool:
        """`aiplex connect` writes the configuration; the real Branchyard parser reads it, written and extended."""
        config = self.repo / "branchyard.toml"
        connect = self.aiplex("connect", "local", "--client", "branchyard", "--write", "--yes", "--file", str(config))
        written = config.read_text()
        self.observations["connect_output"] = connect.out.strip().splitlines()[-6:]
        self.observations["configuration_written"] = written.splitlines()
        first = self.by("config", "validate")
        config.write_text(self.add_routes(written))
        second = self.by("config", "validate")
        self.observations["validate_output"] = [first.out.strip(), second.out.strip()]
        ok = self.check(
            "config-connect-validate",
            "`aiplex connect local --client branchyard --write` writes a file that `by config validate` accepts "
            "(its `ok <path>/branchyard.toml` line and exit 0), keeping the comment already in it; the file still "
            "validates with a second AIPlex backend and a plain backend added",
            connect.code == 0
            and validate_ok(first.code, first.out)
            and validate_ok(second.code, second.out)
            and "keep this comment" in written
            and 'control_plane = "aiplex"' in written
            and "AIPLEX_TOKEN" in written,
            connect_exit=connect.code,
            validate_as_written=[first.code, first.out.strip()[-120:], first.err.strip()[-200:]],
            validate_as_extended=[second.code, second.out.strip()[-120:], second.err.strip()[-200:]],
        )
        if ok:
            self.exec(["git", "add", "."], cwd=self.repo)
            self.exec(["git", "commit", "-q", "-m", "initial"], cwd=self.repo)
        return ok

    def turn_env(self) -> None:
        """The environment of `by`: the hermetic one, the token, the stand-in's key, what the harness checks with."""
        token = self.secrets[0].reveal()
        self.by_env = {
            **self.env,
            "AIPLEX_TOKEN": token,
            "QUALIFY_TOKEN_DIGESTS": token_digests(token),
            "QUALIFY_AIPLEX_PORT": str(self.port),
            "QUALIFY_DIRECT_KEY": "stand-in-key",
        }

    def harness_command(self, results: Path, calls: Iterable[tuple[str, str, str, tuple[str, ...], str]]) -> str:
        """The `SH` prompt line that makes the fake agent run the harness."""
        specs = " ".join(f"{label}:{','.join((api, model, *flags))}" for label, api, model, flags, _kind in calls)
        return f"SH python3 {self.scratch / 'harness.py'} {results} {specs}"

    def read_results(self, results: Path) -> dict[str, Any]:
        """What the harness wrote."""
        try:
            data: dict[str, Any] = json.loads(results.read_text())
        except (OSError, json.JSONDecodeError) as error:
            raise StepError(f"the harness wrote no results ({type(error).__name__})") from error
        return data

    def turn_one(self) -> tuple[dict[str, Any], list[dict[str, Any]], dict[str, Any]]:
        """One turn with every call; returns what the harness saw, Branchyard's call events and `by show`."""
        results = self.scratch / "results-1.json"
        command = self.harness_command(results, CALLS)
        run = self.by(
            "run", "--name", BRANCH, "--harness", HARNESS_ID, "--command", self.args.fake_agent, "--yes", "--",
            command, timeout=TURN_TIMEOUT,
        )  # fmt: skip
        self.observations["turn_exit"] = run.code
        self.observations["turn_output_tail"] = run.out.strip().splitlines()[-8:]
        if run.code != 0:
            raise StepError(f"`by run` exited {run.code}: {(run.err or run.out)[-400:]}")
        harness = self.read_results(results)
        events = call_events(parse_events(self.by("log", BRANCH, "--json").out))
        show: dict[str, Any] = json.loads(self.by("show", BRANCH, "--json").out)
        text_log = self.by("log", BRANCH)
        self.observations["by_log_text"] = [line for line in text_log.out.splitlines() if "model:" in line]
        self.observations["expected_tags"] = expected_tags(show)
        return harness, events, show

    def check_harness(self, harness: Mapping[str, Any], show: Mapping[str, Any]) -> None:
        """What the harness could see: the gateway and nothing of AIPlex, and what came back to it."""
        env = harness["env"]
        calls = harness["calls"]
        self.observations["harness_env"] = env
        self.check(
            "harness-ran",
            f"the harness made {len(CALLS)} calls through the turn's gateway and each got an answer; "
            "the streamed answer ran to its end",
            len(calls) == len(CALLS)
            and all(c.get("status") for c in calls)
            and all(c.get("stream_complete") for c in calls if c.get("stream")),
            statuses=[c.get("status") for c in calls],
            streamed_complete=[c.get("stream_complete") for c in calls if c.get("stream")],
        )
        self.check(
            "branch-record",
            f"`by show` names the branch `{BRANCH}` and the harness `{HARNESS_ID}` that `by run` was given",
            show.get("name") == BRANCH and show.get("harness") == HARNESS_ID,
            name=show.get("name"),
            harness=show.get("harness"),
        )
        problems = gateway_only_problems(env)
        self.check(
            "harness-sees-gateway-only",
            "the harness's base URLs are the turn's gateway on loopback, and no variable of its environment names "
            "AIPlex's loopback address and port (127.0.0.1, localhost, [::1], 0.0.0.0)",
            not problems,
            problems=problems,
            base_urls=[env.get("anthropic_base_url"), env.get("openai_base_url")],
        )
        problems = harness_token_problems(env)
        self.check(
            "harness-holds-no-token",
            "no variable of the harness's environment holds the AIPlex token (the whole token or its signature "
            "segment, as a substring of any value), and `AIPLEX_TOKEN` is not set; the harness was given the "
            "digests it needs to look",
            not problems,
            problems=problems,
            aiplex_token_variable_set=env.get("aiplex_token_variable_set"),
            variables_holding_the_token=env.get("variables_holding_the_token"),
            helpers_received=env.get("helpers_received"),
            cannot_see=list(TOKEN_CHECK_BLIND),
        )
        problems = response_header_problems(calls)
        self.check(
            "harness-response-headers",
            "every response header the harness received on an answer that came from AIPlex is on the gateway's "
            "allow-list ("
            + ", ".join(RELAYED_RESPONSE_HEADERS)
            + "; prefixes "
            + ", ".join(RELAYED_RESPONSE_PREFIXES)
            + ") or is the server's own framing ("
            + ", ".join(FRAMING_RESPONSE_HEADERS)
            + "); in particular none of "
            + ", ".join(FORBIDDEN_RESPONSE_HEADERS)
            + " and no X-Aiplex-* header reached it",
            not problems,
            problems=problems,
            header_names_by_call={c["label"]: c.get("response_header_names") for c in calls},
            framing_headers_exempt="added by Branchyard's own HTTP server to every answer, not relayed from AIPlex",
        )

    def check_events(self, harness: Mapping[str, Any], events: list[dict[str, Any]]) -> None:
        """Branchyard's side: one `model` event per call, which call was refused decided by the script's table."""
        calls = harness["calls"]
        problems = event_problems(CALLS, calls, events)
        self.check(
            "branchyard-events",
            "`by log --json` has one `model` event per call: for the calls to AIPlex, AIPlex's request id and "
            "Branchyard's call id; exactly call d refused, as refused by aiplex with MODEL_NOT_FOUND; for the calls "
            "to the stand-in provider, no AIPlex control plane",
            not problems,
            problems=problems,
            events=[
                {
                    "label": c["label"],
                    "model": e.get("model"),
                    "decision": e.get("decision"),
                    "status": e.get("status"),
                    "backend": e.get("backend"),
                    "control_plane": e.get("control_plane"),
                    "reason": e.get("reason"),
                    "failed_over": e.get("failed_over"),
                }
                for c, e in zip(calls, events, strict=False)
            ],
        )

    def find(self, flag: str, value: str) -> list[dict[str, Any]]:
        """The AIPlex records for one id, by the CLI's own lookup."""
        found = self.aiplex("usage", "records", flag, value, "-o", "json")
        if found.code != 0:
            raise StepError(f"`aiplex usage records {flag}` exited {found.code}: {found.err.strip()[-300:]}")
        return parse_records(found.out)

    def check_records(self, harness: Mapping[str, Any], events: list[dict[str, Any]], show: Mapping[str, Any]) -> None:
        """AIPlex's side: each AIPlex-bound call has one record, found both ways, with the tags Branchyard used."""
        tags = expected_tags(show)
        traces = []
        callers = set()
        for call, event in zip(harness["calls"], events, strict=False):
            label, kind = call["label"], KIND[call["label"]]
            if kind == "direct":
                continue
            plane = event.get("control_plane") or {}
            request_id, call_id = str(plane.get("request_id")), str(plane.get("call_id"))
            by_request = wait_until(partial(self.find, "--request-id", request_id), RECORD_WAIT)
            by_client = self.find("--client-request-id", call_id)
            problems = []
            if len(by_request) != 1:
                problems.append(f"{len(by_request)} records for the request id")
            if len(by_client) != 1 or (by_request and by_client[0].get("id") != by_request[0].get("id")):
                problems.append("the client request id does not find the same single record")
            record = by_request[0] if by_request else {}
            if record:
                problems += compare_record(label, record, event, call, kind)
            traces.append(record.get("trace_id"))
            callers.add((record.get("caller_kind"), record.get("access_point_id")))
            self.check(
                f"record-join-{label}",
                f"call {label} ({call['api']} {call['model']}): AIPlex has one record, found by the request id and "
                "by the client request id, whose client_request_id is Branchyard's call id",
                not problems,
                problems=problems,
                request_id=request_id,
                call_id=call_id,
                client_request_id=record.get("client_request_id"),
                operation=record.get("operation"),
                http_status=record.get("http_status"),
                status=record.get("status"),
                error_class=record.get("error_class"),
            )
            self.check(
                f"record-tags-{label}",
                f"call {label}: the record's tags are by_branch, by_turn and by_harness with the values Branchyard's "
                "own records give",
                tags_of(record) == tags,
                expected=tags,
                observed_tags=tags_of(record),
            )
            if kind == "refused":
                final = refusal_final_problems(event, by_client)
                self.check(
                    "refusal-final",
                    "AIPlex's refusal is final: the refused model's route names two AIPlex backends, the event names "
                    "one and lists none tried before it, and AIPlex holds exactly one record for the call's id",
                    not final,
                    problems=final,
                    backend=event.get("backend"),
                    records_for_the_call_id=len(by_client),
                )
            if call["forged"]:
                found = self.find("--client-request-id", FORGED_ID)
                forged = forged_problems(call, record, found, call_id)
                self.check(
                    "forged-headers-dropped",
                    "the harness sent a forged X-Client-Request-Id and X-AIPlex-Tags (shown in the headers it sent) "
                    "and they reached AIPlex as neither an id nor a tag",
                    not forged,
                    problems=forged,
                    forged_headers_sent={
                        k: v for k, v in call["sent_headers"].items() if k in ("x-client-request-id", "x-aiplex-tags")
                    },
                    records_for_the_forged_id=len(found),
                )
        self.observations["trace_ids_distinct"] = len(set(traces)) == len(traces)
        self.observations["caller_kind_and_access_point"] = sorted(map(list, callers), key=str)

    def check_direct(self, harness: Mapping[str, Any]) -> None:
        """The plain backend beside the AIPlex ones: its calls stay there, and AIPlex holds no record of them."""
        if self.stand_in is None:
            raise StepError("the stand-in provider is not running")
        everything = self.aiplex("usage", "records", "--all", "-o", "json")
        if everything.code != 0:
            raise StepError(f"`aiplex usage records --all` exited {everything.code}")
        records = parse_records(everything.out)
        bound = sum(1 for _l, _a, _m, _f, kind in CALLS if kind != "direct")
        problems = direct_problems(harness["calls"], self.stand_in.requests, records, bound)
        self.check(
            "direct-provider-beside-aiplex",
            f"a call for `{MODEL_DIRECT}` (a route to the plain stand-in backend) and a call for `{MODEL_UNROUTED}` "
            "(no route; the API has a plain backend, so it goes to plain backends only) are answered by the stand-in, "
            "which never receives the AIPlex token, while AIPlex holds records only for the calls routed to it",
            not problems,
            problems=problems,
            stand_in_requests=[{k: v for k, v in r.items() if k != "token_seen"} for r in self.stand_in.requests],
            aiplex_records=len(records),
            aiplex_bound_calls=bound,
        )

    def check_outage(self, before: int) -> None:
        """With AIPlex stopped, a call fails with Branchyard's own error, no other backend is tried, and no
        request reaches the direct provider."""
        if self.stand_in is None:
            raise StepError("the stand-in provider is not running")
        stand_before = len(self.stand_in.requests)
        stop = self.aiplex("dev", "stop")
        self.stop_exits.append(stop.code)
        self.dev_started = False
        self.observations["stop_output"] = stop.out.strip()
        results = self.scratch / "results-2.json"
        outage_calls = (("o", "anthropic", "mock-claude", (), "allowed"),)
        send = self.by("send", BRANCH, "--yes", "--", self.harness_command(results, outage_calls), timeout=TURN_TIMEOUT)
        call = self.read_results(results)["calls"][0]
        events = call_events(parse_events(self.by("log", BRANCH, "--json").out))[before:]
        problems = outage_problems(call, events, stand_before, len(self.stand_in.requests))
        if stop.code != 0 or send.code != 0:
            problems.append(f"`aiplex dev stop` exited {stop.code}, `by send` exited {send.code}")
        event = events[0] if events else {}
        self.check(
            "outage-fails-closed",
            "with AIPlex stopped a call for an AIPlex-routed model fails with Branchyard's own 502 and no AIPlex "
            "header, the event names only the AIPlex backend, and the stand-in direct provider receives no request",
            not problems,
            problems=problems,
            harness_status=call.get("status"),
            harness_body_head=call.get("body_head"),
            event_decision=event.get("decision"),
            event_status=event.get("status"),
            stand_in_requests_before_and_after=[stand_before, len(self.stand_in.requests)],
            event_text=[x for x in self.by("log", BRANCH).out.splitlines() if "model:" in x][-1:],
        )

    def check_secret_hygiene(self) -> None:
        """The token is in no file under the scratch directory and in nothing the script captured."""
        hits = scan_for_secret(self.scratch, self.secrets[0])
        self.check(
            "secret-hygiene-files",
            "the token's value (whole, or its signature segment) is in no file under the run's scratch directory "
            "(repository, .branchyard, private HOME, logs), as plain text",
            not hits,
            files_with_the_token=hits,
        )
        self.check(
            "secret-hygiene-output",
            "the token's value is in no output the script captured from `aiplex`, `by`, git or the harness",
            self.captured_leaks == 0,
            captured_outputs_holding_it=self.captured_leaks,
        )

    # -- cleanup ----------------------------------------------------------------------------

    def live(self) -> list[dict[str, Any]]:
        """The run's processes still alive."""
        return processes_with_home(self.home, self.groups)

    def cleanup(self) -> dict[str, Any]:
        """Stop `aiplex dev`, give what it started time to go, then stop and name what is left. Safe to call twice."""
        report: dict[str, Any] = {}
        if self.dev_started:
            try:
                self.stop_exits.append(self.aiplex("dev", "stop", timeout=30).code)
            except (OSError, subprocess.SubprocessError) as error:
                report["aiplex_dev_stop_error"] = type(error).__name__
            self.dev_started = False
        report["aiplex_dev_stop_exits"] = list(self.stop_exits)
        wait_until(lambda: not self.live(), 10)
        stragglers = self.live()
        report["killed_by_the_sweep"] = [{"pid": p["pid"], "name": p["name"]} for p in stragglers]
        for sig in (signal.SIGTERM, signal.SIGKILL):
            if not stragglers:
                break
            for process in stragglers:
                with contextlib.suppress(ProcessLookupError):
                    os.kill(process["pid"], sig)
            wait_until(lambda: not self.live(), 5)
            stragglers = self.live()
        reap_children()
        report["processes_left"] = len(stragglers)
        if self.stand_in is not None:
            self.stand_in.close()
            self.stand_in = None
        self.cleaned = True
        return report


def step(run: Run, name: str, body: Callable[[], Any]) -> Any:
    """Run one step; an error that is not an interrupt or a setup failure becomes a failed check."""
    try:
        return body()
    except (SetupError, KeyboardInterrupt):
        raise
    except Exception as error:  # a step that cannot finish is a failed check, never a crash
        message = redact(f"{type(error).__name__}: {error}", run.secrets)[:500]
        run.check(f"step-{name}", f"step {name} could finish", False, error=message)
        return None


def command_output(argv: list[str], run: Run) -> list[str]:
    """The lines a version command prints."""
    out = run.exec(argv, timeout=20)
    if out.code != 0:
        raise SetupError(f"`{' '.join(argv)}` exited {out.code}: {(out.err or out.out)[-200:]}")
    return out.out.strip().splitlines()


def record_versions(run: Run) -> None:
    """The versions and hashes of what ran; the AIPlex commit is read from the binary, not the command line."""
    args = run.args
    aiplex_version = command_output([args.aiplex, "version"], run)
    problem = aiplex_commit_problem(aiplex_version, args.aiplex_commit)
    if problem:
        raise SetupError(problem)
    api = Path(args.aiplex).with_name("aiplex-api")
    if not api.is_file():
        raise SetupError(f"aiplex-api is not beside aiplex ({api.name} not found)")
    run.versions = {
        "by": command_output([args.by, "--version"], run)[0],
        "aiplex": aiplex_version[0],
        "aiplex_version_output": aiplex_version,
        "aiplex_commit": {
            "value": parse_aiplex_version(aiplex_version)[0],
            "verified": True,
            "from": "`aiplex version`",
        },
        "branchyard_commit": {
            "value": args.branchyard_commit,
            "verified": False,
            "note": "stated on the command line; `by --version` prints no commit",
        },
        "binary_sha256": {
            Path(p).name: sha256_file(Path(p)) for p in (args.by, args.fake_agent, args.aiplex, str(api))
        },
        "python": platform.python_version(),
        "platform": platform.platform(),
    }


def scenario_one(run: Run) -> None:
    """The stored-token scenario, step by step."""
    record_versions(run)
    run.say(f"versions: {run.versions['by']}; {run.versions['aiplex']}")
    run.start_aiplex()
    run.stand_in = StandIn(run.secrets)
    run.make_yard()
    run.turn_env()
    if not step(run, "connect", run.connect_and_validate):
        return
    outcome = step(run, "turn", run.turn_one)
    if outcome is None:
        return
    harness, events, show = outcome
    step(run, "harness", lambda: run.check_harness(harness, show))
    step(run, "events", lambda: run.check_events(harness, events))
    step(run, "records", lambda: run.check_records(harness, events, show))
    step(run, "direct", lambda: run.check_direct(harness))
    step(run, "outage", lambda: run.check_outage(len(events)))


def write_record(run: Run, path: Path, command: list[str]) -> None:
    """Write the evidence record, after making sure it holds no token and no path of this machine."""
    failed = [c for c in run.checks if not c.passed]
    record = {
        "date": datetime.datetime.now(datetime.UTC).date().isoformat(),
        "scope": SCOPE,
        "scenario": "1: a stored AIPlex token (contract sections 1 to 6 and 8)",
        "outcome": "passed" if not failed else f"failed: {len(failed)} of {len(run.checks)} checks",
        "command": command,
        "versions": run.versions,
        "checks": [
            {
                "id": c.id,
                "claim": c.claim,
                "result": "pass" if c.passed else "fail",
                "observed": c.observed,
                **({"finding": FINDINGS[c.id]} if not c.passed and c.id in FINDINGS else {}),
            }
            for c in run.checks
        ],
        "observations": run.observations,
        "not_done": list(NOT_DONE),
    }
    replacements = {
        str(run.scratch): "<scratch>",
        str(run.scratch.resolve()): "<scratch>",
        str(Path(run.args.aiplex).parent): "<aiplex-bin>",
        str(Path(run.args.by).parent): "<by-bin>",
        str(Path(run.args.fake_agent).parent): "<by-bin>",
    }
    text = sanitize(json.dumps(record, indent=2) + "\n", replacements)
    if leaks(text, run.secrets):
        raise SetupError("the evidence record would hold a token; nothing was written")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


EPILOG = """\
Exit status: 0 every check passed; 1 a check failed (the evidence record says which); 2 the run could not be
set up (a wrong path, a busy --port, an AIPlex build that is dirty or is not --aiplex-commit; nothing is
written); 130 interrupted. `aiplex dev stop` always runs.

Covers (scenario 1, a stored token, contract sections 1 to 6 and 8): the configuration `aiplex connect` writes
and the real `by` parser; both wires, streamed, refused, forged headers; Branchyard's events and AIPlex's records
joined by id; the refusal's finality; a plain backend beside the AIPlex ones; an AIPlex outage; the token's
whereabouts. Does not cover: scenario 2 (a Branchyard-signed token, --federated); a real model or coding agent;
traceparent; a cloud AIPlex; a Branchyard server; other operating systems. The Branchyard commit is recorded as
stated, not verified (`by --version` prints none); the AIPlex commit is read from the binary and must match
--aiplex-commit when that is given.
"""


def parse_args(argv: list[str] | None) -> argparse.Namespace:
    """The command line."""
    today = datetime.datetime.now(datetime.UTC).date().isoformat()
    parser = argparse.ArgumentParser(
        description=(__doc__ or "").split("\n\n")[0],
        epilog=EPILOG,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--aiplex", required=True, help="the `aiplex` binary (`aiplex-api` must be beside it)")
    parser.add_argument("--by", required=True, help="the `by` binary")
    parser.add_argument("--fake-agent", required=True, help="the `fake-acp-agent` binary")
    parser.add_argument("--out", default=f"docs/evidence/aiplex-control-plane-{today}.json", help="the evidence record")
    parser.add_argument("--keep", action="store_true", help="keep the scratch directory for inspection")
    parser.add_argument("--port", type=int, default=0, help="the port for `aiplex dev` (default: a free one)")
    parser.add_argument("--aiplex-commit", default=None, help="checked against the commit `aiplex version` reports")
    parser.add_argument("--branchyard-commit", default="unknown", help="the Branchyard commit of the binaries (stated)")
    parser.add_argument(
        "--federated", action="store_true", help="scenario 2, a Branchyard-signed token: not implemented yet"
    )
    args = parser.parse_args(argv)
    if args.federated:
        parser.error("--federated (scenario 2, a Branchyard-signed token) is not implemented yet")
    for name in ("aiplex", "by", "fake_agent"):
        path = Path(getattr(args, name)).resolve()
        if not (path.is_file() and os.access(path, os.X_OK)):
            parser.error(f"--{name.replace('_', '-')}: {path} is not an executable file")
        setattr(args, name, str(path))
    return args


def become_subreaper() -> bool:
    """Make this process the parent of any descendant that is orphaned (Linux's PR_SET_CHILD_SUBREAPER), so a
    process a command leaves behind, in another session and with another HOME, is still found by descent."""
    try:
        return ctypes.CDLL(None, use_errno=True).prctl(36, 1, 0, 0, 0) == 0
    except (OSError, AttributeError):
        return False


def reap_children() -> None:
    """Collect the children that exited (the orphans this process adopted)."""
    while True:
        try:
            pid, _status = os.waitpid(-1, os.WNOHANG)
        except ChildProcessError:
            return
        if pid == 0:
            return


def on_signal(signum: int, _frame: Any) -> NoReturn:
    """Turn SIGTERM and SIGHUP into the interrupt Ctrl-C raises, so `finally` runs."""
    raise Interrupted(f"signal {signum}")


def main(argv: list[str] | None = None) -> int:
    """Run the scenario; return the exit status."""
    args = parse_args(argv)
    become_subreaper()
    for sig in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, on_signal)
    ceiling = Path(tempfile.gettempdir()).resolve()
    scratch = Path(tempfile.mkdtemp(prefix="qualify-aiplex-", dir=ceiling))
    run = Run(args, scratch)
    run.home.mkdir()
    (scratch / "user").mkdir()
    run.env = hermetic_env(os.environ, run.home, scratch / "user" / "config.toml", ceiling)
    code = 0
    try:
        run.say("scratch directory <scratch>")
        scenario_one(run)
        step(run, "secret-hygiene", run.check_secret_hygiene)
        report = run.cleanup()
        problems = cleanup_problems(report)
        run.check(
            "cleanup",
            "`aiplex dev stop` ran and exited 0, and no process of the run (found by its HOME, by descent from "
            "the script and by process group) was left for the sweep to kill",
            not problems,
            problems=problems,
            **report,
        )
        command = [Path(sys.argv[0]).name, *(argv if argv is not None else sys.argv[1:])]
        command = [Path(c).name if c.startswith("/") else c for c in command]
        write_record(run, Path(args.out), command)
        failed = [c.id for c in run.checks if not c.passed]
        run.say(f"{len(run.checks) - len(failed)} of {len(run.checks)} checks passed; record {args.out}")
        if failed:
            run.say("failed: " + ", ".join(failed))
        code = 1 if failed else 0
    except SetupError as error:
        run.say(f"error: {error}")
        code = 2
    except KeyboardInterrupt:
        run.say("interrupted")
        code = 130
    finally:
        left = run.cleanup() if not run.cleaned else {}
        if left.get("processes_left"):
            run.say(f"warning: {left['processes_left']} process(es) could not be stopped")
        if args.keep:
            run.say(f"scratch directory kept: {scratch}")
        else:
            shutil.rmtree(scratch, ignore_errors=True)
    return code


if __name__ == "__main__":
    sys.exit(main())
