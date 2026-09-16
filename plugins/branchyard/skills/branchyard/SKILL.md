---
name: branchyard
description: Prepare, validate, submit, inspect, and reconcile remote Branchyard tasks and dynamic child graphs using the Branchyard CLI. Use when a harness needs to delegate work through Branchyard or recover an uncertain Branchyard operation. Requires an installed CLI and a compatible server for remote operations.
---

# Branchyard

Branchyard supplies control operations. You decide whether to delegate and what
to do next. Do not invent a fixed committee or start local workers to simulate a
missing server.

## Discover the available boundary

1. Run `branchyard --version`. If it is missing, report that prerequisite.
   The adjacent `scripts/branchyard.py` is a portable launcher; it also accepts
   `BRANCHYARD_BIN` as a single executable path, never a shell command.
2. Run `branchyard doctor` when remote work is required. It reads
   `BRANCHYARD_ENDPOINT` and `BRANCHYARD_TOKEN`. Never print the token or put it
   in an argument, request file, transcript, or repository.
3. Read readiness, registered profiles, qualified flags and actual capabilities.
   A registered name is not proof of a working driver. Unsupported requirements
   must remain explicit. Do not substitute another model, broaden policy, or
   weaken isolation to get a request through.
4. Use `branchyard describe` only when constructing or extending a command.
   Its generated contract is authoritative for shapes. Read
   [the operations reference](references/operations.md) for semantics.

## Submit a bounded operation

- Reuse the task and graph identities supplied by the caller. Inspect their
  current revisions before proposing edits. A child shares the root's admission
  budget; specifying child limits cannot mint new authority.
- Create only the collaborators needed for the current work. `apply_graph`
  proposes a bounded atomic delta. Dependencies and ownership are different.
- Use registered server profiles and immutable source commits/checkpoints.
  Component sharing is explicit. A checkpoint fork does not copy a conversation,
  credentials, or grant network access.
- Write the full command to a caller-owned file before first submission. Generate
  IDs with `branchyard new-id`; preserve that operation ID and exact logical
  request through disconnects. Keep command files out of committed source when
  goals contain private task context.
- Run `branchyard validate --file request.json`. This checks shape and bounds,
  not authorization, graph acyclicity, budgets, or server support.
- Run `branchyard call --file request.json` once under the authority already
  granted by the task. No extra approval is implied by this skill.
- Retain the operation ID, request fingerprint and task IDs. An accepted receipt
  means admission, not successful execution or accepted code.

## Observe and recover

- Inspect with `branchyard task ID` and read bounded pages with
  `branchyard events ID --after CURSOR --limit 50`. Save `next_after` only after
  consuming the page. Cite artifact IDs; do not dump entire logs into context.
- Exit 4 means submission outcome unknown. Use
  `branchyard reconcile --file request.json`; do not mint a replacement ID.
  A not-found lookup alone does not prove that an in-flight request cannot commit.
- HTTP 409 requires inspection: stale revision or ID/input conflict. Replanning
  after a definite rejection needs a new saved command and operation ID.
- Explicit `cancel_task` requests cancellation. Exiting the CLI, dropping an SDK
  future, or a disconnected client does not cancel accepted work. Inspect until
  terminal state; `cancel_requested` does not establish resource revocation.
- A completed task is not a validated merge. Candidate validation and guarded
  promotion are later server capabilities; do not claim they occurred.

Report the observed state, IDs and any blocked prerequisite. The current package
includes a durable admission server. Execution workers and sandbox isolation are
not yet qualified; a successful operation does not mean a harness ran.
