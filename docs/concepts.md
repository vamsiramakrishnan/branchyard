# Concepts

The words Branchyard uses, in the order a newcomer meets them. Each one is a thing you can see in `by ls`, `by show` or `branchyard.toml`. Start with [getting started](getting-started.md) if you have not run `by` yet.

## What a piece of work is

| Word | Meaning | See it |
|---|---|---|
| **Task** | What you asked for, and every attempt at it. Every `by run` starts one. | `by task ls`, `by task show ID` |
| **Branch** (an **attempt**) | One try at a task: a git branch `by/NAME`, its own worktree under `.branchyard/worktrees/NAME`, its own harness session and event log. "Branch" is the git side of an attempt, "attempt" the task side. | `by ls`, `by show NAME` |
| **Base** | The revision the branch started from (`--base`, default `HEAD`). | `by show NAME` |
| **Turn** | One prompt given to the harness, and the harness's work until it stops. `by run` makes turn 1; `by send` adds one. | `by log NAME` |
| **Checkpoint** | The snapshot commit every turn ends with. `by rewind NAME --to N` goes back to one; `by fork NAME --at N` starts a branch from one. | `by show NAME` |
| **Candidate** | The change a branch offers: its latest checkpoint against the base. It is what `by diff` shows and `by merge` merges. A branch has none until a turn changes a file. | `by diff NAME` |
| **Check** | A command (`--check "cargo test"`) that must pass on the exact merge result before `by merge` moves your branch. | `by merge NAME` |

## Ways to start work

| Command | Who decides | What starts | Use it when |
|---|---|---|---|
| `by run` | You | One branch on one harness | You know what you want done |
| `by fan` | You | The same prompt on several harnesses, one branch each (`NAME-HARNESS`), then a comparison table; `--judge` adds a verdict | You want to see which harness does it best, then merge one (`by compare --pick`) |
| `by map` | You | One prompt over every item of a list, one branch per item, answers checked against a JSON schema and collected into a table | The same question has many inputs ([map](map.md)) |
| `by spawn`, `by delegate` | The agent in a running branch | Child branches of that branch, each with its own harness, prompt and budget | The agent should split the work itself; its children's work comes back with `by integrate`, and it is woken when they settle ([delegation](delegation.md)) |

`by fork` starts one more branch from an existing branch's candidate and conversation.

## Harness, profile, provider

- **Harness**: a coding agent you run in a terminal, such as `claude-code`, `codex` or `gemini-cli`. `--harness ID` chooses one; `by harnesses` lists what is installed and logged in.
- **Profile**: how Branchyard talks to a harness, one driver per protocol (`claude-code-stream-json`, `claude-code-acp`, `codex-app-server`). A harness may have two. `--harness` takes either name; `by harnesses --profiles` lists them.
- **Provider**: where the harness process runs: `local` (the default), `microsandbox`, `substrate` or `recipe:NAME` (`--provider`). It is not the model provider (Anthropic, OpenAI and the rest), which the harness or the `[models]` gateway deals with.

## Modes

How much stands between a harness and your machine, in steps. `docs/modes.md` gives the one setting and the one `by doctor` row for each.

- **local**: the harness runs as your user in a git worktree, with no isolation beyond that.
- **isolated**: `--isolated` gives it a scrubbed environment and a private `HOME`.
- **sandbox**: `--provider microsandbox`, `substrate` or `recipe:NAME` runs it in a microVM, an actor or a machine.
- **server**: `by serve` runs the engine behind an authenticated HTTP API and clients use `--remote`.
- **postgres**: `by serve --database URL` keeps the server's state in PostgreSQL so several servers and workers share it (a build with the `postgres` feature).

## Branch states

`by ls` prints the words in the second column; `by show NAME --json` has them as `{"state": "…"}`. A running turn that has had no harness activity for its stall window also shows `(stalled)`. After every run, send and fork, the `next` lines list the commands that fit the state. A branch in a bad state keeps its worktree and session until `by rm`; [troubleshooting](troubleshooting.md) has the fixes.

| State | Shows as | Meaning | Do next |
|---|---|---|---|
| `running` | running | A turn is in progress. | `by watch`; `by log NAME -f`; `by cancel NAME` |
| `waiting` | waiting | A delegated child whose prerequisites have not all settled. It has no worktree and has run no turn. | `by graph show PARENT` |
| `blocked` | blocked: REASON | A child that will not start because a prerequisite failed, was interrupted, hit a limit, is blocked itself or was removed. | Fix or drop the prerequisite; `by graph show PARENT`; `by rm NAME` |
| `ready` | ready | The last turn completed and produced a candidate. | `by diff NAME`, then `by merge NAME` |
| `awaiting_plan_approval` | awaiting plan approval | A planning turn proposed a plan, and nothing changes until it is approved, edited or rejected. | `by plan show NAME`; `by plan approve NAME`; `by plan reject NAME --reason "…"` |
| `waiting_on_children` | waiting on children | A delegating branch whose turn ended while its children still ran. It is parked and starts its next turn by itself when they settle. Not finished. | `by children NAME`; `by wait --all CHILD…` |
| `no_changes` | no changes | The last turn completed without changing any file. | `by send NAME "…"` to ask again; `by rm NAME` |
| `interrupted` | interrupted | The turn stopped before it finished: you cancelled it, or the engine running it stopped. The session is kept. | `by log NAME`; `by send NAME "…"` to continue; `by rm NAME` |
| `budget_exceeded` | over budget: LIMIT | A limit you set (`--budget-usd`, `--max-turns`, `--max-minutes`) stopped it. | `by diff NAME` for what it has; `by send NAME "…" --max-turns N` with a higher limit |
| `failed` | failed: REASON | The turn ended in failure: the harness errored or died, or a plan was rejected. | `by log NAME`; `by send NAME "…"` to try again; `by rm NAME` |
| `merged` | merged into TARGET | The candidate was integrated. The worktree stays until you remove it. | `by rm NAME` |
| `discarded` | discarded: REASON | Set aside on purpose with `by discard`: never integrated, record and cost kept. | `by rm NAME` |

## What "qualified" means

A harness can be driven only through a profile. Two facts about a profile are kept apart: whether a driver exists, and whether anyone ran it against the real program. The words below are used the same way in `by harnesses`, `by doctor`, [compatibility](compatibility.md) and [driver qualification](qualification/README.md).

| Word | Meaning |
|---|---|
| **live-qualified** | The profile's driver ran the nine-scenario battery against the real harness binary, and the report records how many passed, the date and the harness version: "9 of 9 pass, 2026-10-16, codex-cli 0.157.1". `by doctor` shows `qualified 9/9 on 2.1.283`. |
| **unqualified** | A driver exists and passes its protocol tests against mocks (the compatibility table says "not qualified"), but no live run is recorded. It may work; nobody has shown it. |
| **drivable** | `by run --harness ID` can start it: it has a profile. `by harnesses --all` lists the others as "knows about". |
| **inform-only** | Hooks tell a session in that harness about your branches, but there is no driver: Droid and Kiro today. See [harness hooks](harness-hooks.md). |

Qualification is protocol behaviour with the harness started as a local process. It does not cover sandbox isolation, network policy, credential handling or node recovery; those stay unqualified for every profile.

Not done: `drivable` and `inform-only` are this page's names for what the catalog and the hook packaging show; the qualification pages use neither word. The rows for `waiting`, `blocked`, `awaiting_plan_approval` and `waiting_on_children` come from the enum's comments and the delegation, graph and plan pages, not from a live run; the others were run against the fake agent.
