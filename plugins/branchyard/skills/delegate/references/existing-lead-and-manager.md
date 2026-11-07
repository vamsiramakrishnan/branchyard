# Delegate reference: An existing lead, the workspace manager and run/fan deliverables

Read when: you are a native lead that is not a Branchyard branch, or you use the workspace manager or by run and by fan.

Two parts of the opening, then the section "An existing lead starts coworkers".

- **Existing native lead:** when `BRANCHYARD_BRANCH` is unset, use operator
  commands such as `by run`, `by fan`, and `by handoff` from the yard. They
  create or coordinate coworkers without adopting or restarting your session.
  Use explicit waits or status commands. Branchyard does not wake this lead.
  For the remote workspace console API, configure the separate operator MCP
  with `by mcp manager --url URL --token-file FILE`. Add `--ca-file FILE` for
  private trust roots. Its scoped HTTP credential stays outside managed turns.

The workspace manager creates code or knowledge projects, starts and continues
native sessions, inspects sources and immutable outputs, accepts deliverables,
and observes queued operations. Its discovery lists the available tools.
For repository-free knowledge work, an operator starts `by serve --app --knowledge`.
This opt-in retains configured storage and quotas; it does not change your grant.
Reuse one `idempotency_key` when retrying one intended mutation. The ordinary server
scopes and workspace membership still apply. Never pass the operator token to a
managed agent. `BRANCHYARD_MANAGER_TOKEN` is reserved and removed from harness
environments, including inherited environments and explicit keep or set rules.
Manager responses have a 32 KiB JSON budget and previews default to 25 rows.
Oversized responses return an error with small identifiers and cursors. Narrow
the preview, download the original through its authorized endpoint, or delegate
a focused calculation. A mutation can already have succeeded; inspect its
operation and reuse its original idempotency key when reconciling.
For `workspace_action`, pass the `revision` from `inspect_workspace`. A
`revision_conflict` requires fresh inspection and an explicit new proposal with
a new key; never refresh and resubmit automatically.
The operator manager's `start_session` accepts explicit `unapproved_tools: true`
for a profile such as AGY that cannot route tool approvals. The server operator
must also permit it. Omission means false; do not infer this acknowledgment.
When the server answers `preset_required`, pass the operator's execution preset
ID as `preset`; harness and model must then be ones that preset offers.

## An existing lead starts coworkers

```sh
by run "Assess the supplied notes; return final JSON with summary and evidence." \
  --harness antigravity --allow-unapproved-tools --name research \
  --result-schema '{"type":"object","required":["summary","evidence"],"properties":{"summary":{"type":"string"},"evidence":{"type":"array","items":{"type":"string"}}}}' \
  --result-required --result-deliverable --json
by compare research --json
```

The `antigravity` harness profile runs the `agy` CLI. AGY keeps its native approvals. `--allow-unapproved-tools` acknowledges that
it does not route individual tool approvals through Branchyard. With AGY-only
inventory, use deterministic judging or review the evidence in your native
lead. A required model judge needs a harness that enforces the read-only judge
policy. A worker's `--allow-unapproved-tools` never bypasses this judge
requirement. Schema acceptance proves structure, not truth.

`by fan` accepts the same result flags for independent attempts. Knowledge
work outside a repository can use `--no-files`; read the returned task with
`by task show ID --json`. `by fan --judge` supports initial selection. Later
`by judge` and `by compare` need the owning yard context. Obtain an attempt's
private worktree with `by task open ID --attempt BRANCH --print` and run the
commands from that directory. Handoffs within the same private yard work there.
