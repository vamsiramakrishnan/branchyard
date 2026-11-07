# Delegate reference: Statuses, plan approval, approvals, obligations and budget

Read when: you read a child status, approve a plan or a tool call, list what is owed, or reckon a child's budget.

The end of "A managed lead coordinates code by hand".

Statuses: `running`, `ready` (a candidate to merge), `no_changes`,
`interrupted`, `budget_exceeded`, `failed`, `merged`, `discarded`,
`waiting` and `blocked` (prerequisites), `awaiting_plan_approval` (a child
spawned with `--plan` wrote its plan and waits for you), `waiting_on_children`
(its turn ended while its children run; it is woken when they settle).

A child spawned with `by spawn --plan` plans read-only first and escalates
its plan to your inbox. Read it with `by plan show <child>`. `by plan approve
<child>` runs it as the child's next turn (`--edit` or `--file PATH` changes
the plan first); `by plan reject <child> --reason "<why>"` ends the child, or
with `--replan` has it plan again. When a descendant's tool or connector call
needs approval, the ask comes to you too: `by approvals ls` lists what waits,
`by approvals allow <id>` or `by approvals deny <id> --reason "<why>"` answers
it. Only an ancestor may decide either.

Before you call a delegated task done, `by obligations <your branch>` lists
what your subtree still owes: calls whose answer was lost, staged calls,
approvals, lost turns, ready children you have not integrated or discarded,
each with the command that deals with it. An empty list means nothing is owed.

Inside a harness, `by spawn` refuses `--parent`, `--yes`, `--ask` and
`--permissions`: your children run under your policy. `by spawn --help`
marks every flag you may not pass.

Budget: `--budget-usd` sets a child's cost limit, which refusals and statuses
call `max_usd`. A child that is running, waiting, blocked, awaiting plan
approval or waiting on its own children holds its whole budget of yours. Once it settles (any other status,
`discarded` included) it holds only what it spent, and the rest is yours again;
a removed child's spend still counts. `by inspect` shows what live children
hold (`reserved_usd`, by `reserving_children`) and what settled ones spent (`settled_children_usd`).
Sending a settled child more work holds its budget again, narrowed to what you
have left. Your envelope's `max_children` counts only live children.
