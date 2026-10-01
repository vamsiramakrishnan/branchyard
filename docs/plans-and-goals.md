# Plan approval and goals

Two ways to keep an agent on course before and after it works. **Plan approval**: the first turn only reads and proposes a plan, and the branch waits until a person (or, for a delegated child, its parent) approves it, edits it or rejects it; nothing changes before that. **Goals a judge verifies**: a branch given a goal is not done when a turn ends `ready`, but when its check passes, its diff is not empty, and, if a judge is configured, the judge finds evidence that the goal is met; until then it gets follow-up turns with what is missing. Both are the *plan approval and goals* track of the [roadmap](roadmap.md#wave-3), learned from Kiro's specs, Factory's Spec Mode and Manus's Plan Mode (a read-only plan, approved or edited before execution), and from Codex's and OpenHands's `/goal` (a judge must find evidence of completion).

> **Status.** Implemented in local mode, on the server and through delegation; tested hermetically against the fake ACP agent, with plans and verdicts it answers from files. No model has planned or judged a goal yet: see [what needs a model](#what-is-deterministic-and-what-needs-a-model).

## Plans

```text
by run "migrate the config loader" --plan        # or by fan --plan, or [fleet.<kind>] plan = true
by plan show BRANCH [--json]
by plan approve BRANCH [--edit [--editor EDITOR] | --file FILE] [--yes ...] [--json]
by plan reject BRANCH [--reason TEXT] [--replan] [--yes ...] [--json]
```

With `--plan` (`TaskOptions::plan`, `"plan": true` in a task request, `plan: true` in a [spawn](delegation.md)), a new branch's first turn:

1. **Plans read-only.** Its prompt is a planning prompt around the task (it starts `[branchyard plan mode]`) asking for the approach, the files to change, the risks and how to check the result, in Markdown, ending if it can with one fenced JSON task list, `{"tasks": [{"title": "...", "detail": "..."}]}`. It runs under **`branchyard::read_only_policy()`, whatever policy the caller passed**: reading and searching tools (`Read`, `Grep`, `Glob`, `LS`, `read_file`, `list_directory`, …, `READ_ONLY_TOOLS`) are allowed and every other request (writes, edits, commands) is denied through the permission policy, with a delegating parent's denials still first. A profile whose tool requests never reach the policy (Antigravity, Pi, Amp) cannot be held read-only, so `--plan` refuses it before anything is created.
2. **Waits.** A planning turn that completes leaves the branch **`awaiting_plan_approval`**, a new status, with its reply as the plan (`Plan { markdown, tasks, tasks_error, turn, round }`; a task list that does not parse is left out and says why). A planning turn that changed files anyway keeps them in the candidate, with a warning. A plain `by send` is refused while the plan waits; `by watch` marks the branch `?`.

`by plan show` prints the plan, its task list and its phase (`planning`, `awaiting`, `approved`, `rejected`); `by show` adds a `plan` line and `by show --json` a `plan` object, from the events, so they work remotely too.

- **`by plan approve`** sends the plan as the branch's next turn (its prompt starts `[branchyard plan approved]`), under your options: the same `--yes`, `--ask`, limits and provisioning flags as `by send`, so it runs with normal permissions. `--edit` opens the plan in your editor (`$VISUAL`, `$EDITOR` or `--editor`) and approves what you save; `--file` approves a file's text. A goal, if the branch has one, is pursued after that turn.
- **`by plan reject`** ends the branch `failed` (`its plan was rejected by ana: …`), or with **`--replan`** runs another read-only planning turn whose prompt carries your reason; the branch then awaits approval of round 2.

Each step is an `Activity::Plan` event (`PlanActivity`: `planning`, `proposed`, `approved` with who and whether it was edited, `rejected` with the reason and whether it re-plans, `escalated`), shown by `by log` (`plan round 1 approved with edits by ana`). Who approves is `$USER` locally, the token's name on a server, the parent branch through delegation.

### Delegated children

A child spawned with `plan: true` (`by spawn --plan`, the MCP `spawn` tool's `plan`, `Spawn::plan`) plans read-only like any branch, and when its plan awaits approval it **escalates** it to its parent's inbox: an `escalation` message with the plan and how to approve it, delivered into the parent's running turn by steering when it can, else at its next turn ([inbox](delegation.md#inbox)). The parent decides with `by plan approve|reject` in its shell, `branchyard.approve_plan`/`reject_plan` in Python, `Delegate::approve_plan`/`reject_plan`, or the MCP `approve_plan`/`reject_plan` tools; only an ancestor may, and the approved turn runs on the parent's engine like any child's. A person can decide it too, with `by plan` outside the harness.

## Goals

```text
by run "speed up the parser" --goal "parsing the 1 MB fixture takes under 50 ms" \
       [--goal-rounds N] [--goal-judge ID [--goal-judge-command CMD]] --check "cargo bench --bench parse"
```

With a goal (`TaskOptions::goal`, `Goal { text, rounds, judge, custom }`; `"goal": {"text", "rounds", "judge"}` in a task request), whenever a turn of the branch ends `ready`:

1. **Deterministic checks first.** The branch's check runs on its exact candidate, as `by compare --check` runs it, and its diff must change something. Either failing is a verdict *not met*, with what failed as the missing list, and no judge is asked.
2. **Then the judge, when there is one.** `--goal-judge` (or `[fleet.<kind>] goal_judge` for the task's kind, or an SDK caller's own `Judge` in `Goal::custom`) runs read-only on a scratch branch, removed afterwards, as [`by judge`'s harness](fleet.md#judging) does. Its prompt has the goal, the task, the deterministic results, the rubric, a summary of the transcript (each prompt's start and each turn's last reply) and the diff (the first 16,000 characters). It must answer one JSON object and nothing else (one fenced block allowed):

   ```json
   {"met": false, "evidence": ["parse.rs no longer allocates per token"], "missing": ["the benchmark still reports 71 ms"]}
   ```

   Parsed strictly (`branchyard::parse_goal_verdict`): exactly `met`, `evidence` and `missing`; no empty items, at most 20 each; met with evidence and nothing missing, or not met with something missing. Anything else, or a judge branch that fails, **falls back to the deterministic result** (met, since the checks passed), and the verdict says why.
3. **Without a judge**, the deterministic result decides.

Not met, the branch gets a **follow-up turn** whose prompt starts `[branchyard goal not met]` and lists what is missing, then is checked again; at most `--goal-rounds` follow-ups (default 2), and within the branch's budget (a follow-up over `--max-turns` or `--budget-usd` ends the branch `budget_exceeded`, as any turn would). Met, the branch stays `ready` with the evidence recorded. Not met with no follow-up left, the branch ends **`failed`** (`its goal was not met after 2 follow-up turn(s); missing: …`), its candidate kept for a person to look at or merge, and `by run` exits 1.

Each verdict is an `Activity::Goal` event (`GoalActivity`: `set`, `verdict` with the round, the turn, met, evidence, missing, the deterministic checks and who decided, `exhausted`), shown by `by log` (`goal not met (round 0, harness claude-code); missing: …`) and summarized by `by show` (`goal  … — met: … (1 of 2 follow-up turns, by harness claude-code)`) and `by show --json` (`goal`). A plan and a goal combine: the goal is checked after the approved plan's turn, not while the plan waits.

## Configuration

```toml
[fleet.migration]
candidates = [{ harness = "claude-code" }]
plan = true                                          # every migration plans first
goal_judge = { harness = "codex", effort = "high", rubric = "Require a passing migration test." }
```

`plan` and `goal_judge` (`harness`, `model`, `effort`, `command`, `rubric`) apply to a new branch of that kind, routed or not (`by run --harness X` still looks up the entry for the task's kind); `goal_judge` only judges a `--goal` that names no judge. Both are checked strictly and are in `schema/branchyard.config.json`. A trigger's branches never plan: no one waits to approve them.

## What is deterministic, and what needs a model

| Deterministic (tested hermetically) | Needs a real harness and model to evaluate |
|---|---|
| The read-only policy: a write attempted while planning is denied even with `--yes`; the plan from the reply, the task list's strict parsing | Whether harnesses plan well when every write is denied, and which read-only tools each needs allowed (the allow-list is from their documentation) |
| `awaiting_plan_approval`, approve (with edits), reject, re-plan; refusals; escalation to a parent and its approval through delegation; the server's operations | Whether models follow an approved, edited plan |
| Goals: the checks first, strict verdicts, fallback, follow-ups with the missing list, rounds and budget, the events | Whether a model judge's verdicts agree with people, and whether its evidence is real |

## Surfaces

| Operation | SDK | by | by --remote | HTTP | client | delegation |
|---|---|---|---|---|---|---|
| Plan first | `TaskOptions::plan`, `TaskBuilder::plan` | `run --plan`, `fan --plan`, `[fleet.<kind>] plan` | yes | `plan` in the task | yes | `spawn` with `plan` |
| Show a plan | `Yard::plan` (`PlanInfo`), `plan_from_events` | `plan show`, `show` | yes | `GET …/branches/{b}/plan` | `plan` | `events` |
| Approve | `Yard::approve_plan` | `plan approve [--edit\|--file]` | yes, editing here | `POST …/branches/{b}/plan/approve` (`PlanApproveRequest`), an `approve_plan` operation | `approve_plan` | `approve_plan`, descendants only |
| Reject, re-plan | `Yard::reject_plan` | `plan reject [--replan]` | yes | `POST …/branches/{b}/plan/reject` (`PlanRejectRequest`), a `reject_plan` operation | `reject_plan` | `reject_plan`, descendants only |
| In `by watch` | n/a | `a` approves, `e` re-plans with a reason, `X` rejects | yes | n/a | n/a | n/a |
| Goal | `TaskOptions::goal` (`Goal`), `TaskBuilder::goal`, `Yard::goal` (`GoalInfo`), `goal_from_events` | `run --goal`, `fan --goal`, `--goal-rounds`, `--goal-judge`, `[fleet.<kind>] goal_judge` | yes; the judge is one of the server's harnesses | `goal` in the task (`GoalRequest`) | yes | no: a child gets no goal |

A plan decision on a server is admitted as an operation that locks the branch, refused at once (`409 no_plan`) when no plan awaits approval; the approval's `send` fields (`budget`, `policy`, `check`, …) are a send's. Routed branches that fail over to another harness keep neither the plan nor the goal; that is not done yet.

## Code and tests

| Where | What |
|---|---|
| [`plan.rs`](../crates/branchyard/src/plan.rs) | Phases, the read-only policy, prompts, the task list, settling a planning turn, escalation, approval and rejection; unit tests |
| [`goal.rs`](../crates/branchyard/src/goal.rs) | Goals, strict verdicts, the judge's prompt, the pursuit loop; unit tests |
| [`engine.rs`](../crates/branchyard/src/engine.rs), [`run.rs`](../crates/branchyard/src/run.rs), [`graph.rs`](../crates/branchyard/src/graph.rs), [`delegation.rs`](../crates/branchyard/src/delegation.rs) | The policy and status of a planning turn, new branches and children planning, `approve_plan`/`reject_plan` for delegates |
| [`tests/plans_and_goals.rs`](../crates/branchyard/tests/plans_and_goals.rs) | A write denied while planning; approval with edits; re-plan and rejection; a profile without approvals refused; a child's plan escalated and approved by its parent, refused to an outsider; goals met at once, unmet then met, rounds exhausted, stopped by the budget, a failing check without asking the judge, a harness verdict used and an invalid one falling back; a plan then a goal |
| [`plan_cmd.rs`](../crates/branchyard-cli/src/plan_cmd.rs), [`tests/plans_and_knowledge.rs`](../crates/branchyard-cli/tests/plans_and_knowledge.rs), [`tests/remote.rs`](../crates/branchyard-cli/tests/remote.rs) | `by plan`, `--plan`, `--goal`; end to end with an editor script and a judge answering a sequence of verdicts; against a server |
| [`knowledge_routes.rs`](../crates/branchyard-server/src/knowledge_routes.rs), [`work.rs`](../crates/branchyard-server/src/work.rs), [`tests/knowledge.rs`](../crates/branchyard-server/tests/knowledge.rs) | The plan routes and operations, goals in task requests, over HTTP |
