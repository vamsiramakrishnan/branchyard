# Repository knowledge

Short rules a person adopted for the agents working in a repository: "run `cargo fmt` before you finish", "the parser's tests live next to it", "never unwrap in library code". Branchyard proposes them from how branches went, a person reviews each one, and only adopted entries are given to harnesses, in the managed instructions block of every turn of every matching branch, most specific first and within a token budget. The `provisioned` event of each turn names the entries it was given, so you can trace what an agent was told. This is the *repository knowledge* track of the [roadmap](roadmap.md#wave-3), learned from Devin's Knowledge and Session Insights (suggested learnings, adopted on approval), Manus Projects (self-updating instructions proposed for approval) and Jules's repository memory.

> **Status.** Implemented in local mode and on the server, tested hermetically against the fake ACP agent. No model has distilled or followed an entry yet: see [what needs a model](#what-is-deterministic-and-what-needs-a-model).

## Entries

| Field | Meaning |
|---|---|
| `id` | Assigned by the store, from 1; shown as `k<id>` |
| `scope` | Where it applies: the whole repository, files matching a `path` glob, a `kind` of task, or a path and a kind |
| `text` | The rule, at most 600 characters |
| `source` | `person` (who added it) or `branch` (the branch, the turn whose correction it came from, and `via`: `send`, `steer`, `review`, `pull_request`, or `harness <id>` for a harness distiller) |
| `status` | `proposed`, `adopted` or `rejected` |
| `created_ms`, `decided_ms` | When it was proposed, and when a person last adopted, rejected or edited it |
| `adopted_by` | Who adopted it, while it is adopted |
| `note` | Why a distiller proposed it, or why a person rejected it |

Entries live in the repository's store (the `knowledge` table in SQLite, `by_knowledge` in PostgreSQL, scoped by repository like every other table) behind `KnowledgeBackend`, with the same conformance test on both: ids grow, an update is a compare-and-swap on the status (two people deciding at once cannot both win), concurrent adds from two engines get distinct ids, and entries outlive the branches they came from. On PostgreSQL the table is made the way the server's own tables are: the catalog is read first and an opener that finds the table does nothing that could lock it; one that does not makes it alone, under the schema's advisory lock, after checking again.

## Learning, only with approval

When a top-level branch ends (it merges, a judge proposes it as the pick, or, if you ask for it, a turn of it ends ready), it is **distilled**: entries are proposed from it, and nothing more. A proposed entry is never given to a harness.

Two distillers exist:

- **The extractor**, deterministic, the default. It reads the branch's event log for what a person told it after its task: every prompt sent with `by send` (or `Branch::send`) after the first, every `by send --steer`, each comment of a `by review` (scoped to the comment's file), and each review comment `by pr --watch` delivered (scoped to its file; failed CI checks are not corrections). Branchyard's own prompts (plan mode, plan approval, goal follow-ups), inbox blocks, a rewind's summary, and replies that only keep a turn going (`continue`, `yes`, `lgtm`, …) are skipped. Each correction is proposed as written, with the repository as its scope unless it came with a file.
- **A harness distiller**: any `Judge` (normally a `HarnessJudge`, the [judge's](fleet.md#judging) machinery) run read-only on a scratch branch that is removed afterwards. Its prompt has the task, the extractor's corrections, what changed, the agent's last reply and the entries already adopted, and asks for at most five general rules. It must answer one JSON object and nothing else (one fenced block is allowed):

  ```json
  {"entries": [{"text": "Run cargo fmt before you finish.", "path": null, "kind": "bugfix", "why": "the person asked for it twice"}]}
  ```

  Parsed strictly (`branchyard::parse_distilled`): exactly `entries`, at most five, each with exactly `text` (1 to 600 characters), `path` (a glob or null), `kind` (a task kind or null) and `why`. Anything else, or a distiller branch that fails, falls back to the extractor's proposals, and the `distilled` event says why.

A proposal whose text (case and spacing aside) and scope match an entry that already exists, in any status, is not proposed again: a rejection sticks. Each distillation records `Activity::Knowledge(KnowledgeActivity::Distilled)` on the branch: the ids proposed, how many were already known, by whom, the fallback reason, and the trigger (`merged`, `judged_best`, `ready` or `asked`). `by log` shows it as `distilled (merged, deterministic): proposed knowledge #3, #4`.

## Review

```text
by knowledge list [--status proposed|adopted|rejected | --all] [--json]
by knowledge show ID [--json]
by knowledge review [--editor EDITOR] [--json]
by knowledge adopt ID... | reject ID... [--reason TEXT]
by knowledge edit ID [--text TEXT] [--path GLOB] [--kind KIND] [--editor EDITOR]
by knowledge add "TEXT" [--path GLOB] [--kind KIND] [--propose]
by knowledge rm ID
by knowledge distill BRANCH [--harness ID [--command CMD] | --deterministic]
by knowledge export [--out FILE]
```

`review` walks the proposed entries one at a time and reads one answer a line: `a` adopts, `r` rejects (and asks for a reason), `e` opens the text in your editor and adopts what you save, `s` skips, `q` stops; `--json` prints the decisions made. `edit` without `--text`, `--path` or `--kind` opens the text in the editor; `--path ""` and `--kind ""` widen the scope back to the repository. `add` adopts what you write at once (you are its author), unless `--propose`. Who adopts or rejects is `$USER` locally and the token's name on a server. `distill` proposes now, with the configured distiller, another harness (`--harness`), or only the extractor; on a server, only the extractor. `export` writes the adopted entries as an `AGENTS.md`-style file, grouped by scope (everywhere first), each ending with its id.

`by knowledge` refuses to run inside a harness: knowledge is the people's decision.

## Use

Before every turn, the adopted entries that match the branch are added to its instructions, after the task's own `--instructions` and before the connectors' line, on the same path as every other instruction ([provisioning](provisioning.md#design)): the driver's session channel where there is one, else the managed block between `<!-- BEGIN BRANCHYARD MANAGED -->` markers in the harness's home.

- **Matching.** A repository entry always matches. A `kind` entry matches the branch's kind: its route's (`--kind`, or the router's), else the classifier's ([fleet](fleet.md#task-kinds)). A `path` entry matches when a file the task is known to touch matches the glob: a file its candidate changed, or an existing repository path its prompt names. With no file known yet, path entries wait for one.
- **Order.** Most specific first: a path and a kind, then a path, then a kind, then the repository; older first within each.
- **Budget.** At most about `budget_tokens` (four characters a token, header included; default 1500). An entry that does not fit is left out and the next is tried; a warning names those left out.
- **Tracing.** The turn's `provisioned` event lists the ids given (`by log`: `provisioned: knowledge #3, #1; …`, `by log --json`: `"knowledge": [3, 1]`).

The block reads:

```text
## Repository knowledge
Rules people adopted for this repository, most specific first. Follow them unless the task says otherwise.
- [k3] (path crates/parser/**) Keep the parser's tests next to it.
- [k1] Run cargo fmt before you finish.
```

A judge's or distiller's scratch branch is given none.

## Configuration

```toml
[knowledge]
provision = true                        # give adopted entries to harnesses (default true)
budget_tokens = 1500                    # per turn
distill_on = ["merged", "judged_best"]  # also "ready"; [] for only `by knowledge distill`
distiller = { harness = "claude-code", model = "small" }   # optional; also effort, command
```

Checked strictly like the rest of `branchyard.toml` (an unknown trigger or harness, a zero budget, a bad effort are errors naming the key); in `schema/branchyard.config.json`. A server's repositories use the defaults. In the SDK, `Yard::use_knowledge(KnowledgeSettings { … })`.

## What is deterministic, and what needs a model

| Deterministic (tested hermetically) | Needs a real harness and model to evaluate |
|---|---|
| The store on SQLite and PostgreSQL; ids, compare-and-swap decisions, entries outliving branches | Whether agents follow adopted entries, and whether the order and the budget are right |
| The extractor over sends, steers, `by review` and `by pr --watch` prompts; duplicates and rejections never re-proposed | Whether a person's corrections, as written, make good rules (the extractor does not generalize) |
| Matching, ordering, the budget, the `provisioned` trace; `by knowledge` locally and remotely | Whether a model distiller proposes general, correct rules, and answers the format reliably |
| A harness distiller's protocol: scratch branch, deny-all policy, strict parsing, fallback | |

## Surfaces

| Operation | SDK | by | by --remote | HTTP | client |
|---|---|---|---|---|---|
| List, show | `Yard::knowledge`, `knowledge_entry` | `knowledge list`, `show` | yes | `GET /v1/repos/{repo}/knowledge[?status=]`, `…/knowledge/{id}` | `knowledge`, `knowledge_entry` |
| Add | `Yard::add_knowledge` (`NewKnowledge`) | `knowledge add` | yes, as the token's name | `POST …/knowledge` (`KnowledgeAddRequest`) | `add_knowledge` |
| Adopt, reject | `adopt_knowledge`, `reject_knowledge` | `knowledge adopt`, `reject`, `review` | yes | `POST …/knowledge/{id}/adopt`, `…/reject` | `adopt_knowledge`, `reject_knowledge` |
| Edit, remove | `edit_knowledge` (`KnowledgeEdit`), `remove_knowledge` | `knowledge edit`, `rm` | yes | `POST …/knowledge/{id}/edit`, `DELETE …/knowledge/{id}` | `edit_knowledge`, `remove_knowledge` |
| Distill | `Yard::distill` (any `Judge`) | `knowledge distill` | the extractor only | `POST …/branches/{branch}/distill` | `distill` |
| Export | `export_knowledge` | `knowledge export` | yes | `GET …/knowledge/export` | `export_knowledge` |
| Settings | `Yard::use_knowledge` (`KnowledgeSettings`) | `[knowledge]` | the server's defaults | n/a | n/a |

Reading needs the `read` scope; everything else `run`. Delegation has no knowledge operations: a harness cannot adopt what it is told.

## Code and tests

| Where | What |
|---|---|
| [`knowledge.rs`](../crates/branchyard/src/knowledge.rs) | Entries, scopes, `KnowledgeBackend`, people's operations, matching and the budget, the extractor, the distiller's prompt and strict parsing, distillation and its triggers; unit tests for each |
| [`sqlite.rs`](../crates/branchyard/src/sqlite.rs), [`pg.rs`](../crates/branchyard/src/pg.rs), [`conformance.rs`](../crates/branchyard/src/conformance.rs) | The tables, and the `knowledge` conformance check on both |
| [`provisioning.rs`](../crates/branchyard/src/provisioning.rs), [`engine.rs`](../crates/branchyard/src/engine.rs) | The block in the instructions, the ids in `provisioned` |
| [`tests/knowledge.rs`](../crates/branchyard/tests/knowledge.rs) | Most specific first and the trace, the budget, provisioning off; a merged branch's corrections and review comments proposed, not used until adopted, a rejection sticking, edits and removal; checks; distilling on ready; a harness distiller's answer and its fallback |
| [`knowledge_cmd.rs`](../crates/branchyard-cli/src/knowledge_cmd.rs), [`tests/plans_and_knowledge.rs`](../crates/branchyard-cli/tests/plans_and_knowledge.rs), [`tests/remote.rs`](../crates/branchyard-cli/tests/remote.rs) | `by knowledge` end to end with `review` on stdin, `[knowledge]`, and against a server |
| [`knowledge_routes.rs`](../crates/branchyard-server/src/knowledge_routes.rs), [`knowledge_api.rs`](../crates/branchyard-client/src/knowledge_api.rs), [`tests/knowledge.rs`](../crates/branchyard-server/tests/knowledge.rs) | The routes, the client, and their test over HTTP |
