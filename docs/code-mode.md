# Code mode: from a CLI to an SDK a harness programs

> **Status.** Design, October 2026. It is based on the real-harness battery below. Nothing on this page is built yet, except where it says so.

## What the battery measured

We ran Claude Code as a meta-harness delegating to Claude Code children. That covered 12 repository scenarios and 3 knowledge-work campaigns, the campaigns run against a Worldloom company served over MCP and graded on the company's facts.

| Mode | Cases | Mean score | Calls per case | Cost per case | Wall time |
|---|---|---|---|---|---|
| One session, MCP tool calls | 2 | 0.85 | 8.5 | $0.47 | 110 s |
| Delegated, MCP tool calls | 4 | 0.79 | 10.5 | $0.62 | 140 s |
| Delegated, code mode (Python SDK) | 4 | 0.75 | 26.5 | $0.57 | 225 s |
| Worldloom reference agent | 40 | 1.00 | 2.95 | — | — |

Code mode did not win on its own. It cost about the same, but it made about 3× the calls: the programs crawled whole result sets, and the grader scores calls outside the plan as noise. What code mode does give is control flow (loops, branches, retries) without spending a model turn per step, and intermediate data that stays out of context. To be worth it, the SDK has to make the *right* program the easy one to write.

Delegation itself showed the same pattern. Most of a meta-harness's tokens went on:
- polling with `by inspect`;
- recovering from refusals;
- working around missing verbs (`wait`, multi-integrate, discard).

Each workaround cost a model round trip.

## Principles

1. **One operation registry; generated surfaces.** Every delegation, storage, messaging and connector operation is declared once: name, arguments, result schema, effect class, and the contexts it is allowed in. The CLI, the Python and TypeScript SDKs, the MCP tools, the skill's command table and the docs tables are generated from that declaration, or checked against it. Today the CLI and the Python module are maintained by hand and have drifted: Python has `wait` and the CLI does not.
2. **Code runs where the ledger is.** A program that calls Branchyard and Anvil only through the SDK cannot act outside the turn's token: every call goes through the delegation broker or the effect-ledger proxy, which decide and record it. So a sandboxed code-execution tool whose only side-effect channels are those SDKs can be pre-approved as a whole, while each call inside it is still approved, staged or blocked individually. This is the safe version of "let the model write a script".
3. **Plan, then execute.** Anvil's composite `Flow` already has `plan`, `validate`, `dry_run`, `run` and `compensate`. Code mode should default to building a `Flow` (a declared DAG of business steps) and running it. Ad-hoc loops should be the exception. The plan is reviewable, Worldloom can grade it, and the effect contract applies per step.
4. **Events, not polling.** Waiting is a primitive with no token cost: `wait_any`/`wait_all` on the broker, and park-and-wake for a turn that ends while its children run. A harness should never spend tokens asking "are you done?".
5. **Bulk and server-side shaping.** Most of code mode's extra calls were reads it did not need. The SDK should offer query-shaped reads (filters, projections, `limit`, `fields`) and batch writes with per-item results, so one call does what a loop of ten did, and the trace stays attributable to plan nodes.

## The surfaces

| Today (CLI, a subprocess per call) | Code mode |
|---|---|
| `by spawn …` × N | `ws = await by.spawn_many([...])`: atomic against the envelope, returns handles |
| `by inspect` in a loop | `done = await by.wait_any(ws)` / `wait_all`, event-driven over the broker |
| `by integrate a; by integrate b` (fails per child) | `await by.integrate([a, b, c])`: one merge, one check, one compare-and-swap |
| `by send`, `by send --steer` | `await child.send(...)`, `child.steer(...)`, with typed delivery state |
| Anvil per-connector CLI and SDK | `anvil_compose.Flow`: steps across connectors, `plan()` → `dry_run()` → `run()`; ledger IDs per step |
| Reading JSON into context | Results stay in the program; the turn receives a summary the program prints |

The Python and TypeScript SDKs talk to the broker socket directly, not through `by --json` subprocesses. The CLI becomes a thin client of the same registry.

## Execution sandbox

The code-mode tool runs a program in a sandbox, for example `execute(language, source, budget)` exposed as one MCP tool or a harness skill:
- the branch's worktree, mounted read-write;
- a private temporary directory;
- egress only to the broker and the effect-ledger proxy;
- CPU, memory and time limits;
- the turn's token, mounted as a file.

Its output is captured, truncated and returned. The effect ledger records every effect the program caused, under the turn and the program's run ID. Approvals that need a person suspend the program and resume it when answered (ask), or stage the call (stage). A program cannot outspend the branch's budget, because spawns and connector calls reserve from the envelope as they do today.

## Efficiency avenues, ranked by expected effect

1. **Park-and-wake** for delegating turns. This removes polling turns and the "no changes" failure. It is being built now.
2. **Atomic multi-integrate.** This removes the sibling-merge workarounds and the git-merge bypass. It is being built now.
3. **Leaf children on cheaper models.** `spawn(model=...)`, or seat defaults, puts Haiku-class models on mechanical subtasks and keeps the meta on a stronger model. The children's prompts share a cached prefix: the skill, repository context and task template.
4. **Flow-first code mode** over Anvil composites, with server-side query shaping and batch writes. Measure it against tool calls on the same Worldloom suite, and keep the version that wins on score per dollar.
5. **Warm worktrees and dependency caches** for children: pools already exist, and the build cache can be shared across branches.
6. **Structured child results.** `by report --json` (or the final message as JSON against a schema the parent gives at spawn) instead of free text that the parent re-reads and summarizes.
7. **Pre-approved delegation commands** (`--allow-delegation`) so that delegation verbs do not each cost a permission round trip.
8. **Artifacts instead of context.** Children exchange data by artifact ID, not by pasting it into prompts.

## How we will know

The battery becomes a qualification suite: the repository scenarios plus the Worldloom campaigns, each run with real harnesses, recording:
- score (tests or Worldloom grade);
- cost;
- wall time;
- model round trips;
- calls outside the plan;
- friction items.

A change to delegation, SDKs or code mode ships with its before and after numbers on that suite.
