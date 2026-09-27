"""Validate real request/response payloads against `schema/contract.json`,
with a minimal JSON Schema validator (stdlib only: no `jsonschema` package).

The payloads come from `docs/server.md`'s own worked examples: the exact
bytes `by --remote` sends and receives, and the Python SDK receives the
same shapes back through `by ... --json` (`docs/sdk.md#plugin-and-standalone-skill`,
`docs/server.md`). A schema that cannot validate its own documented
examples is wrong, so this doubles as `schema/contract.json`'s sanity
check, besides `crates/branchyard-client/tests/contract.rs`'s freshness
check.
"""
import json
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
CONTRACT = json.loads((ROOT / "schema/contract.json").read_text())
TYPES = CONTRACT["types"]


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
    `properties`/`required`/`additionalProperties`, `items`, `enum`,
    `const`, and `oneOf`/`anyOf`. Anything else (`format`, `minimum`, ...)
    is intentionally not checked; this is a structural check, not a full
    validator.
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
        if schema.get("additionalProperties") is False:
            extra = set(value) - set(properties)
            if extra:
                raise ValidationError(f"{path}: unknown properties {sorted(extra)}")
        for name, sub in value.items():
            if name in properties:
                validate(sub, properties[name], defs, f"{path}.{name}")
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


def _graph_proposal() -> dict:
    """The proposal in `docs/graph.md`'s first ```json fence."""
    text = (ROOT / "docs/graph.md").read_text()
    return json.loads(re.findall(r"```json\n(.*?)\n```", text, re.S)[0])


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
                {"branch": "r", "revision": 0, "children": [], "dependencies": [
                    {"dependent": "a", "prerequisite": "b", "after": "later"}]},
                "Graph",
            )


if __name__ == "__main__":
    unittest.main()
