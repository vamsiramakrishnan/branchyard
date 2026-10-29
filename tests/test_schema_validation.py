"""Validate real request/response payloads against `schema/contract.json`,
with a minimal JSON Schema validator (stdlib only: no `jsonschema` package).

The payloads come from `docs/server.md`'s own worked examples: the exact
bytes `by --remote` sends and receives, and the Python SDK receives the
same shapes back through `by ... --json` (`docs/delegation.md#python`,
`docs/server.md`; `docs/sdk.md` says which module is for what). A schema that cannot validate its own documented
examples is wrong, so this doubles as `schema/contract.json`'s sanity
check, besides `crates/branchyard-client/tests/contract.rs`'s freshness
check.
"""

import json
import re
import tomllib
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CONTRACT = json.loads((ROOT / "schema/contract.json").read_text())
TYPES = CONTRACT["types"]
SERVER_CONFIG_SCHEMA = json.loads((ROOT / "schema/server.config.json").read_text())
RIG_SCHEMA = json.loads((ROOT / "schema/rig.json").read_text())


class ValidationError(Exception):
    pass


def _defs(schema_doc):
    return schema_doc.get("$defs", {})


def _resolve(ref, defs):
    name = ref.rsplit("/", 1)[-1]
    if name not in defs:
        raise ValidationError(f"unresolved $ref {ref!r}")
    return defs[name]


def validate(value, schema, defs, path="$"):
    """A minimal, non-exhaustive validator for the subset of JSON Schema
    (draft 2020-12) `schemars` 1.x emits for these types: `$ref`, `type`,
    `properties`/`required`/`additionalProperties` (false, or a schema
    for the other properties), `propertyNames`' `pattern`, `items`,
    `enum`, `const`, and `oneOf`/`anyOf`. Anything else (`format`,
    `minimum`, ...) is intentionally not checked; this is a structural
    check, not a full validator.
    """
    if "$ref" in schema:
        return validate(value, _resolve(schema["$ref"], defs), defs, path)

    if "oneOf" in schema or "anyOf" in schema:
        branches = schema.get("oneOf") or schema.get("anyOf")
        errors = []
        for branch in branches:
            try:
                validate(value, branch, defs, path)
                return
            except ValidationError as error:
                errors.append(str(error))
        raise ValidationError(f"{path}: {value!r} matches none of {len(branches)} alternatives: {errors}")

    if "const" in schema:
        if value != schema["const"]:
            raise ValidationError(f"{path}: {value!r} != const {schema['const']!r}")
        return

    if "enum" in schema:
        if value not in schema["enum"]:
            raise ValidationError(f"{path}: {value!r} not in enum {schema['enum']!r}")
        return

    kind = schema.get("type")
    if kind == "object" or (kind is None and "properties" in schema):
        if not isinstance(value, dict):
            raise ValidationError(f"{path}: {value!r} is not an object")
        properties = schema.get("properties", {})
        for name in schema.get("required", []):
            if name not in value:
                raise ValidationError(f"{path}: missing required property {name!r}")
        additional = schema.get("additionalProperties")
        if additional is False:
            extra = set(value) - set(properties)
            if extra:
                raise ValidationError(f"{path}: unknown properties {sorted(extra)}")
        pattern = schema.get("propertyNames", {}).get("pattern")
        for name, sub in value.items():
            if pattern is not None and not re.search(pattern, name):
                raise ValidationError(f"{path}: property name {name!r} does not match {pattern!r}")
            if name in properties:
                validate(sub, properties[name], defs, f"{path}.{name}")
            elif isinstance(additional, dict):
                validate(sub, additional, defs, f"{path}.{name}")
        return

    if kind == "array":
        if not isinstance(value, list):
            raise ValidationError(f"{path}: {value!r} is not an array")
        if "items" in schema:
            for i, item in enumerate(value):
                validate(item, schema["items"], defs, f"{path}[{i}]")
        return

    if kind == "string":
        if not isinstance(value, str):
            raise ValidationError(f"{path}: {value!r} is not a string")
        return

    if kind == "boolean":
        if not isinstance(value, bool):
            raise ValidationError(f"{path}: {value!r} is not a boolean")
        return

    if kind in ("integer", "number"):
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise ValidationError(f"{path}: {value!r} is not a {kind}")
        return

    if kind == "null":
        if value is not None:
            raise ValidationError(f"{path}: {value!r} is not null")
        return

    # No `type` and none of the above keywords: an unconstrained schema
    # (schemars emits this for e.g. serde_json::Value); anything matches.


def validate_as(value, type_name):
    schema = TYPES[type_name]
    validate(value, schema, {**_defs(schema)})


def _fenced_json_after(marker: str) -> dict:
    """The JSON object in the first ```json fence in `docs/server.md` whose
    first line is `marker`."""
    text = (ROOT / "docs/server.md").read_text()
    for fence in re.findall(r"```json\n(.*?)\n```", text, re.S):
        lines = fence.split("\n", 1)
        if lines[0].strip() == marker:
            return json.loads(lines[1])
    raise AssertionError(f"no ```json fence starting with {marker!r} in docs/server.md")


class ContractCoversEveryEndpoint(unittest.TestCase):
    def test_workspace_wire_types_have_namespaced_contract_roots(self):
        source = ROOT / "crates/branchyard-client/src/workspace_api.rs"
        names = set(re.findall(r"^pub (?:struct|enum) ([A-Z][A-Za-z]+)", source.read_text(), re.MULTILINE))
        self.assertTrue(names)
        missing = sorted(f"workspace.{name}" for name in names if f"workspace.{name}" not in TYPES)
        self.assertEqual(missing, [], "workspace wire types missing from schema/contract.json")

    def test_every_json_response_type_is_a_contract_root(self):
        """Each `Json<T>` a server handler returns is published as a root,
        so a client can look up every documented response."""
        sources = [
            ROOT / "crates/branchyard-server/src/api.rs",
            ROOT / "crates/branchyard-server/src/storage_routes.rs",
            ROOT / "crates/branchyard-server/src/triggers/routes.rs",
        ]
        returned = set()
        for source in sources:
            for name in re.findall(r"Json<(?:[a-z_]+::)*([A-Z][A-Za-z]+)>", source.read_text()):
                returned.add(name)
        self.assertTrue(returned)
        missing = sorted(returned - set(TYPES))
        self.assertEqual(missing, [], "response types missing from schema/contract.json")


class ContractTypesValidateTheirDocumentedExamples(unittest.TestCase):
    def test_the_documented_task_request_validates(self):
        request = _fenced_json_after("POST /v1/repos/app/tasks")
        validate_as(request, "TaskRequest")

    def test_a_task_request_missing_the_only_required_field_is_refused(self):
        request = _fenced_json_after("POST /v1/repos/app/tasks")
        del request["prompt"]
        with self.assertRaises(ValidationError):
            validate_as(request, "TaskRequest")

    def test_an_unknown_field_is_refused_like_the_real_deny_unknown_fields(self):
        request = _fenced_json_after("POST /v1/repos/app/tasks")
        request["not_a_real_field"] = True
        with self.assertRaises(ValidationError):
            validate_as(request, "TaskRequest")

    def test_the_documented_operation_accepted_response_validates(self):
        operation = {
            "id": "op_1",
            "repo": "app",
            "kind": "task",
            "state": "queued",
            "branches": ["flaky"],
            "cursor": 41,
            "created_at_ms": 1790000000000,
        }
        validate_as(operation, "Operation")

    def test_share_and_scratch_bodies_from_the_route_table_validate(self):
        validate_as({"to": "reviewer"}, "ShareRequest")
        validate_as({"name": "shared-fixtures"}, "CreateScratchRequest")
        validate_as({"ok": True}, "Ack")


def _trigger_example(marker: str) -> dict:
    """The JSON object in the ```json fence in `docs/triggers.md` whose
    first line is `marker`."""
    text = (ROOT / "docs/triggers.md").read_text()
    for fence in re.findall(r"```json\n(.*?)\n```", text, re.S):
        lines = fence.split("\n", 1)
        if lines[0].strip() == marker:
            return json.loads(lines[1])
    raise AssertionError(f"no ```json fence starting with {marker!r} in docs/triggers.md")


class TriggerExamplesValidate(unittest.TestCase):
    def test_the_documented_trigger_spec_validates(self):
        validate_as(_trigger_example("POST /v1/triggers"), "TriggerSpec")

    def test_a_trigger_spec_with_an_unknown_field_or_when_is_refused(self):
        spec = _trigger_example("POST /v1/triggers")
        spec["schedule"] = "daily"
        with self.assertRaises(ValidationError):
            validate_as(spec, "TriggerSpec")
        spec = _trigger_example("POST /v1/triggers")
        spec["when"] = {"kind": "hourly"}
        with self.assertRaises(ValidationError):
            validate_as(spec, "TriggerSpec")

    def test_the_documented_run_validates(self):
        validate_as(_trigger_example("TriggerRun"), "TriggerRun")

    def test_the_documented_spec_that_delivers_to_a_branch_validates(self):
        spec = _trigger_example("POST /v1/triggers (a branch that lives on)")
        validate_as(spec, "TriggerSpec")
        self.assertEqual(spec["deliver"]["busy"], "steer")
        self.assertEqual(
            spec["deliver"]["lifecycle"],
            {"idle_minutes": 30, "dormant_days": 7, "delete_days": 30},
        )
        spec["deliver"]["busy"] = "drop"
        with self.assertRaises(ValidationError):
            validate_as(spec, "TriggerSpec")

    def test_a_lifecycle_is_part_of_deliver_and_names_only_its_three_thresholds(self):
        spec = _trigger_example("POST /v1/triggers (a branch that lives on)")
        spec["deliver"]["lifecycle"] = {"delete_days": 14}
        validate_as(spec, "TriggerSpec")
        spec["deliver"]["lifecycle"] = {"idle": 30}
        with self.assertRaises(ValidationError):
            validate_as(spec, "TriggerSpec")
        # Beside `deliver` it is an unknown field of the spec.
        spec = _trigger_example("POST /v1/triggers (a branch that lives on)")
        spec["lifecycle"] = spec["deliver"].pop("lifecycle")
        with self.assertRaises(ValidationError):
            validate_as(spec, "TriggerSpec")

    def test_a_lifecycle_note_is_a_run_state_and_a_lifecycle_activity_is_an_event(self):
        run = _trigger_example("TriggerRun")
        run["state"] = "lifecycle"
        validate_as(run, "TriggerRun")

        def events(state):
            lifecycle = {"state": state, "reason": "no turn ended for 7d", "idle_since_ms": 5}
            return {"events": [{"at_ms": 6, "activity": {"lifecycle": lifecycle}}], "cursor": 1}

        validate_as(events("dormant"), "BranchEvents")
        with self.assertRaises(ValidationError):
            validate_as(events("asleep"), "BranchEvents")

    def test_schedules_and_acks_validate(self):
        validate_as({"kind": "cron", "expr": "0 3 * * 1-5", "timezone": "Europe/Berlin"}, "When")
        validate_as({"kind": "interval", "seconds": 3600}, "When")
        validate_as({"duplicate": True}, "FireAck")
        validate_as({"ignored": "a GitHub ping"}, "FireAck")


class GrantEntriesTakeEitherForm(unittest.TestCase):
    """A connector grant entry is the `--connector` string or the object
    (`docs/connectors.md#grants`), wherever one goes on the wire."""

    def test_a_spawn_may_ask_in_either_form(self):
        validate_as(
            {"prompt": "p", "connectors": ["github:write:issues.*", {"connector": "linear", "mode": "read"}]},
            "SpawnRequest",
        )
        validate_as(
            {"prompt": "p", "provision": {"connectors": ["github@work:read:issues.list,pulls.list"]}},
            "TaskRequest",
        )

    def test_an_entry_that_is_neither_form_is_refused(self):
        # Through `provision.connectors`, whose array the validator checks
        # (a spawn's `connectors` is `array | null`, a type list it skips).
        for bad in [7, {"operations": ["*"]}, {"connector": "github", "scope": "x"}]:
            with self.assertRaises(ValidationError, msg=repr(bad)):
                validate_as({"prompt": "p", "provision": {"connectors": [bad]}}, "TaskRequest")


def _graph_proposal() -> dict:
    """The proposal in `docs/graph.md`'s first ```json fence."""
    text = (ROOT / "docs/graph.md").read_text()
    return json.loads(re.findall(r"```json\n(.*?)\n```", text, re.S)[0])


class ResultContractsValidate(unittest.TestCase):
    """A result contract is a spawn's `result`, a report's `result`, and
    what an inspection and a result event show back
    (`docs/delegation.md#result-contract`)."""

    CONTRACT = {  # noqa: RUF012
        "schema": {"type": "object", "required": ["files"], "properties": {"files": {"type": "array"}}},
        "required": True,
    }

    def test_a_spawn_may_ask_for_a_result_in_a_shape(self):
        validate_as({"prompt": "p", "result": self.CONTRACT}, "SpawnRequest")
        validate_as({"prompt": "p", "result": {"schema": {"type": "string"}}}, "SpawnRequest")
        validate_as({"prompt": "p"}, "SpawnRequest")

    def test_a_contract_names_only_its_schema_and_whether_it_is_required(self):
        for bad in [{"required": True}, {"schema": {}, "requried": True}, {"schema": {}, "required": "yes"}]:
            with self.assertRaises(ValidationError, msg=repr(bad)):
                validate_as({"prompt": "p", "result": bad}, "SpawnRequest")

    def test_a_graph_proposals_spawn_edit_takes_the_same_contract(self):
        proposal = {
            "expected_revision": 0,
            "edits": [{"kind": "spawn", "prompt": "p", "name": "a", "result": self.CONTRACT}],
        }
        validate_as(proposal, "GraphRequest")
        proposal["edits"][0]["result"] = {"schema": {}, "bogus": 1}
        with self.assertRaises(ValidationError):
            validate_as(proposal, "GraphRequest")

    def test_a_report_may_carry_a_result_of_any_shape(self):
        validate_as({"text": "done"}, "ReportRequest")
        for result in [{"files": ["a.rs"]}, ["a.rs"], "a.rs", 3, True, {}]:
            validate_as({"text": "done", "result": result}, "ReportRequest")
        with self.assertRaises(ValidationError):
            validate_as({"text": "done", "results": {}}, "ReportRequest")
        with self.assertRaises(ValidationError):
            validate_as({"result": {}}, "ReportRequest")

    def test_a_settled_child_in_a_wait_carries_its_contract_and_result(self):
        inspection = {
            "name": "kid",
            "status": {"state": "ready"},
            "harness": "claude-code",
            "profile": "claude-code-stream-json",
            "parent": "root",
            "children": [],
            "depth": 1,
            "turns": 1,
            "candidate": None,
            "cost_usd": None,
            "subtree_cost_usd": 0.0,
            "max_usd": None,
            "remaining_usd": None,
            "envelope": None,
            "last_message": "",
            "result_contract": self.CONTRACT,
            "result": {"files": ["a.rs"]},
        }
        validate_as({"settled": [inspection], "pending": []}, "Waited")
        inspection["result_contract"] = {"schema": {}, "required": "yes"}
        with self.assertRaises(ValidationError):
            validate_as({"settled": [inspection], "pending": []}, "Waited")

    def test_a_result_is_an_event_on_the_branch_that_reported_it(self):
        def events(activity):
            return {"events": [{"at_ms": 6, "activity": activity}], "cursor": 1}

        validate_as(events({"result": {"value": {"files": ["a.rs"]}}}), "BranchEvents")
        validate_as(events({"result": {"value": [1, 2]}}), "BranchEvents")
        with self.assertRaises(ValidationError):
            validate_as(events({"result": {"values": []}}), "BranchEvents")


class GraphTypesValidate(unittest.TestCase):
    def test_the_documented_graph_proposal_validates_as_a_graph_request(self):
        validate_as(_graph_proposal(), "GraphRequest")

    def test_an_unknown_edit_kind_or_field_is_refused(self):
        proposal = _graph_proposal()
        proposal["edits"][0]["kind"] = "rename"
        with self.assertRaises(ValidationError):
            validate_as(proposal, "GraphRequest")
        proposal = _graph_proposal()
        proposal["edits"][0]["colour"] = 1
        with self.assertRaises(ValidationError):
            validate_as(proposal, "GraphRequest")

    def test_a_spawn_that_waits_validates(self):
        validate_as(
            {
                "prompt": "Use the new column",
                "depends_on": ["schema"],
                "after": "integrated",
                "bindings": [{"scratch": "notes", "access": "exclusive_write"}],
            },
            "SpawnRequest",
        )
        with self.assertRaises(ValidationError):
            validate_as({"prompt": "p", "after": "eventually"}, "SpawnRequest")

    def test_a_graph_with_waiting_and_blocked_children_validates(self):
        validate_as(
            {
                "branch": "root",
                "revision": 2,
                "children": [
                    {"name": "schema", "status": {"state": "ready"}},
                    {"name": "api", "status": {"state": "waiting"}, "depends_on": ["schema"]},
                    {
                        "name": "docs",
                        "status": {"state": "blocked", "reason": "its prerequisite lint failed"},
                        "depends_on": ["lint"],
                    },
                ],
                "dependencies": [
                    {"dependent": "api", "prerequisite": "schema", "after": "integrated"},
                    {"dependent": "docs", "prerequisite": "lint"},
                ],
            },
            "Graph",
        )
        with self.assertRaises(ValidationError):
            validate_as(
                {
                    "branch": "r",
                    "revision": 0,
                    "children": [],
                    "dependencies": [{"dependent": "a", "prerequisite": "b", "after": "later"}],
                },
                "Graph",
            )


class ServerConfigExampleValidatesAgainstItsSchema(unittest.TestCase):
    """`deploy/config.example.json` against `schema/server.config.json`
    (generated from `crates/branchyard-server/src/config.rs`'s
    `FileConfig`; freshness is `crates/branchyard-server/tests/
    server_config_schema.rs`). This is the deploy recipe's own sanity
    check, besides `crates/branchyard-server/tests/deploy_config.rs`'s
    parse-with-the-real-loader check."""

    def _example(self):
        return json.loads((ROOT / "deploy/config.example.json").read_text())

    def test_the_example_validates(self):
        validate(
            self._example(),
            SERVER_CONFIG_SCHEMA,
            _defs(SERVER_CONFIG_SCHEMA),
        )

    def test_an_unknown_top_level_key_is_refused(self):
        example = self._example()
        example["not_a_real_field"] = True
        with self.assertRaises(ValidationError):
            validate(example, SERVER_CONFIG_SCHEMA, _defs(SERVER_CONFIG_SCHEMA))

    def test_the_dollar_schema_key_is_accepted(self):
        example = self._example()
        example["$schema"] = "https://example.invalid/server.config.json"
        validate(example, SERVER_CONFIG_SCHEMA, _defs(SERVER_CONFIG_SCHEMA))

    def test_a_wrongly_typed_field_is_refused(self):
        # A required, non-nullable field (unlike most of `FileConfig`'s
        # `Option<T>` fields, whose `["T", "null"]` schema type this
        # minimal validator does not itself enforce; see `validate`'s
        # docstring).
        example = self._example()
        example["repos"] = "not an object"
        with self.assertRaises(ValidationError):
            validate(example, SERVER_CONFIG_SCHEMA, _defs(SERVER_CONFIG_SCHEMA))

    def test_an_unknown_token_field_is_refused(self):
        example = self._example()
        example["tokens"][0]["colour"] = "blue"
        with self.assertRaises(ValidationError):
            validate(example, SERVER_CONFIG_SCHEMA, _defs(SERVER_CONFIG_SCHEMA))


class ExampleRigsValidateAgainstTheirSchema(unittest.TestCase):
    """`examples/rigs/*.toml` against `schema/rig.json` (generated from
    `crates/branchyard-cli/src/rig.rs`'s `Raw*` types; freshness is that
    file's `rig_json_is_generated_and_fresh`, behind the `schema`
    feature). The rig parser itself lowers the same examples to
    `crates/branchyard-cli/tests/golden/`."""

    def _examples(self):
        paths = sorted((ROOT / "examples/rigs").glob("*.toml"))
        self.assertGreaterEqual(len(paths), 2)
        return [(path.name, tomllib.loads(path.read_text())) for path in paths]

    def _check(self, rig):
        validate(rig, RIG_SCHEMA, _defs(RIG_SCHEMA))

    def test_every_example_validates(self):
        for name, rig in self._examples():
            with self.subTest(name):
                self._check(rig)

    def test_an_unknown_seat_field_is_refused(self):
        _, rig = self._examples()[0]
        next(iter(rig["seats"].values()))["collaborates_with"] = ["lead"]
        with self.assertRaises(ValidationError):
            self._check(rig)

    def test_an_unknown_top_level_field_is_refused(self):
        _, rig = self._examples()[0]
        rig["culture_file"] = "culture.md"
        with self.assertRaises(ValidationError):
            self._check(rig)

    def test_a_seat_name_must_be_usable(self):
        _, rig = self._examples()[0]
        rig["seats"]["Not A Name"] = {}
        with self.assertRaises(ValidationError):
            self._check(rig)

    def test_a_policy_default_outside_allow_deny_ask_is_refused(self):
        _, rig = self._examples()[0]
        rig["seats"][rig["root"]]["policy"] = {"default": "yolo"}
        with self.assertRaises(ValidationError):
            self._check(rig)

    def test_effort_is_a_name_or_a_level(self):
        _, rig = self._examples()[0]
        seat = rig["seats"][rig["root"]]
        seat["effort"] = 40
        self._check(rig)
        seat["effort"] = "huge"
        with self.assertRaises(ValidationError):
            self._check(rig)

    def test_the_version_is_required(self):
        _, rig = self._examples()[0]
        del rig["version"]
        with self.assertRaises(ValidationError):
            self._check(rig)


if __name__ == "__main__":
    unittest.main()
