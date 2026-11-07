# Delegate reference: Choosing a harness

Read when: you pick a harness, a model tier or a split-or-solo for a subtask: by fleet advise, --why, risk words.

## Choosing a harness

Pick with data, not by name. Before each spawn, ask:

    by fleet advise "<the subtask, as you will give it to by spawn>" --json

It answers without running anything: the subtask's `kind`; the
`candidates` (your task's `[fleet]` entry for that kind, or the harnesses
your envelope allows), each with `available` (and `reason` when not), its
recorded outcomes for this kind (`stats.runs`, `stats.mean_cost_usd`) and
`expected`, the P(OK) the router samples around (0.50 with no outcomes);
the router's own `pick`; your `remaining_usd`; and `notes`. Python:
`branchyard.advise(prompt)`; MCP: `advise`.

Read it like this:

- Prefer the cheapest candidate whose P(OK) for this kind clears what the
  subtask can afford to get wrong: about 0.7 for mechanical work with a
  check, higher for design or debugging you would have to redo.
- With no recorded outcomes every P(OK) is 0.50: take the cheapest that can
  do the work, give it a check, and let the outcome teach the store.
- `notes` names risk words (production, delete, migration, security,
  payment, ...): never give such a subtask to the cheapest tier.
- Escalate on evidence: a child that failed its check or ran out of budget
  is retried on the next tier up, not the same one.
- The router's `pick` is one opinion from the same data; you know the
  subtask and the repository, it does not.

Then say why, so the choice is recorded beside how the child turns out:

    by spawn "Rename Compat::Versioned to Compat::PathVersioned everywhere" \
      --harness codex --model small --check "cargo test" --budget-usd 0.20 \
      --why "mechanical rename, cargo test checks it; codex small is 6/6 on refactors here"

`by log <child>` shows the choice (`chosen by ... : codex model=small: ...`),
and `by fleet stats` counts children's outcomes beside routed runs (its
PARENT column), so the next advice knows.

Split or solo? Do it yourself when the work is small or tightly coupled.
Split when the parts are separable with little shared state (you
supervise, children work), when one change repeats across many targets
(fan out, then integrate together), when correctness matters more than
speed (one child does, another reviews), or when parts have an order (a
graph with `--depends-on`). A split costs a session per child and a merge;
it pays when the children run in parallel or on a cheaper tier.
