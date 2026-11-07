# Delegate reference: Asking for a result

Read when: you want a child's answer as JSON: --result-schema, --result-deliverable, file artifacts, compare and judge.

## Asking for a result

Read a child's answer as JSON instead of prose by saying its shape at spawn:
`--result-schema '<JSON Schema>'` (or `--result-schema-file PATH`), with
`--result-required` when it must answer. The schema may use only `type`
(`object`, `array`, `string`, `number`, `integer`, `boolean`, `null`, or a
list of them), `properties`, `required`, `additionalProperties`, `items`,
`enum`, `const`, `minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum`,
`minLength`, `maxLength`, `pattern`, `minItems`, `maxItems`, `uniqueItems`,
`anyOf` and `oneOf`, and annotations such as `title` and `description`; any
other keyword, such as `$ref`, `allOf` or `format`, is refused by name. The
child reports with `by report "<text>" --result '<JSON>'`, which is refused,
naming the path (`result.files[2]: expected string, got number`), when the
JSON does not match. You read it as `result` in `by inspect <child> --json` and in each
settled child of `by wait --json`; a child that ends its turn without a
required result leaves a warning in your log. In Python,
`branchyard.spawn(..., result={"schema": {...}, "required": True})` and
`branchyard.inspect(child).result`; the MCP `spawn` tool takes `result`.
A result is the child's claim. A schema validates its shape, not its truth.

With `--result-deliverable`, a schema is required and the native final JSON
can be the output itself. The same flags work on `by run` and `by fan`.
Branchyard captures it only after successful native completion, and prefers
an explicit accepted result from that turn. Earlier-turn or invalid output
does not qualify. No Git change, reporting tool, automatic retry, or native
restart is required. A seat uses `result = { purpose = "deliverable",
required = true, schema = {...} }` for this contract.

For file deliverables, add `--result-artifacts outputs/artifacts.json`.
After checking the actual files, use the installed artifact-tools skill's
`manifest` command to list files below that manifest's directory. Optional
`--result-artifacts-max-files N` and `--result-artifacts-max-bytes N` narrow
the collection limits. Collection completes after workspace transfer and
before result acceptance; failure refuses the deliverable. Published files
belong to this producing turn. Share them through existing artifact grants
and handoffs. MCP and SDK contracts use `capture = { manifest =
"outputs/artifacts.json" }` inside the required deliverable result contract.

Compare and judge can rank valid result-only deliverables. Ordinary report
metadata does not turn unchanged code into a candidate. Deterministic ranking
checks structural evidence, cost, and time. Review the evidence yourself.
`by judge --require-judge` leaves no pick if its required model judge cannot
supply a valid verdict. Git-only `--pick` and `--into` refuse result-only winners.
Code integration still uses `by check` and `by integrate`.
