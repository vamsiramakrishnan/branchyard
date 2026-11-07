# Delegate reference: With Python and without a shell

Read when: you drive Branchyard from Python (branchyard.spawn, wait_all, delegate) or from the MCP tools, with no shell.

## With Python

The `branchyard` module is on `PYTHONPATH`; it runs `by --json` for you.

```python
import branchyard

me = branchyard.inspect()
# remaining_usd is None on a branch with no cost limit: then give no budget.
share = None if me.remaining_usd is None else me.remaining_usd / 2
parts = [
    branchyard.spawn("Port the tokenizer to the new API; run its tests",
                     name="tokenizer", budget_usd=share),
    branchyard.spawn("Port the formatter to the new API; run its tests",
                     name="formatter", budget_usd=share),
]
done = branchyard.wait_all(*(child.name for child in parts), timeout=1800)
ready = [i.name for i in done.settled if i.status.state == "ready"]
try:
    branchyard.integrate(*ready)        # together, checked once
except branchyard.BranchyardError as error:
    print(error.kind, error.message)
```

A managed lead whose native work is finished can hand the engine the split
and end its turn; its next turn says
what landed:

```python
branchyard.delegate(
    [
        {"name": "tokenizer", "prompt": "Port the tokenizer; run its tests", "owns": ["src/tokenizer"]},
        {"name": "formatter", "prompt": "Port the formatter; run its tests", "owns": ["src/formatter"]},
    ],
    brief="The new API is in src/api.rs; keep the public names.",
    land=True,
    review={"harness": "codex", "budget_usd": 0.50},
)
```

`branchyard.wait_any(...)` returns when the first settles. Errors are
`DeniedError` (envelope, budget or authority), `RunningError` (also a wait
that timed out), `NotFoundError`, or `BranchyardError` with a `kind`.

## Without a shell

If you cannot run commands, the same operations are MCP tools on the
`branchyard` server: `delegate` (`tasks`, `brief`, `land`, `review`), `brief`,
`spawn` (with `depends_on`, `owns`, `land`, `review` and `result`), `inspect`, `events`, `send`,
`steer`, `propose_integration` (`branch`, or `branches` to integrate several
together), `check` (configured checks for your coding work or a child's), `wait` (`branches`, `any`, `timeout_seconds`), `cancel`, `discard`,
`children`, `graph`, `apply_graph`, `approve_plan` and `reject_plan` (a
plan-first child's plan), `answer_approval`, the artifact and scratch tools, and
`ask`, `report` (with `result`), `escalate`, `report_status`, `answer`,
`inbox` and `message` (`to`, `text`, optional `kind`, `payload`, `key`: a
sibling reads it at its next turn). `handoff` (`from`, `to`, optional
`artifacts`, `note`, and `expected`) relays a settled result without
starting a recipient turn.
