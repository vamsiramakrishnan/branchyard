---
name: delegate
description: Coordinate coworkers from an existing native lead or a managed Branchyard branch. Keep each harness's own planning and subagents. Choose owned code changes or structured deliverables, review evidence, relay frozen results, and use checked integration for code.
---

# Delegating with Branchyard

Keep your native plan, task graph, tools, and subagents. Branchyard coordinates
work between harness sessions. You do not need to import your internal task
graph or replace your current lead.

- **Existing native lead:** when `BRANCHYARD_BRANCH` is unset, use operator
  commands such as `by run`, `by fan`, and `by handoff` from the yard. They
  create or coordinate coworkers without adopting or restarting your session.
  Branchyard does not wake this lead: use explicit waits or status commands.
- **Managed lead:** when `BRANCHYARD_BRANCH` is set, `by` acts as that branch.
  It can create Branchyard children only within its envelope and budget, and
  acts on its own descendants. These limits count Branchyard branches, not
  the harness's native subagents. Native permissions and limits still apply.

Decide whether each coworker produces checked code or a structured answer.
A researcher with a deliverable contract can return native final JSON without
Git changes or a Branchyard reporting tool. A code worker runs its configured
check before its candidate is integrated.

Continue while your own native tasks or tools are active. A managed lead may
end its turn to await Branchyard children after its native work finishes.
Automatic landing is an opt-in coding workflow, not a requirement for research.

## When to delegate

- The task splits into parts that touch different files or modules and can
  proceed in parallel.
- A part suits another harness better, and your envelope allows it.
- The work matters enough that someone else should review it before it
  lands: a reviewer on another vendor's harness catches what its author
  cannot.
- You want two approaches to the same problem and will keep one.

Do small or tightly coupled work yourself. Each child costs a harness
session, and work split along lines that cross the same files conflicts.

## The loop for a managed code lead

1. **Split by ownership.** Give each part the paths it owns (`owns`): siblings
   that own disjoint paths cannot conflict, and a change outside a child's
   paths is refused when it lands. Files every part needs (a lockfile) belong
   to no child: regenerate them yourself afterwards.
2. **Settle the interfaces in a brief.** The goal, the conventions and the
   contracts between the parts, which every child is given each turn:
   `by brief "The lexer returns Spanned<Token>."` (children running now are
   sent it at once).
3. **Write each prompt so a stranger could finish it.** The goal in one
   sentence; the files it owns and may only read; the constraints; the exact
   check command and what passing means; what to report. A child sees your
   committed work, the brief and its prompt, not this conversation.
4. **Delegate the plan, left to land.** One call creates every child, all or
   none, and the engine integrates each when it is ready, its check on the
   exact merge included:

   ```sh
   by delegate --plan "$TMPDIR/plan.json" --land
   ```

   The plan is JSON: `{"brief": "...", "review": {"harness": "codex",
   "budget_usd": 0.50}, "tasks": [{"name": "lexer", "prompt": "...", "owns":
   ["src/lexer"]}, {"name": "parser", "prompt": "...", "owns": ["src/parser"],
   "depends_on": ["lexer"], "after": "integrated"}]}`. A single child works
   the same way: `by spawn "..." --owns src/x --land --review codex`.
5. **End your turn.** Do not wait, poll or integrate. When everything has
   settled, Branchyard starts your next turn with a summary of what landed,
   what was left for you and why, and each child's status, cost and last message.
6. **Finish.** Read what landed, run `by check` on the whole, fix what is left
   (`by send <child> "<fix>"`, or do it yourself; `by integrate <child>` once a
   left-over child is fixed), and end your turn.

To drive each child yourself instead: `by spawn "<prompt>" --name N --owns
PATHS --budget-usd 0.50`, then `by wait`, then `by integrate`. Pick the harness
with data first (`by fleet advise "<subtask>" --json`).

## Rules that always apply

- **Never `git merge` a child.** `by integrate a b c` merges them in order,
  runs the check once on the result and moves your branch once, all or none.
  `by check <child>` runs a child's check without integrating.
- **Do not leave a poll running.** End a managed turn when native work is done
  and Branchyard wakes you. Never run `by wait` or an orchestration script as a
  backgrounded shell command. In the foreground, keep `by wait --timeout S`
  under any limit of your own.
- **Run `by check` before you end a managed turn** when you have a check; fix
  what it names and run it again.
- **Budget is yours to spend carefully.** `by inspect` shows your
  `remaining_usd`; `by spawn --budget-usd N` caps a child, and a live child
  holds that whole amount of yours. `by spawn --model small` suits only
  mechanical work with a check.
- **Scratch files go under `$TMPDIR`**, private to your branch, never `/tmp`.
  Branchyard commits your edits at the end of each turn as `by/<branch>: turn <n>`.
- **A result is a claim.** A schema proves shape, not truth; review evidence.
- **Never pass the operator token to a managed agent.** Reuse one
  `idempotency_key` when retrying one intended mutation.
- **Inside a harness**, `by spawn` refuses `--parent`, `--yes`, `--ask` and
  `--permissions`: children run under your policy. `by spawn --help` marks
  every flag you may not pass. Every command takes `--json`; call `by` as a
  single command, without pipes, `&&` or substitutions.
- **Before you call a delegated task done**, `by obligations <your branch>`
  lists what your subtree still owes. An empty list means nothing is owed.

## Reference files

The details are in the files in the `references` directory beside this file. Without that
directory, `tools search WORDS` finds each in your tools tree (`helpers/delegate/references/`).
Read only the row you need.

| Read | When |
|---|---|
| `references/delegate-and-land.md` | the engine creates, reviews and lands the children: plan fields, `--land`, `review`, `"harness": "auto"` |
| `references/writing-prompts.md` | a worked good and bad prompt |
| `references/spawn-wait-steer.md` | spawn by hand, pick a model tier, wait, steer, retry a cut-off turn |
| `references/integrate-and-cleanup.md` | integrate, a failed check, a conflict between siblings, cancel, discard, rm |
| `references/statuses-plans-budget.md` | statuses, `--plan` approval, tool approvals, obligations, budget arithmetic |
| `references/choosing-a-harness.md` | `by fleet advise`, `--why`, solo or split |
| `references/commands-and-finishing.md` | the by cheatsheet, commits and scratch files, what to run before you finish |
| `references/graph-issues-rigs.md` | `--depends-on`, `by graph apply`, `--issue`, seats in a rig |
| `references/result-contracts.md` | `--result-schema`, `--result-deliverable`, file artifacts, compare and judge |
| `references/messaging-and-artifacts.md` | artifacts, `by ask`, `by report`, `by report-status`, inbox, `by message` |
| `references/handoffs.md` | `by handoff` of a frozen result to another coworker |
| `references/python-and-mcp.md` | the `branchyard` Python module, the MCP tools when you have no shell |
| `references/existing-lead-and-manager.md` | you are not a managed branch, or use the workspace manager, `by run` or `by fan` with a deliverable |
| `references/operator-setup.md` | an operator sets up Substrate, a compute worker or a native sandbox provider |
