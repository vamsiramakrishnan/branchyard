# Fleet: routing, judging and failover

A **fleet table** in `branchyard.toml` says which harnesses, models and efforts run each kind of task, how many attempts to start, what each may spend and who judges them. `by run` without `--harness` (or with `--auto`) asks a **router** to pick from it, learning from how earlier branches turned out. `by fan --auto` starts several attempts, and `by judge` (or `by fan --judge`) scores them and proposes one to merge. When a harness itself fails (it exits, never completes its handshake, is rate-limited or cannot log in), the task **fails over** to the next candidate. This is the *judge and router* track of the [roadmap](roadmap.md).

> **Status.** Implemented in local mode and tested hermetically against the fake ACP agent, with a judge harness answering a canned verdict. No model has judged anything and the router has learned from no real outcomes yet: see [what needs a model](#what-is-deterministic-and-what-needs-a-model). `by --remote` refuses routing and judging with a message.

## Task kinds

Every routed branch has a kind: `bugfix`, `feature`, `refactor`, `review`, `research`, `docs`, `migration`, `tests` or `other`. `--kind KIND` gives it; otherwise a classifier infers it from the prompt, without a model:

- The prompt is split into lowercase words of letters and digits.
- A word that **starts with** one of a kind's stems scores a point for that kind; the prompt's **first word** (usually the imperative verb) scores three.
- The highest score wins. Ties go to the kind listed first below. No match is `other`.

| Kind | Stems (in tie order) |
|---|---|
| review | review, audit, critique, proofread |
| migration | migrat, upgrad, bump, deprecat, port |
| refactor | refactor, restructur, reorganiz, rename, extract, simplif, cleanup, clean, dedup, tidy, split |
| tests | test, coverage, fixture, assert |
| docs | doc, readme, comment, changelog, tutorial, guide, explain |
| bugfix | fix, bug, crash, broke, regress, fail, panic, error, wrong, incorrect, flaky, leak, hang |
| research | research, investigat, explor, evaluat, survey, analy, why, compare, benchmark, find |
| feature | add, implement, support, feature, create, build, introduc, new, enable, allow |

"Fix the flaky parser test" is a bugfix (fix 3, flaky 1, against test 1); "Write tests for the parser" is tests; "Update the README" is docs. The table is `branchyard::fleet::KEYWORDS`; `branchyard::classify` returns the kind and the words that decided it, and `by run` prints them. The kind is recorded on the branch (the `routed` event below) and in the outcome store. A run that names its harness can still record a kind: `by run --harness codex --kind docs` (`Yard::run_with_kind`).

## The fleet table

```toml
[fleet.default]
candidates = [
  { harness = "claude-code", model = "large", effort = "high" },
  { harness = "codex", effort = "medium" },
  { harness = "gemini-cli" },
]
attempts = 2            # by fan --auto starts two; by run starts one
budget_usd = 3          # per attempt; a failover chain shares it
max_turns = 20
max_minutes = 30
failover = true         # by run --auto always fails over
exploration = 0.1       # the chance of a random pick (default 0.1)
judge = { harness = "claude-code", model = "large", rubric = "Prefer the smallest correct change." }

[fleet.docs]
candidates = [{ harness = "codex", effort = "low" }]
environment = "docs-site"   # recorded, not acted on yet
connectors = ["github"]     # recorded, not acted on yet
```

| Key | Meaning |
|---|---|
| `[fleet.<kind>]`, `[fleet.default]` | The entry for a kind; `default` for every kind without one. Other keys are refused |
| `candidates` | At least one; each a `harness` (harness or profile ID, as `by harnesses` lists them) with an optional `model`, `effort` (`low`, `medium`, `high`, `xhigh` or 0-100) and `command` (an executable replacing the profile's, as `--command`; for development and tests). Listed twice is refused |
| `attempts` | 1 to 16; how many branches `by fan --auto` starts (default 1) |
| `budget_usd`, `max_turns`, `max_minutes` | Each attempt's limits, filling only what the command line leaves unset |
| `failover` | Fail over when a harness fails; implied by `--auto` |
| `exploration` | 0 to 1 |
| `judge` | A judge harness: `harness`, optional `model`, `effort`, `command`, `rubric` |
| `environment`, `connectors` | Opaque strings passed through to the `routed` event for the prepared-environments and connectors tracks; Branchyard does not act on them yet |
| `plan` | Plan first: a new branch of this kind, routed or not, starts with a read-only planning turn and waits for approval ([plans and goals](plans-and-goals.md)) |
| `goal_judge` | The judge harness of a `--goal` given to a branch of this kind when the command names none: `harness`, optional `model`, `effort`, `command`, `rubric` ([plans and goals](plans-and-goals.md#goals)) |

The table is checked as strictly as `[workspace]`: unknown keys, an unknown harness, a bad effort, an empty candidate list, `attempts = 0`, an exploration outside 0 to 1 are errors naming the key (`by config validate` reports them). It may be in the project file or the user file; a project's `[fleet.<kind>]` replaces the user file's for the same kind. It is in `schema/branchyard.config.json`. A harness running on a branch (`BRANCHYARD_BRANCH` set) reads no configuration, so its `by` never routes.

## Routing

A new top-level branch is routed when `by run` or `by fan` is given `--auto`, or when the configuration has a `[fleet]` and the command names no harness (`[defaults] harness` then does not apply). `--auto` with `--harness` is refused. Before anything starts, `by` prints the decision:

```text
by: routed as bugfix (classifier: fix, flaky) by [fleet.default], seed 1834...
by:   codex effort=medium — sampled success rate 0.81 (6 recorded bugfix outcomes, 4.5 successes)
by:   not gemini-cli: gemini-cli is unavailable: gemini was not found on PATH
```

The router (`branchyard::fleet::plan`) works per kind and candidate (harness, model, effort):

1. **Eligible** candidates are those whose profile exists and is allowed (`--allow-unapproved-tools` for profiles without tool approvals), whose executable is found (the check `by harnesses` makes, with the candidate's or the task's `command`; skipped in a sandbox), and whose **mean recorded cost** for the kind is not over the entry's `budget_usd`, and whose login has not used more than `[usage] skip_over` of its 5-hour or weekly window ([usage](usage.md#the-guard-and-the-router); also `near_percent` with `guard = "refuse"`). None eligible is an error listing each reason, and nothing is created.
2. With probability `exploration`, one eligible candidate is picked uniformly at random.
3. Otherwise it is **Thompson sampling**: each candidate's success rate is drawn from Beta(1 + successes, 1 + failures) of its recorded outcomes (see [credit](#outcomes)), and the highest draw wins; an equal draw goes to the earlier candidate. A candidate with no history draws from Beta(1, 1).
4. A fan of N attempts picks without replacement; more attempts than eligible candidates start another round, so a candidate can run twice (best of N on one harness).

The generator is SplitMix64 with Marsaglia-Tsang gamma sampling, so a **seed** fixes the route: `--seed N` on `run` and `fan`, `RouteOptions::seed` in the SDK. Without one, `by` draws a seed and prints it. `by fleet route "prompt" [--kind K] [--attempts N] [--seed N] [--json]` shows what the router would pick without running anything.

Each routed branch records `Activity::Fleet(FleetActivity::Routed(RouteDecision))` before its first turn: the kind and where it came from, the entry, the candidate, the attempt (`2 of 3`), the reason, whether it fails over, the remaining candidates, the chain's budget and spend, the branch it failed over from, and the passed-through `environment` and `connectors`. `by log` shows it as `routed bugfix (classifier) by [fleet.default] to codex effort=medium: sampled success rate 0.81 (...)`, and `by log --json` as a `fleet` activity.

A routed fan names its branches `<name>-<harness>` as `by fan` does, and a harness picked again `<name>-<harness>-2`; `by compare --fan`, `by judge` and `by watch`'s `c` key find them all.

**A session stays on its model.** A candidate's model and effort are stored with the branch. A send that names no model keeps the branch's (before, a send's provisioning replaced the branch's whole, dropping its model), and a send naming a different model is refused once a turn has run: switching models is not a continuation. Reincarnate or fork with a fresh session to change it.

## Outcomes

When a turn of a top-level branch ends, when it merges, and when it is judged, its row in the **outcome store** is written or replaced: `repo`, `branch`, `kind`, `harness`, `model`, `effort`, `outcome`, `score`, `cost_usd`, `duration_ms`, `turns`, whether it was routed, and when. A judge's scratch branch and delegated children are not recorded. Rows outlive their branches: they are what the router learns from.

| Outcome | When | Credit |
|---|---|---|
| `merged` | the branch merged | 1 |
| `judged_best` | a judge proposed it (kept until it merges or a later turn fails) | 1 |
| `ready` | its turn completed with a candidate | its judge score / 100, or 0.5 unjudged |
| `failed` | failed, stopped at a limit, or changed nothing | 0 |
| `interrupted` | interrupted or cancelled | not counted |

A candidate's successes are the sum of its credits and its failures the sum of one minus each. The store is the `outcomes` table in SQLite and `by_outcomes` in PostgreSQL (scoped by the repository like every other table), behind `OutcomeBackend`, with the same conformance test on both. A row is committed as an event append is, without a synchronous commit of its own: it is statistics derived from the branch, and the next turn or merge rewrites it.

`by fleet stats [--kind K] [--json]` prints them per kind and candidate: runs, merged, judged best, ready, failed, interrupted, the posterior mean success `P(OK)` = (1 + successes) / (2 + successes + failures), and mean cost, time, turns and score. `Yard::outcomes` and `branchyard::fleet_stats` give the same in the SDK.

## Judging

```text
by judge <fan-name | branch...> [--harness ID [--command CMD] | --deterministic] [--rubric TEXT]
         [--pick [--into TARGET] [--discard-others] [--yes]] [--json]
by fan "..." --auto --judge
Yard::judge(branches, &JudgeOptions { run_checks, judge, rubric, record })
```

One argument naming a `by fan` is that fan's branches; otherwise the branches named. For each attempt:

1. **Its check runs on its exact candidate**, in a private worktree, as `by compare --check` does (the same code, with the merge's 30-minute timeout).
2. **A deterministic score**, 0 to 100, from what `by compare` gathers: the check (passed 60; none or not run 40), the diff's size in lines (up to 20, smaller better), cost (up to 10, cheaper better; 5 when unknown) and time (up to 10, faster better; 5 when unknown), each relative to the other eligible attempts. An attempt that cannot be picked scores 0: no candidate, a last turn that did not end `ready`, or a check that failed, timed out or could not run.
3. **Optionally a judge harness**: `--harness ID`, else the `judge` of the `[fleet]` entry for the attempts' kind (from the first attempt's `routed` event, or the classifier), unless `--deterministic`. It runs on a **scratch branch** from `HEAD`, with every tool request denied, and is removed afterwards along with its worktree; it is never recorded as an outcome. Its prompt has the task, a default rubric (does the task and breaks nothing, a passing check counts; changes only what is needed; clear; small) plus the entry's or `--rubric`'s, and each attempt's status, check, diff stats, turns, cost, time and diff (the first 16,000 characters). It must answer **one JSON object and nothing else** (one fenced block is allowed):

   ```json
   {"ranking": ["best", "next"], "scores": {"best": 90, "next": 40}, "reasons": {"best": "…", "next": "…"}}
   ```

   Parsed strictly (`branchyard::parse_verdict`): exactly these three keys; the ranking names every attempt once; scores and reasons have every attempt as keys; scores are 0 to 100. Anything else, or a judge branch that fails, **falls back** to the deterministic score, and the output says why.

The ranking is the judge's when its verdict was used, else by deterministic score (ties: fewer turns, then name). The **proposed pick** is the best-ranked attempt that can be picked: a judge ranking a failed check first does not get it picked, and its reason says so. `by judge` prints a table (rank, branch, harness, score, check, status, +/-, cost, time, why), who judged, and the pick; `--json` prints the `Judgement`. Each attempt gets a `judged` event (score, rank, picked, by whom, reason, check), and its outcome row its score (and `judged_best` for the pick).

`--pick` merges the pick through the same code as `by compare --pick` (its check on the exact merge result, compare-and-swap of the target), and `--discard-others` removes the rest after a confirmation or with `--yes`. `by fan --judge` judges the fan's attempts when they finish and prints the table and the pick, without merging.

The judge is pluggable: `Judge` is a trait (`name`, `verdict(yard, prompt, candidates) -> text`); `HarnessJudge` (or `branchyard::harness_judge(spec, options)`) is the harness implementation, and SDK callers may pass their own.

## Failover

A routed branch fails over when its turn ended `failed` **because of its harness**, and its route asked for it (`--auto`, or `failover = true`). `branchyard::harness_fault` decides from the failure:

| Fails over | Never fails over |
|---|---|
| The harness could not start or was unavailable (`could not start`, `not found on PATH`) | A check that failed (checks run at merge or judging, not in a turn) |
| It exited, or the connection closed (`the harness exited`, `the turn's outcome is unknown`, a broken pipe writing to it) | A limit (`budget_exceeded`), a cancel or interrupt (`interrupted`) |
| It did not complete its handshake (`open failed`, `timed out`) | The model refusing, provisioning or a driver refusing the task's configuration, workspace setup, a lost lease, a denial |
| Rate limits and overload (`429`, `rate limit`, `overloaded`, `quota`) and authentication (`401`, `403`, `invalid api key`, `not logged in`), as the driver reports them | Anything else |

Failing over starts a new branch on the next remaining candidate that is available, through reincarnation: from the failed branch's candidate with a handoff brief, or, when it has none, from its base with its original prompt (or a brief, when a turn ran). The new branch is named `<branch>-<harness>`, records a `routed` event with `from`, and the failed branch records `failing over to …: the harness exited: …` and is `superseded_by` the new one. A chain is bounded: each candidate is tried at most once, and the chain stops once its cost reaches the entry's `budget_usd` (the next branch gets what is left). Every branch of a chain stays in its entry and runs under the limits the first attempt did, the task's filled from the entry's (`max_turns`, `max_minutes`); only the cost limit is shared. A chain moves only to candidates the router found eligible: one it excluded for its recorded cost or its login's usage never joins the chain, even if it is available by then. When nothing is left the failed branch says why (`not failing over: no candidate left (…)`). `by run --auto` and `by fan --auto` fail over until a branch succeeds or the chain ends, and print each move; `by send` on a routed branch fails over the same way after its turn (`Yard::failover`), under the send's own limits.

A judge's pick (and a merge) also proposes [repository knowledge](knowledge.md) from the branch's corrections, for a person to adopt; nothing is used before that. A goal's judge ([plans and goals](plans-and-goals.md#goals)) is the same machinery with another verdict: `{met, evidence, missing}`.

## What is deterministic, and what needs a model

| Deterministic (tested hermetically) | Needs a real harness and model to evaluate |
|---|---|
| The classifier, the router under a seed, exclusion of unavailable and over-budget candidates, the exploration floor's rate, Beta sampling's mean | Whether the stems classify real prompts well enough |
| Outcome recording and its credit; the store on SQLite and PostgreSQL | Whether Thompson sampling over these credits converges on better candidates in practice, and a good exploration floor |
| Checks on exact candidates, the deterministic score and ranking, strict verdict parsing and the fallback | Whether a model judge's verdicts agree with people, and whether the default rubric is right |
| Failover classification of the reasons above, the chain's bounds and naming, the handoff | The exact text real harnesses print when rate-limited or logged out (the patterns are from their documentation, not observed) |
| The judge harness protocol: scratch branch, deny-all policy, removal | A model answering the verdict format reliably |

## Surfaces

| Operation | SDK | by | by --remote |
|---|---|---|---|
| Route without running | `Yard::route` | `fleet route` | no: refused, local only for now |
| Routed run, fan | `Yard::run_routed`, `fan_routed` (`Routed`) | `run --auto`, `fan --auto`, or a `[fleet]` without `--harness` | no: refused |
| Record a kind | `Yard::run_with_kind` | `run --kind` | no: refused |
| Fail over after a turn | `Yard::failover` | after `send` on a routed branch | no |
| Judge | `Yard::judge`, `Judge`, `HarnessJudge`, `parse_verdict`, `deterministic_scores` | `judge`, `fan --judge` | no: refused |
| Outcomes | `Yard::outcomes`, `fleet_stats` | `fleet stats` | no: refused |

## Not done yet

- **Remote.** The server has no routing, judging or outcome endpoints; `by --remote` refuses them. The outcome table is in the PostgreSQL store already.
- **No real outcomes.** The router starts from uniform priors; with a few outcomes per candidate it explores by design.
- **Costs** come from harnesses that report them; a candidate without cost reports is never excluded for cost.
- **Failover of delegated children** and of rig seats: only branches started by `run`, `fan` and `send` with a route fail over.
- **`environment` and `connectors`** are recorded only, for the tracks that will act on them.

## Code and tests

| Where | What |
|---|---|
| [`fleet.rs`](../crates/branchyard/src/fleet.rs) | Kinds and the classifier, the table, routing events, outcomes and `OutcomeBackend`, the router, routed runs and fans, failover; unit tests: classification, determinism under a seed, learning, the exploration floor, exclusion, rounds, Beta's mean, failure classification, credit |
| [`judge.rs`](../crates/branchyard/src/judge.rs) | The deterministic score, the prompt, strict verdicts, `Judge` and `HarnessJudge`, ranking and recording; unit tests for each |
| [`run.rs`](../crates/branchyard/src/run.rs) | `run_attempts` (a run or fan with per-attempt harness, command, provisioning, budget and events), `reincarnate_with` (from the base when there is no candidate), a send keeping its model |
| [`sqlite.rs`](../crates/branchyard/src/sqlite.rs), [`pg.rs`](../crates/branchyard/src/pg.rs), [`conformance.rs`](../crates/branchyard/src/conformance.rs) | The `outcomes` tables and their conformance test on both |
| [`tests/fleet.rs`](../crates/branchyard/tests/fleet.rs) | A routed run recording its route and outcome, merged; failover from a harness that exits, keeping the entry's limits and never to a candidate the router excluded; no failover for a driver refusing the configuration, with no candidate left, or with failover off; unavailable candidates never routed to; a routed fan's names; a send keeping its model; the deterministic judge; a judge harness on a read-only scratch branch, its strict verdict and fallback; a pluggable judge |
| [`fleet_cmd.rs`](../crates/branchyard-cli/src/fleet_cmd.rs), [`tests/fleet.rs`](../crates/branchyard-cli/tests/fleet.rs) | `by run` routed by a `[fleet]`, `--kind`, `--auto` failing over, `by fan --auto --judge` with a judge harness, `by judge --pick --discard-others`, `by fleet stats|route`, refusals |
| [`config.rs`](../crates/branchyard-setup/src/config.rs) | `[fleet]`'s format and checks |
