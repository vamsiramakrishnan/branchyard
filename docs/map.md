# Wide map

`by map` runs one prompt over every item of a list. Each item gets its own branch, so its own worktree, session and context. Each answer is checked against a JSON schema. The answers are collected into a table, and an optional reduce turn summarizes them. A map is recorded, so an interrupted one resumes where it stopped. It is the *wide map* track of the [roadmap](roadmap.md#wave-4).

> **Status.** Implemented in local mode and on the server (`--remote`), 1 October 2026. Tested hermetically against the fake ACP agent, which answers from files. No model has run a map yet: see [what is untested](#what-is-untested).

## Running a map

```text
by map PROMPT [--items FILE | --from-command CMD] [--input-format jsonl|json|csv|lines]
       [--schema FILE] [--out FILE.csv|FILE.jsonl] [--concurrency N] [--retries N]
       [--total-usd X] [--reduce PROMPT [--reduce-out FILE]] [--rm] [--retry-failed]
       [-n NAME] [--harness ID | --auto [--kind KIND] [--seed N]] [run's limits, permissions,
       launch and provisioning flags] [--json]
by map resume NAME [--retry-failed]
by map ls | show NAME | rm NAME       [--json]
```

For example:

```text
by map "Find the license and the star count of {{item.repo}}. Do not change any file." \
    --items repos.csv --schema answer.schema.json --out results.csv --concurrency 8 --yes
```

Each item's branch is named `<map>-<item id>` (with `-2`, `-3` for later attempts). The map's name is `-n NAME`, or a slug of the prompt, as `by run` names a branch. As each item ends, `by` says so on standard error and rewrites `--out`. At the end it prints the counts, the failed items with their errors, the reduce's answer, and what to do next.

`by map` exits 1 when an item failed, when the total budget left items not started, or when the reduce failed. With `--json` it prints the map's report (below) instead.

## Items

Items come from `--items FILE` (`-` for standard input), from `--from-command CMD`, or from standard input when neither is given and it is not a terminal.

| Format | Read as | Chosen by |
|---|---|---|
| `jsonl` | one JSON value per line; blank lines skipped | `.jsonl` or `.ndjson`, or text starting with `{` |
| `json` | one JSON array | `.json`, or text starting with `[` |
| `csv` | a header row, then one object of strings per row (RFC 4180 quoting) | `.csv` only; never guessed |
| `lines` | each non-blank line, as a string | `.txt`, or anything else |

`--input-format` overrides the choice.

Every item has an id. It is the item's `id` field (a string or a whole number) when there is one. Otherwise it is the first 12 hexadecimal digits of the BLAKE3 hash of the item, with object keys sorted. A rerun recognizes an item by its id, so give items an `id` when their content may change between runs. Two items with one id are refused.

`--from-command` runs `sh -c CMD` in the repository root, as you, with no standard input. A failing command (its status and the end of its standard error) stops the map before anything runs. It is refused inside a harness's branch (`BRANCHYARD_BRANCH` set), where nothing may be trusted to run; the same rule as workspace scripts.

## The prompt template

The prompt is a template with the placeholders a [trigger](triggers.md)'s prompt uses, `{{ ... }}`, spaces allowed:

| Placeholder | Value |
|---|---|
| `{{item}}` | the whole item: a string as is, anything else as JSON |
| `{{item.FIELD}}`, `{{item.a.b.0}}` | a value by path; a number indexes an array |
| `{{id}}` | the item's id |
| `{{index}}` | the item's position in the list, from 1 |
| `{{map}}` | the map's name |

Every item is rendered before anything runs. An unknown placeholder, or a path that leads nowhere for some item, stops the map with the item named. The renderer is the SDK's own (`render_map_prompt`); the trigger renderer lives in the server and knows only trigger fields.

## Answers and the schema

With `--schema FILE`, each branch must answer with JSON matching the schema. The item's prompt ends with an `## Answer` section that gives the schema and asks for the answer alone in a fenced ```` ```json ```` block at the end of the reply.

The answer is read from the branch's last reply, as a [judge's](fleet.md) verdict is. It is the whole reply when that is JSON, else the last fenced block in the reply. We chose the final message over a result file in the worktree because it works the same on every provider (local, Microsandbox, Substrate) and on a server, needs no file transfer, and does not put the answer into the branch's diff.

An answer that is missing, not JSON, or fails the schema gets **one follow-up turn on the same branch**. The follow-up starts with `[branchyard map: answer invalid]`, lists every error with where it is (`$.stars: expected integer, got string`), and repeats the task, so a harness that lost its context still has it. If the answer is still invalid, the attempt failed. A branch that did not end `ready` or `no changes` (it failed, was interrupted, or hit its limit) gets no follow-up: the attempt failed.

Without `--schema`, an item's result is its last reply's text, and only the branch's status decides success.

### Schemas

There is no JSON Schema crate in the workspace's offline vendor set, so Branchyard checks a documented subset with its own code (`branchyard::JsonSchema`):

- Checked: `type` (a name or a list), `properties`, `required`, `additionalProperties` (a boolean or a schema), `items` (one schema), `enum`, `const`, `minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum`, `minLength`, `maxLength`, `minItems`, `maxItems`, `uniqueItems`, `anyOf`. A schema may be `true` or `false`.
- Allowed and not checked: `$schema`, `$id`, `$comment`, `title`, `description`, `default`, `examples`, `format` (an annotation in JSON Schema 2020-12 too).
- Refused when the map starts: anything else, such as `pattern`, `$ref`, `oneOf`, `allOf`, `not`, `if`. Nothing a schema says is silently left unchecked.

Numbers compare by value (`1` equals `1.0`); `integer` accepts `3.0`. At most 20 errors are reported for one answer.

## Retries, concurrency and budgets

`--concurrency N` (1 to 64, default 4) is how many branches run at once. Workers take items in list order.

`--retries N` (0 to 10, default 1) is how many new branches an item gets after its first attempt fails, for any reason: a harness that would not start, a failed or interrupted branch, an answer still invalid after its follow-up. Each retry is a fresh branch with a fresh session. Failed attempts' branches are kept.

The flags `--budget-usd`, `--max-turns`, `--max-minutes` and `--stall-after` limit **each branch**, as for `by run`; the follow-up turn counts against the same branch's limits.

`--total-usd X` limits **the map**, across all its runs: before each attempt, if the costs recorded in its rows (and its reduce) reach `X`, no more items start, and each branch's cost limit is lowered to what is left. Items not started stay pending; the map says the budget was reached and exits 1. Running items are not stopped, so a map can overshoot by what they spend. As with every cost limit, a harness that reports no cost is not limited by it.

## Routing and failover

Without `--harness`, and with a `[fleet]` table, or with `--auto`, each attempt is routed through the [fleet](fleet.md) as `by run --auto` routes it: the entry for the task's kind (`--kind`, or the classifier on the item's prompt), the router's pick, and failover to the next candidate while a harness fails. The router's seed is varied per item and attempt, so a map spreads over the candidates as the router learns. An item's row lists the failover chain's branches; its `branch` is the one that answered. Routing is local only, as for `by run` and `by fan`.

## Results

`--out FILE` is written after every item ends and once at the end, through a temporary file and a rename, so a reader (or an interrupted map) never sees half a file. A `.csv` path gets CSV; anything else gets JSON lines.

| Column (CSV) / field (JSON lines) | Value |
|---|---|
| `id`, `status` | the item's id; `ok` or `failed` |
| the schema's top-level properties (CSV), or `result` | the answer's fields; a value that is not a string is written as JSON. Without an object schema, CSV has one `result` column |
| `error` | why the last attempt failed |
| `branch` | the branch that answered, or the last tried |
| `attempts` | branches started for the item (failovers not counted) |
| `cost_usd` | the item's branches' reported cost, when any reported one |

Rows are in the items' order. Items not finished have no row.

## Durability and resuming

A map is recorded in `.branchyard/maps/<name>/`:

- `map.json`: the spec, with its items and how it was started (`launch`).
- `results.jsonl`: one row per finished item, appended and synced as each ends. The last row for an id wins. A last line cut short by a crash is skipped.
- `reduce.json`: the reduce's outcome.
- `lock`: held by the process running the map, so two processes never run one map, and `by map ls` can say it is running.

Running a map again skips every item whose last row is `ok`, and every `failed` one unless `--retry-failed` is given. Items with no row, including one whose branch was interrupted, run again on a new branch (`<map>-<id>-2`); the interrupted branch is recovered as `interrupted`, as any branch is.

There are two ways to run it again:

- **The same command.** The items are read again, so a changed file is seen: new ids run, and items whose ids are gone are no longer shown. A map with that name but another prompt or schema is refused; forget it with `by map rm NAME` or choose another name.
- **`by map resume NAME`.** The recorded command line runs again from the directory it ran in, with the **recorded** items (standard input and `--from-command` are not read again), and the recorded `--out`, schema file and flags. `branchyard.toml` is read again.

`by map show NAME` (or `by show NAME`, when no branch has that name) prints the progress and every row; `by map ls` lists maps; `by ls` adds a `maps` section after the branches; `by watch` shows the running and unfinished maps' progress in its header. `by map rm NAME` forgets the record (refused while the map runs); the branches stay.

## The reduce

`--reduce PROMPT` runs one more branch, `<map>-reduce`, after the items. Its prompt is `PROMPT`, then a `## Results` section with every row as one compact JSON object per line (`id`, `status`, `result` or `error`), at most 100,000 characters; beyond that the prompt says how many rows were left out. Its last reply is the map's summary: printed, kept in `reduce.json`, shown by `by map show`, and written to `--reduce-out FILE` when given.

The reduce runs when the map has rows and the total budget did not stop it. A rerun whose results are unchanged reuses the last successful reduce instead of running another branch.

## Branches and cleanup

Every branch is kept by default, as `by fan` keeps its attempts: an item may have changed code you want to merge, and a kept branch shows what happened. With `--rm`, an item's branches are removed (as `by rm` removes them) once its answer is recorded `ok`, and the reduce's once it answered; the failed items' branches are always kept for inspection. A row still names its removed branch.

## On a server

With `--remote`, a map is one operation of kind `map`, admitted like a task and claimed by any worker serving the repository, which runs the map's branches on threads of its own process.

| Operation | SDK | by | by --remote | HTTP | client |
|---|---|---|---|---|---|
| Run a map | `Yard::map(MapSpec, &MapOptions)` → `MapReport` | `map PROMPT ...` | the same; items, schema, `--out` and `--reduce-out` are read and written on the client | `POST /v1/repos/{repo}/maps` (`MapRequest`) → `Operation` (`map`) | `Repo::submit_map` |
| Resume one | `Yard::map` with `Yard::map_spec`'s items | `map resume NAME` | the same, with the server's recorded request | `POST .../maps/{name}/resume` (`MapResumeRequest`) | `Repo::resume_map` |
| List maps | `Yard::maps` → `MapSummary` | `map ls`, and a section of `ls` | the same | `GET .../maps` (`MapList`) | `Repo::maps` |
| Show one | `Yard::map_report` | `map show NAME`, or `show NAME` | the same | `GET .../maps/{name}` (`MapReport`) | `Repo::map` |
| Forget one | `Yard::remove_map` | `map rm NAME` | the same | `DELETE .../maps/{name}` | `Repo::remove_map` |

The operation's result carries the map's report (`OperationResult::map`) and the branches that answered. `by` follows the operation's event stream, showing every branch named `<map>-...`. The operation locks the name `map:<name>`, so one map never runs twice at once on the server. A map started locally cannot be resumed through the server, and the other way round: each records how it was started in its own form.

What differs from `by run --remote`:

- The map's record is in `.branchyard/maps/` of the worker that ran it. Workers sharing a PostgreSQL store but not a repository directory do not see each other's map records, so a resume claimed by another worker starts the items again there.
- A map is one operation against `max_running` while it runs up to `--concurrency` branches, and its branches are not counted against `max_branches` at admission (none is known then). Size `--concurrency` for the worker.
- Routing (`--auto`, `--kind`, a `[fleet]`) is local only, as for `by run` and `by fan`.
- `by map resume --remote` has no `--out`; use `by --remote map show NAME --json`.

## What Manus and Genspark taught

Manus's *Wide Research* (from its public description; we have none of its code) runs one sub-agent per item of a list, each with a fresh context, in parallel, and collects their structured findings into a table, rather than one agent working through a long list as its context fills. Genspark's parallel agents fan one task over many agents and merge what they find. Branchyard takes the shape: an item per branch, so a fresh session and worktree each; a schema so every answer has the same columns; and a reduce turn to merge. What it adds is what a branch already has: durable records, retries on new branches, failover across harnesses, per-branch limits and a total budget, and resuming without rerunning what is done.

## What is untested

- No model has run a map. The fake ACP agent answers from files (`REPLY_FILE`, `REPLY_SEQUENCE`) and runs shell commands (`SH`). Whether real harnesses follow the `## Answer` instructions, and how often the follow-up fixes an answer, is unknown.
- The total budget is tested only with a cost written into a row by hand: the fake agent reports no cost.
- No map has run on a sandbox provider, on a PostgreSQL-backed server, or across several workers.
- `by watch`'s map line is not covered by a test that drives the dashboard.
- Large maps (thousands of items) have not been run; each row rewrite of `--out` reads the whole results file.

## Tests

`crates/branchyard-cli/tests/map.rs`, through the built `by` and the fake ACP agent: items from CSV, JSON lines, a JSON array on standard input and lines from `--from-command`, with every placeholder substituted; a rerun starting nothing; an invalid answer fixed by the follow-up (whose prompt is checked); an item failing after its retry; at most two of four branches at once with `--concurrency 2`, each pair overlapping; a map killed mid-item and run again, without rerunning the item done; `by map resume` with recorded items and `--retry-failed`; CSV results, the reduce with its prompt, `--reduce-out`, `--rm` and a reduce not rerun; the total budget; a routed map failing over from a harness that exits; refusals before anything runs; and the same map through `by serve` with `--remote`, resumed there. Unit tests cover the schema subset, the item formats and ids, CSV, the template, answer parsing, the result tables and the prompts.
