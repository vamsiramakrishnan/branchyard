#!/usr/bin/env python3
"""tools/qualify_aiplex.py, the run of the real `by` against a real `aiplex dev`, on its pure parts:
the token never prints and is scrubbed from everything the script says or captures, `aiplex dev env`
and `by log --json` are read as they are printed, an event and a record are compared with what the
harness saw, the secret scan finds a token in a file, the environment of the programs is hermetic,
and a leftover process is told by its HOME. It starts no process and needs neither binary."""

import contextlib
import io
import json
import os
import sys
import tempfile
import unittest
from argparse import Namespace
from pathlib import Path
from typing import ClassVar

REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO / "tools"))
import qualify_aiplex as q  # noqa: E402

TOKEN = "eyJhbGciOiJFUzI1NiJ9.eyJzdWIiOiJkZXZlbG9wZXIifQ.c2lnbmF0dXJlLXNpZ25hdHVyZS1zaWduYXR1cmU"


def event(**changes):
    base = {
        "kind": "call",
        "model": "mock-claude",
        "api": "anthropic",
        "decision": "allowed",
        "backend": "aiplex-local-anthropic",
        "status": 200,
        "control_plane": {"plane": "aiplex", "call_id": "8yHq305eNci--FTNUp5v4Q", "request_id": "req-1-18"},
    }
    return {**base, **changes}


def seen(**changes):
    base = {
        "model": "mock-claude",
        "api": "anthropic",
        "status": 200,
        "stream": False,
        "aiplex_request_id": "req-1-18",
        "aiplex_error_code": None,
    }
    return {**base, **changes}


def refusal():
    plane = {
        "plane": "aiplex",
        "call_id": "3t2XFh0t4b_1SnhagWKkVw",
        "request_id": "req-1-21",
        "error_code": "MODEL_NOT_FOUND",
    }
    return event(
        model="mock-absent",
        decision="refused",
        status=404,
        reason="refused by aiplex (MODEL_NOT_FOUND)",
        control_plane=plane,
    )


def refusal_seen():
    return seen(model="mock-absent", status=404, aiplex_request_id="req-1-21", aiplex_error_code="MODEL_NOT_FOUND")


class Secrets(unittest.TestCase):
    def test_a_secret_prints_as_redacted_whatever_formats_it(self):
        secret = q.Secret(TOKEN)
        self.assertEqual(str(secret), q.REDACTED)
        self.assertEqual(repr(secret), q.REDACTED)
        self.assertEqual(
            f"{secret} {[secret]} {{'t': {secret!r}}}", f"{q.REDACTED} [{q.REDACTED}] {{'t': {q.REDACTED}}}"
        )
        self.assertEqual(secret.reveal(), TOKEN)

    def test_redact_removes_the_value_and_anything_shaped_like_a_signed_token(self):
        secret = q.Secret("plain-secret-value")
        other = "eyJhbGciOiJFZERTQSJ9.eyJpc3MiOiJ4In0.abcdefghijkl"
        text = f"Authorization: Bearer plain-secret-value and {TOKEN} and {other}"
        out = q.redact(text, [secret])
        self.assertNotIn("plain-secret-value", out)
        self.assertNotIn("eyJ", out)
        self.assertEqual(out.count(q.REDACTED), 3)
        self.assertEqual(q.redact("nothing here"), "nothing here")

    def test_leaks_sees_the_value_and_the_shape(self):
        self.assertTrue(q.leaks("x plain-secret-value y", [q.Secret("plain-secret-value")]))
        self.assertTrue(q.leaks(f"x {TOKEN}", []))
        self.assertFalse(q.leaks("a request id req-6ac3e6b12d627f8c-18", [q.Secret("plain-secret-value")]))
        self.assertFalse(q.leaks("anything", [q.Secret("")]))

    def test_the_run_scrubs_what_it_says(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = q.Run(Namespace(), Path(tmp))
            run.secrets.append(q.Secret(TOKEN))
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                run.say(f"the token is {TOKEN} and again {run.secrets[0]}")
                run.check("x", f"claim with {TOKEN}", False, seen=1)
            self.assertNotIn(TOKEN, out.getvalue())
            self.assertNotIn("eyJ", out.getvalue())
            self.assertIn("FAIL  x", out.getvalue())

    def test_the_scan_finds_the_token_whole_or_as_its_signature(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "sub").mkdir()
            (root / "clean.txt").write_text("nothing\n")
            (root / "sub" / "whole.log").write_text(f"x {TOKEN} y")
            (root / "sub" / "tail.bin").write_bytes(b"\0" + TOKEN.rsplit(".", 1)[-1].encode())
            (root / "link").symlink_to(root / "sub" / "whole.log")
            self.assertEqual(q.scan_for_secret(root, q.Secret(TOKEN)), ["sub/tail.bin", "sub/whole.log"])
            self.assertEqual(q.scan_for_secret(root, q.Secret("")), [])


class Reading(unittest.TestCase):
    def test_dev_env_reads_plain_exports_and_skips_references(self):
        text = (
            "export AIPLEX_URL='http://127.0.0.1:18452'\n"
            f"export AIPLEX_TOKEN='{TOKEN}'\n"
            'export OPENAI_API_KEY="$AIPLEX_TOKEN"\n'
            "export PLAIN=value\n"
            "not an export\n"
        )
        env = q.parse_dev_env(text)
        self.assertEqual(env["AIPLEX_URL"], "http://127.0.0.1:18452")
        self.assertEqual(env["AIPLEX_TOKEN"], TOKEN)
        self.assertEqual(env["PLAIN"], "value")
        self.assertNotIn("OPENAI_API_KEY", env)

    def test_events_are_read_as_an_array_or_as_lines(self):
        array = '[{"activity": "status"}, {"activity": "model", "model": {"kind": "call"}}]'
        lines = '{"activity": "status"}\n{"activity": "model", "model": {"kind": "gateway"}}\n'
        self.assertEqual(len(q.parse_events(array)), 2)
        self.assertEqual(len(q.parse_events(lines)), 2)
        self.assertEqual(q.parse_events(""), [])
        self.assertEqual(q.call_events(q.parse_events(array)), [{"kind": "call"}])
        self.assertEqual(q.call_events(q.parse_events(lines)), [])

    def test_records_are_read_from_the_page(self):
        self.assertEqual(q.parse_records('{"records": [{"id": "a"}, 3]}'), [{"id": "a"}])
        self.assertEqual(q.parse_records('{"records": []}'), [])
        self.assertEqual(q.parse_records('{"records": null}'), [])

    def test_the_expected_tags_come_from_branchyards_record_of_the_branch(self):
        show = {"name": "qualify", "harness": "gemini-cli", "turns": 1}
        self.assertEqual(q.expected_tags(show), {"by_branch": "qualify", "by_harness": "gemini-cli", "by_turn": "1"})
        self.assertIsNone(q.tags_of({"tags": None}))
        self.assertIsNone(q.tags_of({"tags": {}}))
        self.assertEqual(q.tags_of({"tags": {"by_turn": 1}}), {"by_turn": "1"})


class Comparing(unittest.TestCase):
    def test_a_good_allowed_call_has_no_problem(self):
        self.assertEqual(q.compare_event("a", event(), seen(), "allowed"), [])

    def test_a_good_refusal_has_no_problem_and_a_retry_after_one_is_one(self):
        self.assertEqual(q.compare_event("d", refusal(), refusal_seen(), "refused"), [])
        tried_on = refusal() | {"failed_over": ["aiplex-local-openai: 503"]}
        problems = q.compare_event("d", tried_on, refusal_seen(), "refused")
        self.assertEqual(len(problems), 1)
        self.assertIn("another backend was tried", problems[0])

    def test_each_wrong_field_is_named(self):
        wrong_id = event(control_plane={"plane": "aiplex", "call_id": "short", "request_id": "req-9-9"})
        problems = q.compare_event("a", wrong_id, seen(), "allowed")
        self.assertFalse(any("the harness" in p for p in problems))  # the harness never sees AIPlex's request id
        self.assertTrue(any("22 characters" in p for p in problems))
        no_plane = event(control_plane=None)
        self.assertTrue(any("no control_plane aiplex" in p for p in q.compare_event("a", no_plane, seen(), "allowed")))
        self.assertTrue(q.compare_event("a", event(decision="refused"), seen(), "allowed"))
        self.assertTrue(q.compare_event("d", event(), refusal_seen(), "refused"))

    def test_a_record_joins_by_both_ids_and_names_the_right_wire(self):
        record = {
            "request_id": "req-1-18",
            "client_request_id": "8yHq305eNci--FTNUp5v4Q",
            "model_id": "mock-claude",
            "operation": "compat.messages",
            "streaming": False,
            "http_status": 200,
        }
        self.assertEqual(q.compare_record("a", record, event(), seen(), "allowed"), [])
        wrong = record | {"client_request_id": "forged-id", "operation": "compat.chat-completions", "streaming": True}
        self.assertEqual(len(q.compare_record("a", wrong, event(), seen(), "allowed")), 3)

    def test_a_refusals_record_needs_an_operation_but_not_a_wire(self):
        record = {
            "request_id": "req-1-21",
            "client_request_id": "3t2XFh0t4b_1SnhagWKkVw",
            "model_id": "mock-absent",
            "operation": "unknown-model",
            "status": "error",
            "error_class": "model_not_found",
            "streaming": False,
            "http_status": 404,
        }
        self.assertEqual(q.compare_record("d", record, refusal(), refusal_seen(), "refused"), [])
        self.assertTrue(q.compare_record("d", record | {"operation": ""}, refusal(), refusal_seen(), "refused"))
        for mutated in ({"status": "ok"}, {"error_class": None}):
            problems = q.compare_record("d", record | mutated, refusal(), refusal_seen(), "refused")
            self.assertEqual(len(problems), 1)
            self.assertIn("not a refusal", problems[0])


class Hermetic(unittest.TestCase):
    def test_the_environment_is_private_and_holds_no_credential(self):
        base = {
            "PATH": "/usr/bin",
            "BRANCHYARD_SERVER": "x",
            "AIPLEX_TOKEN": "t",
            "AIPLEX_URL": "u",
            "ANTHROPIC_API_KEY": "k",
            "GOOGLE_CLOUD_PROJECT": "p",
            "HOME": "/root",
        }
        env = q.hermetic_env(base, Path("/s/home"), Path("/s/user/config.toml"), Path("/tmp"))
        self.assertEqual(env["HOME"], "/s/home")
        self.assertEqual(env["BRANCHYARD_USER_CONFIG"], "/s/user/config.toml")
        self.assertEqual(env["GIT_CEILING_DIRECTORIES"], "/tmp")
        self.assertEqual(env["PATH"], "/usr/bin")
        for name in ("BRANCHYARD_SERVER", "AIPLEX_TOKEN", "AIPLEX_URL", "ANTHROPIC_API_KEY", "GOOGLE_CLOUD_PROJECT"):
            self.assertNotIn(name, env)
        self.assertIn("127.0.0.1", env["NO_PROXY"])
        self.assertEqual(base["HOME"], "/root")

    def test_a_process_is_the_scripts_when_it_carries_the_private_home(self):
        blob = b"PATH=/usr/bin\0HOME=/s/home\0X=1\0"
        self.assertTrue(q.environ_has_home(blob, "/s/home"))
        self.assertFalse(q.environ_has_home(blob, "/s/hom"))
        self.assertFalse(q.environ_has_home(b"HOME=/s/home2\0", "/s/home"))

    def test_stat_fields_survive_a_name_with_spaces_and_brackets(self):
        self.assertEqual(q.stat_fields("77 (a b) c) S 1 70 71 0"), ("S", 1, 70, 71, "a b) c"))

    def test_a_zombie_is_told_by_its_state(self):
        self.assertEqual(q.stat_state("6363 (aiplex) Z 1 6363 6363 0 -1"), "Z")
        self.assertEqual(q.stat_state("77 (a b) c) S 1 77 77 0"), "S")

    def test_this_process_is_not_among_those_with_a_home_nobody_has(self):
        self.assertEqual(q.processes_with_home(Path("/nonexistent/private/home")), [])
        self.assertNotIn(os.getpid(), [p["pid"] for p in q.processes_with_home(Path(os.environ.get("HOME", "/")))])


class Waiting(unittest.TestCase):
    def test_wait_until_returns_the_first_truthy_answer(self):
        answers = iter([[], [], ["x"]])
        now = [0.0]
        pauses = []

        def pause(seconds):
            pauses.append(seconds)
            now[0] += seconds

        self.assertEqual(q.wait_until(lambda: next(answers), 10, 0.5, lambda: now[0], pause), ["x"])
        self.assertEqual(pauses, [0.5, 0.5])

    def test_wait_until_gives_up_at_the_bound_with_the_last_answer(self):
        now = [0.0]

        def pause(seconds):
            now[0] += seconds

        self.assertEqual(q.wait_until(lambda: [], 1.0, 0.4, lambda: now[0], pause), [])
        self.assertGreaterEqual(now[0], 1.0)


class Command(unittest.TestCase):
    def refuses(self, *argv):
        err = io.StringIO()
        with contextlib.redirect_stderr(err), self.assertRaises(SystemExit) as raised:
            q.parse_args(list(argv))
        self.assertEqual(raised.exception.code, 2)
        return err.getvalue()

    def test_a_path_that_is_not_an_executable_file_is_a_usage_error(self):
        binary = sys.executable
        message = self.refuses("--aiplex", binary, "--by", "/nonexistent/by", "--fake-agent", binary)
        self.assertIn("--by", message)
        self.assertIn("not an executable file", message)

    def test_good_paths_are_resolved(self):
        binary = sys.executable
        args = q.parse_args(["--aiplex", binary, "--by", binary, "--fake-agent", binary, "--port", "18000"])
        self.assertEqual(args.port, 18000)
        self.assertTrue(Path(args.by).is_absolute())

    def test_the_harness_is_valid_python_and_holds_no_token(self):
        compile(q.HARNESS, "harness", "exec")
        self.assertNotIn("Bearer eyJ", q.HARNESS)
        self.assertNotIn('AIPLEX_TOKEN"]', q.HARNESS)

    def test_the_known_findings_name_checks_the_script_makes(self):
        for check_id in q.FINDINGS:
            self.assertTrue(check_id.startswith(("harness-", "record-tags-")))


def calls_seen(**changes):
    """What a harness reports for every call of CALLS when all goes as it should."""
    out = []
    for label, api, model, _flags, kind in q.CALLS:
        base = {
            "label": label,
            "api": api,
            "model": model,
            "status": 200,
            "stream": False,
            "aiplex_request_id": None if kind == "direct" else f"req-1-{label}",
            "aiplex_error_code": None,
            "response_header_names": ["content-type"],
            "body_head": "stand-in" if kind == "direct" else "{}",
            "sent_headers": {},
        }
        out.append({**base, **changes.get(label, {})})
    return out


def events_for(seen_calls, **changes):
    """The `model` events Branchyard writes for `calls_seen()` when all goes as it should."""
    out = []
    for call, (label, api, model, _flags, kind) in zip(seen_calls, q.CALLS, strict=True):
        if kind == "direct":
            built = {"kind": "call", "model": model, "api": api, "decision": "allowed", "status": 200,
                     "backend": q.BACKEND_DIRECT}  # fmt: skip
        else:
            plane = {"plane": "aiplex", "call_id": (label * 22)[:22], "request_id": f"req-1-{label}"}
            built = {"kind": "call", "model": model, "api": api, "decision": "allowed", "status": 200,
                     "backend": q.BACKEND_ANTHROPIC, "control_plane": plane}  # fmt: skip
            if kind == "refused":
                call.update(status=404, aiplex_error_code="MODEL_NOT_FOUND")
                built.update(decision="refused", status=404, reason="refused by aiplex (MODEL_NOT_FOUND)")
                plane["error_code"] = "MODEL_NOT_FOUND"
        out.append({**built, **changes.get(label, {})})
    return out


class Mutations(unittest.TestCase):
    """Each check's condition on observations made false in the way a reviewer's shim would make them false."""

    def test_the_unmutated_observations_pass_every_condition(self):
        calls = calls_seen()
        self.assertEqual(q.event_problems(q.CALLS, calls, events_for(calls)), [])

    def test_item_1_a_run_with_no_refused_call_fails_even_if_the_harness_agrees(self):
        calls = calls_seen()
        events = events_for(calls)
        # the shim answers call d with a success: no refusal anywhere, and the harness reports it faithfully
        calls[3].update(status=200, aiplex_error_code=None)
        events[3].update(decision="allowed", status=200, reason=None)
        events[3]["control_plane"].pop("error_code")
        problems = q.event_problems(q.CALLS, calls, events)
        self.assertTrue(any("refused calls are []" in p for p in problems))

    def test_item_1_a_refusal_of_another_call_fails(self):
        calls = calls_seen()
        events = events_for(calls)
        events[0].update(decision="refused", reason="refused by aiplex (MODEL_NOT_FOUND)")
        events[0]["control_plane"]["error_code"] = "MODEL_NOT_FOUND"
        self.assertTrue(any("refused calls are ['a', 'd']" in p for p in q.event_problems(q.CALLS, calls, events)))

    def test_item_1_a_harness_that_names_another_model_is_caught(self):
        calls = calls_seen()
        calls[3]["model"] = "mock-claude"
        problems = q.event_problems(q.CALLS, calls, events_for(calls_seen()))
        self.assertTrue(any("another call than the one the script made" in p for p in problems))

    def test_item_1_a_second_attempt_after_a_refusal_fails_both_ways(self):
        record = {"id": "ul_1"}
        event = refusal() | {"backend": q.BACKEND_ANTHROPIC}
        self.assertEqual(q.refusal_final_problems(event, [record]), [])
        retried = event | {"failed_over": [f"{q.BACKEND_ANTHROPIC}: 404"], "backend": q.BACKEND_SECOND}
        self.assertTrue(q.refusal_final_problems(retried, [record]))
        # Branchyard hides the retry from its event, but AIPlex saw two attempts under one id
        problems = q.refusal_final_problems(event, [record, {"id": "ul_2"}])
        self.assertEqual(problems, ["AIPlex holds 2 records for the call's id, not 1"])
        self.assertTrue(q.refusal_final_problems(event, []))

    def test_item_2_forged_headers_that_were_never_sent_fail(self):
        call = {"sent_headers": {"x-client-request-id": q.FORGED_ID, "x-aiplex-tags": q.FORGED_TAGS}}
        record = {"client_request_id": "c" * 22, "tags": {"by_branch": "qualify"}}
        self.assertEqual(q.forged_problems(call, record, [], "c" * 22), [])
        problems = q.forged_problems({"sent_headers": {}}, record, [], "c" * 22)
        self.assertEqual(len(problems), 2)
        one = q.forged_problems({"sent_headers": {"x-client-request-id": q.FORGED_ID}}, record, [], "c" * 22)
        self.assertEqual(one, ["the harness did not send the forged X-AIPlex-Tags"])

    def test_item_2_forged_values_in_aiplexs_side_fail(self):
        call = {"sent_headers": {"x-client-request-id": q.FORGED_ID, "x-aiplex-tags": q.FORGED_TAGS}}
        leaked = {"client_request_id": q.FORGED_ID, "tags": {"by_branch": "forged"}}
        self.assertEqual(len(q.forged_problems(call, leaked, [{"id": 1}], "c" * 22)), 3)

    def test_item_3_a_leftover_is_a_failure_whoever_killed_it(self):
        clean = {"aiplex_dev_stop_exits": [0], "killed_by_the_sweep": [], "processes_left": 0}
        self.assertEqual(q.cleanup_problems(clean), [])
        killed = clean | {"killed_by_the_sweep": [{"pid": 9, "name": "sleep"}]}
        self.assertEqual(q.cleanup_problems(killed), ["the sweep had to kill sleep (pid 9)"])
        self.assertTrue(q.cleanup_problems(clean | {"aiplex_dev_stop_exits": [1]}))
        self.assertTrue(q.cleanup_problems(clean | {"processes_left": 1}))

    def test_item_3_processes_are_found_by_descent_group_or_home_not_by_home_alone(self):
        def row(pid, ppid, state="S", pgrp=1, sid=1, name="x"):
            return {"pid": pid, "ppid": ppid, "state": state, "pgrp": pgrp, "sid": sid, "name": name}

        table = [
            row(100, 1, name="me"),
            row(101, 100, name="child"),
            row(102, 101, name="grandchild"),
            row(200, 1, pgrp=555, name="orphan-in-our-group"),
            row(201, 1, sid=555, name="orphan-in-our-session"),
            row(300, 1, name="other-home"),
            row(301, 1, name="stranger"),
            row(302, 100, state="Z", name="zombie"),
        ]
        found = q.select_processes(table, lambda r: r["pid"] == 300, 100, {555})
        self.assertEqual(
            sorted(p["name"] for p in found),
            sorted(["child", "grandchild", "orphan-in-our-group", "orphan-in-our-session", "other-home"]),
        )
        self.assertNotIn("stranger", [p["name"] for p in found])

    def test_item_4_a_missing_helper_never_passes(self):
        env = {
            "helpers_received": {"token_digests": 2, "aiplex_port": True},
            "aiplex_token_variable_set": False,
            "variables_holding_the_token": [],
            "variables_naming_aiplex": [],
            "anthropic_base_url": "http://127.0.0.1:1/anthropic",
            "openai_base_url": "http://127.0.0.1:1/openai/v1",
        }
        self.assertEqual(q.harness_token_problems(env), [])
        self.assertEqual(q.gateway_only_problems(env), [])
        no_digest = env | {"helpers_received": {"token_digests": 0, "aiplex_port": False}}
        self.assertTrue(q.harness_token_problems(no_digest))
        self.assertTrue(q.gateway_only_problems(no_digest))
        self.assertTrue(q.harness_token_problems({}))
        self.assertTrue(q.harness_token_problems(env | {"aiplex_token_variable_set": True}))
        self.assertTrue(q.harness_token_problems(env | {"variables_holding_the_token": ["X"]}))
        # The copy `by` is started with, under a name no list knows: a gateway that hides the credential only by
        # the configured name leaves it in the harness's environment, and the check names it.
        self.assertFalse(q.TOKEN_COPY_VARIABLE.startswith("QUALIFY_"))
        self.assertEqual(
            q.harness_token_problems(env | {"variables_holding_the_token": [q.TOKEN_COPY_VARIABLE]}),
            [f"the token is in {q.TOKEN_COPY_VARIABLE}"],
        )
        self.assertTrue(q.gateway_only_problems(env | {"variables_naming_aiplex": ["URL"]}))

    def test_item_4_the_window_search_finds_a_glued_copy_and_not_an_absent_one(self):
        digests = [tuple(p.split(":")) for p in q.token_digests(TOKEN).split(",")]
        self.assertEqual(len(digests), 2)
        self.assertTrue(q.window_hits(TOKEN, digests))
        self.assertTrue(q.window_hits("Bearer " + TOKEN + ";x=1", digests))
        self.assertTrue(q.window_hits("a" * 40 + TOKEN.rsplit(".", 1)[-1] + "b", digests))
        self.assertFalse(q.window_hits("Bearer " + TOKEN[:-3] + ";x=1", digests[:1]))
        self.assertFalse(q.window_hits("", digests))
        self.assertNotIn(TOKEN, q.token_digests(TOKEN))

    def test_item_6_the_direct_provider_conditions(self):
        calls = calls_seen()
        stand_in = [{"model": q.MODEL_DIRECT, "token_seen": False}, {"model": q.MODEL_UNROUTED, "token_seen": False}]
        records = [{"model_id": "mock-claude"}] * 5
        self.assertEqual(q.direct_problems(calls, stand_in, records, 5), [])
        self.assertTrue(q.direct_problems(calls, stand_in[:1], records, 5))
        self.assertTrue(q.direct_problems(calls, [dict(stand_in[0], token_seen=True), stand_in[1]], records, 5))
        self.assertTrue(q.direct_problems(calls, stand_in, [*records, {"model_id": q.MODEL_DIRECT}], 5))

    def test_item_6_an_outage_that_reaches_the_direct_provider_fails(self):
        call = {"status": 502, "body_head": "Branchyard's model gateway: no backend answered"}
        event = {"decision": "failed", "status": 502, "backend": q.BACKEND_ANTHROPIC, "reason": q.BACKEND_ANTHROPIC}
        self.assertEqual(q.outage_problems(call, [event], 2, 2), [])
        self.assertTrue(q.outage_problems(call, [event], 2, 3))
        self.assertTrue(q.outage_problems(call, [event | {"reason": q.BACKEND_DIRECT}], 2, 2))
        self.assertTrue(q.outage_problems(call | {"status": 200}, [event], 2, 2))
        self.assertTrue(q.outage_problems(call | {"aiplex_error_code": "X"}, [event], 2, 2))

    def test_item_7_forbidden_response_headers_on_aiplex_answers_only(self):
        calls = calls_seen(a={"response_header_names": ["content-type", "www-authenticate"]})
        self.assertEqual(q.response_header_problems(calls), ["a: www-authenticate (including www-authenticate)"])
        unlisted = calls_seen(b={"response_header_names": ["content-type", "x-aiplex-request-id", "x-custom"]})
        self.assertEqual(
            q.response_header_problems(unlisted), ["b: x-aiplex-request-id, x-custom (including x-aiplex-request-id)"]
        )
        allowed = [
            "content-type",
            "retry-after",
            "anthropic-ratelimit-requests",
            "x-codex-primary",
            "date",
            "content-length",
        ]
        self.assertEqual(q.response_header_problems(calls_seen(a={"response_header_names": allowed})), [])
        direct = calls_seen(f={"response_header_names": ["server", "via"]})
        self.assertEqual(q.response_header_problems(direct), [])

    def test_item_8a_validate_needs_its_ok_line_not_just_exit_zero(self):
        self.assertTrue(q.validate_ok(0, "ok       /s/repo/branchyard.toml\n"))
        self.assertFalse(q.validate_ok(0, ""))
        self.assertFalse(q.validate_ok(1, "ok       /s/repo/branchyard.toml\n"))
        self.assertFalse(q.validate_ok(0, "ok       /s/repo/other.toml\n"))


class Versions(unittest.TestCase):
    LINES: ClassVar[list[str]] = ["aiplex version f4e3991", "  commit:   f4e3991", "  console:  embedded"]

    def test_a_clean_build_that_matches_is_accepted(self):
        self.assertIsNone(q.aiplex_commit_problem(self.LINES, None))
        self.assertIsNone(q.aiplex_commit_problem(self.LINES, "f4e3991"))
        self.assertIsNone(q.aiplex_commit_problem(self.LINES, "f4e3991abc"))

    def test_a_dirty_or_different_or_unreadable_build_is_refused(self):
        dirty = ["aiplex version 45bd484-dirty", "  commit:   45bd484"]
        self.assertIn("dirty", q.aiplex_commit_problem(dirty, None) or "")
        self.assertIn("6466058", q.aiplex_commit_problem(self.LINES, "6466058") or "")
        self.assertIn("no `commit:`", q.aiplex_commit_problem(["aiplex version dev"], None) or "")

    def test_paths_are_replaced_longest_first(self):
        text = "/s/home/x /s/home/x/y /bin/by"
        out = q.sanitize(text, {"/s/home": "<h>", "/s/home/x": "<hx>", "/bin": "<b>"})
        self.assertEqual(out, "<hx> <hx>/y <b>/by")

    def test_file_hash_is_sha256(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "f"
            path.write_bytes(b"abc")
            self.assertEqual(q.sha256_file(path), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")

    def test_a_free_port_is_a_port(self):
        self.assertTrue(1024 <= q.free_port() <= 65535)


class Recording(unittest.TestCase):
    def test_the_record_holds_no_token_and_no_path_of_this_machine(self):
        with tempfile.TemporaryDirectory() as tmp:
            scratch = Path(tmp) / "scratch"
            scratch.mkdir()
            args = Namespace(aiplex="/opt/bin/aiplex", by="/opt/by/by", fake_agent="/opt/by/fake", federated=False)
            run = q.Run(args, scratch)
            run.secrets.append(q.Secret(TOKEN))
            run.versions = {"v": f"{scratch}/x"}
            run.check("c", "claim", False, where=f"{scratch}/home", bin="/opt/bin/aiplex-api")
            out = Path(tmp) / "out" / "record.json"
            with contextlib.redirect_stdout(io.StringIO()):
                q.write_record(run, out, ["qualify_aiplex.py"])
            text = out.read_text()
            self.assertIn("<scratch>/home", text)
            self.assertIn("<aiplex-bin>/aiplex-api", text)
            self.assertNotIn(tmp, text)
            self.assertIn('"result": "fail"', text)
            self.assertIn("failed: 1 of 1", text)

    def test_the_record_is_not_written_when_it_would_hold_the_token(self):
        with tempfile.TemporaryDirectory() as tmp:
            args = Namespace(aiplex="/a/aiplex", by="/a/by", fake_agent="/a/f", federated=False)
            run = q.Run(args, Path(tmp))
            run.secrets.append(q.Secret("plain-secret-value"))
            run.observations["oops"] = "plain-secret-value"
            out = Path(tmp) / "record.json"
            with self.assertRaises(q.SetupError):
                q.write_record(run, out, [])
            self.assertFalse(out.exists())


def b64(obj):
    import base64
    import json

    return base64.urlsafe_b64encode(json.dumps(obj).encode()).rstrip(b"=").decode()


def jwt_of(claims):
    return f"{b64({'alg': 'EdDSA'})}.{b64(claims)}.c2lnbmF0dXJl"


class SignedToken(unittest.TestCase):
    """Scenario 2: each check's condition on observations made false the way a reviewer's shim would."""

    CONFIG: ClassVar[dict] = {
        "models": {
            "backends": {
                "a": {"api": "anthropic", "url": "u", "control_plane": "aiplex", "auth": "turn-token", "audience": "x"}
            }
        }
    }

    def test_item_1_a_server_configuration_with_a_key_a_plain_backend_or_secrets_is_refused(self):
        self.assertEqual(q.server_config_problems(self.CONFIG), [])
        backend = self.CONFIG["models"]["backends"]["a"]
        for mutated in (
            {"models": {"backends": {"a": backend | {"key": "k"}}}},
            {"models": {"backends": {"a": {k: v for k, v in backend.items() if k != "control_plane"}}}},
            {"models": {"backends": {"a": backend | {"auth": None}}}},
            {"models": {"backends": {"a": backend | {"audience": ""}}}},
            {"models": {"backends": {}}},
            {"models": {"backends": {"a": backend}}, "secrets": {"aiplex": "AIPLEX_TOKEN"}},
        ):
            self.assertTrue(q.server_config_problems(mutated), mutated)

    def test_item_1_the_token_anywhere_in_the_server_is_found(self):
        token = q.Secret("tok-value-0123456789")
        environ = b"PATH=/bin\0HOME=/h\0"
        self.assertEqual(q.server_secret_problems(environ, b"by\0serve\0", "{}", token, []), [])
        named = q.server_secret_problems(environ + b"AIPLEX_TOKEN=other\0", b"", "{}", token, [])
        self.assertEqual(named, ["AIPLEX_TOKEN is in the server's environment"])
        self.assertTrue(q.server_secret_problems(environ + b"X=tok-value-0123456789\0", b"", "{}", token, []))
        self.assertTrue(q.server_secret_problems(environ, b"--token\0tok-value-0123456789", "{}", token, []))
        self.assertTrue(q.server_secret_problems(environ, b"", '{"k": "tok-value-0123456789"}', token, []))
        self.assertEqual(len(q.server_secret_problems(environ, b"", "{}", token, ["data/x.db"])), 1)
        self.assertTrue(q.server_secret_problems(b"", b"", "{}", token, []))  # an unreadable environment never passes

    def test_item_2_the_plan_must_show_the_issuer_subject_and_audience(self):
        plan = (
            "Identity plan: agent a\n  Issuer:   http://127.0.0.1:9\n  Subject:  branchyard:default\n  Audience: aud\n"
        )
        self.assertEqual(q.plan_problems(plan, "http://127.0.0.1:9", "aud"), [])
        self.assertEqual(len(q.plan_problems(plan, "http://127.0.0.1:99", "aud")), 1)
        self.assertEqual(len(q.plan_problems(plan, "http://127.0.0.1:9", "other")), 1)
        self.assertEqual(len(q.plan_problems("", "http://127.0.0.1:9", "aud")), 3)

    def test_item_3_a_record_must_name_the_agent_as_its_caller(self):
        record = {"caller_kind": "agent", "client_id": q.AGENT, "principal_display": "branchyard:default"}
        self.assertEqual(q.agent_record_problems("a", record), [])
        for mutated in ({"caller_kind": "user"}, {"client_id": "someone-else"}, {"principal_display": "dev@localhost"}):
            self.assertEqual(len(q.agent_record_problems("a", record | mutated)), 1)

    def test_item_3_the_probe_needs_the_grant_refusal_not_a_credential_refusal(self):
        event = {"control_plane": {"error_code": "ACTION_DENIED"}}
        record = {"caller_kind": "agent", "client_id": q.AGENT, "principal_id": "principal-abc"}
        self.assertEqual(q.probe_problems(event, {"status": 403}, record), [])
        unauthenticated = {"control_plane": {"error_code": "UNAUTHENTICATED"}}
        self.assertTrue(q.probe_problems(unauthenticated, {"status": 401}, {}))
        self.assertTrue(q.probe_problems(event, {"status": 403}, record | {"caller_kind": "user"}))
        self.assertTrue(q.probe_problems(event, {"status": 403}, record | {"principal_id": ""}))

    def test_item_4_the_key_set_must_be_one_public_ed25519_key_and_have_been_fetched(self):
        key = {"kty": "OKP", "crv": "Ed25519", "x": "abc", "kid": "by-1", "alg": "EdDSA", "use": "sig"}
        body = json.dumps({"keys": [key]})
        self.assertEqual(q.jwks_problems(200, body), [])
        self.assertTrue(q.jwks_problems(404, ""))
        self.assertTrue(q.jwks_problems(200, "not json"))
        self.assertTrue(q.jwks_problems(200, json.dumps({"keys": []})))
        self.assertTrue(q.jwks_problems(200, json.dumps({"keys": [key, key]})))
        self.assertTrue(q.jwks_problems(200, json.dumps({"keys": [key | {"d": "secret"}]})))
        self.assertTrue(q.jwks_problems(200, json.dumps({"keys": [key | {"crv": "P-256"}]})))
        self.assertTrue(q.jwks_problems(200, json.dumps({"keys": [key | {"alg": "RS256"}]})))
        log = "INFO request{path=/.well-known/jwks.json status=200 latency_ms=3}: x\nINFO path=/healthz status=200\n"
        self.assertEqual(q.jwks_fetches(log), 1)
        self.assertEqual(q.jwks_fetches("INFO path=/.well-known/jwks.json status=404"), 0)
        self.assertEqual(q.jwks_fetches(""), 0)

    def test_item_4_the_negative_control_must_be_a_final_401_unauthenticated_with_no_record(self):
        event = {
            "decision": "refused",
            "status": 401,
            "backend": q.WRONG_AUDIENCE_BACKEND,
            "control_plane": {"error_code": "UNAUTHENTICATED"},
        }
        seen = {"status": 401, "response_header_names": ["content-type"]}
        self.assertEqual(q.negative_problems(event, seen, [], True), [])
        # every call is refused 401 (the binding is wrong everywhere): a 401 then shows nothing
        self.assertEqual(len(q.negative_problems(event, seen, [], False)), 1)
        # the call is waved through: AIPlex answers 200 and holds a record
        waved = event | {"decision": "allowed", "status": 200, "control_plane": {}}
        self.assertTrue(q.negative_problems(waved, {"status": 200}, [{"id": "ul"}], True))
        # refused for another reason, or retried on another backend, or a challenge leaks
        self.assertTrue(q.negative_problems(event | {"control_plane": {"error_code": "ACTION_DENIED"}}, seen, [], True))
        self.assertTrue(q.negative_problems(event | {"failed_over": ["x: 401"]}, seen, [], True))
        self.assertTrue(q.negative_problems(event | {"backend": q.BACKEND_ANTHROPIC}, seen, [], True))
        self.assertTrue(q.negative_problems(event, seen | {"response_header_names": ["www-authenticate"]}, [], True))

    def test_item_5_a_jwt_with_by_purpose_in_the_harness_fails_and_the_gateways_own_does_not(self):
        env = {"jwt_variables": [{"name": "ANTHROPIC_API_KEY", "by_purpose": False}]}
        self.assertEqual(q.jwt_variable_problems(env), [])
        self.assertEqual(q.jwt_variable_problems({"jwt_variables": []}), [])
        mutated = {"jwt_variables": [*env["jwt_variables"], {"name": "AIPLEX_JWT", "by_purpose": True}]}
        self.assertEqual(q.jwt_variable_problems(mutated), ["AIPLEX_JWT holds a token with by_purpose"])
        self.assertTrue(q.jwt_variable_problems({"jwt_variables": [{"name": "X", "by_purpose": None}]}))
        self.assertTrue(q.jwt_variable_problems({}))  # a harness that did not report never passes

    def test_item_5_the_scan_finds_a_signed_token_and_not_the_gateways_own(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "gw.log").write_text("x " + jwt_of({"sub": "t", "by_tenant": "default"}))
            self.assertEqual(q.signed_token_files(root), [])
            (root / "sub").mkdir()
            (root / "sub" / "signed.bin").write_bytes(b"\0" + jwt_of({"by_purpose": "aiplex"}).encode() + b"\0")
            self.assertEqual(q.signed_token_files(root), ["sub/signed.bin"])

    def test_the_harness_reports_its_jwt_variables_by_name_only(self):
        source = q.HARNESS
        self.assertIn("jwt_variables", source)
        compile(source, "harness", "exec")
        self.assertNotIn('"value"', source)

    def test_scenario_two_is_selected_by_federated_and_names_its_record(self):
        binary = sys.executable
        args = q.parse_args(["--aiplex", binary, "--by", binary, "--fake-agent", binary, "--federated"])
        self.assertTrue(args.federated)
        self.assertIn("aiplex-signed-token-", args.out)
        plain = q.parse_args(["--aiplex", binary, "--by", binary, "--fake-agent", binary])
        self.assertFalse(plain.federated)
        self.assertIn("aiplex-control-plane-", plain.out)

    def test_the_calls_of_scenario_two_have_the_same_kinds_as_scenario_one(self):
        for label, _api, _model, _flags, kind in q.CALLS2:
            self.assertEqual(q.KIND[label], kind)


if __name__ == "__main__":
    unittest.main()
