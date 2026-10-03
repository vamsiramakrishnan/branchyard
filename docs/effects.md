# Effects, approvals and undo

> **Status.** Design, 3 October 2026. Being built in Wave 5 (approvals and the effect ledger) with Anvil's side in parallel. This page is the contract both sides build to; sections say which side owns what.

Rewinding a task restores its files and its conversation (see [task repositories](task-repos.md)). It cannot unsend an email. Branchyard therefore keeps two histories:

- **The task's own history**, in git: files, the conversation, approvals. Undo here is exact.
- **What the task did to the world**, in the effect ledger: every call that changes something outside the machine. Undo here is only what the upstream supports, and Branchyard says so before it acts.

## Effect classes

Every effect has one class, decided by the operation that caused it (Anvil declares it; see below), never guessed by a harness.

| Class | Meaning | Examples |
|---|---|---|
| `reversible` | The upstream offers a true inverse, which leaves the world as it was | delete a comment, message or calendar event this task created; restore a file's previous version; revert a commit; close a pull request it opened |
| `compensable` | The upstream offers an action that cancels the effect but leaves a trace | close (not delete) an issue; refund a payment; reopen a ticket; send a correction |
| `irreversible` | No undo exists | an email delivered; a webhook a third party received; a message already read; data made public |
| `read` | Changes nothing | lists, searches, fetches |

An inverse may expire (a provider allows deleting a message only for a while). The ledger keeps the deadline, and undo shows it.

## The ledger

One entry per effectful call, written before the call and finished after it, so a crash leaves an entry that says what is unknown rather than nothing:

| Field | Meaning |
|---|---|
| `id` | ULID; also the call's idempotency key upstream where the operation takes one |
| `task`, `branch`, `turn`, `subject` | Where it came from and on whose behalf |
| `connector`, `operation`, `account` | What was called, for which connected account |
| `class` | `reversible`, `compensable`, `irreversible` |
| `state` | `staged` → `begun` → `confirmed` \| `failed` \| `unknown`; then `undone`, `compensated`, `undo_failed`, `expired` |
| `request_digest` | blake3 of the canonical request; never the request's secrets |
| `undo` | Present when the class allows it: the inverse operation, its arguments captured from the response (a created comment's ID, a file's previous version), and `deadline_ms` |
| `approval` | Who approved it, when, and through which surface, when policy asked |

`unknown` entries are reconciled, never retried blindly: the reconciler asks the upstream through the operation's declared lookup (by idempotency key or captured ID) and moves the entry to `confirmed` or `failed`. When no lookup exists the entry stays `unknown` and is shown to the person.

Stores: the server's operation store (SQLite, PostgreSQL) and, locally, the yard's store; the same conformance suite as every other backend. Entries are append-only events; the current state is a projection.

## Approvals

Each tool, and each connector operation, resolves to `allow`, `ask` or `block` from, in order: an administrator's locked policy (cannot be loosened below), the seat's or rig's policy, the person's policy, the preset. Defaults follow the class:

| Class | Default |
|---|---|
| `read` | `allow` |
| `reversible` | `allow` |
| `compensable` | `ask` |
| `irreversible` | `stage`: written as a draft and held until approved |
| any deletion | always `ask`, whatever the policy says, unless an administrator's policy says otherwise |

An `ask` is a question in the person's inbox, answerable from `by watch`, the companion page, the server's API or a delegating parent; the turn waits, within its budget. Every answer is recorded on the ledger entry and in the task's history.

## Staged effects

The safest undo is not doing the thing yet. An `irreversible` operation that declares a draft form (an email draft, an unpublished post, a draft pull request) is performed as the draft; the real effect happens only when the person approves the draft or accepts the task. One without a draft form is held in an outbox until approved. Staged entries are `staged` in the ledger and listed in `by effects`.

## Undo

`by undo TASK [--to TURN]` (and the same in the UI) plans before it acts:

```
Rewinding "board update" to turn 3:
  files and conversation       restored exactly
  upstream, can be undone       slack: message in #board (deletable until 14:05)
                                calendar: event "Board prep" Thu 10:00
  upstream, can be compensated  github: issue #42 will be closed, not deleted
  upstream, cannot be undone    gmail: email to finance@ was delivered 09:12
                                  -> a correction draft is ready to review
Undo which upstream effects? [all reversible] ...
```

Undo of an upstream effect is a new effectful call (the inverse), made through the same gateway with the same grant, approved like any other, and recorded on the original entry. Partial undo is normal. Undo never claims more than happened: an inverse that fails leaves `undo_failed` with the upstream's answer.

## The contract with Anvil

Anvil owns what an operation means; Branchyard owns the ledger, approvals and undo plans.

**Anvil (AIR and gateway):**

1. Each effectful operation declares `effect.class` and, where one exists, `effect.inverse`: the inverse operation's name and an argument mapping from the original request and response (JSONPath-like), and an optional `deadline` rule; `effect.compensate` likewise for compensable ones; `effect.lookup` for reconciliation by idempotency key or ID; `effect.draft` for an operation's draft form.
2. The gateway, for every effectful call, returns `_meta.effect` (MCP) or `X-Anvil-Effect` (REST) with: the class, the resolved `undo` (inverse operation and concrete arguments), `deadline_ms`, and the upstream idempotency key it used.
3. The gateway accepts `Idempotency-Key` (or `_meta.idempotency_key`) and passes it upstream where the operation supports one.
4. The gateway accepts a call to the inverse like any other call, under the same grant; it does not undo on its own.
5. `stage: true` on a call with a draft form performs the draft and returns its handle; `promote` on that handle performs the real effect.

**Branchyard:**

1. Writes the ledger entry (`begun`) before the call, with the ID as the idempotency key; finishes it from `_meta.effect`.
2. Decides allow, ask, block or stage before the call; answers come from any surface.
3. Plans and performs undo from the ledger; reconciles `unknown` through `effect.lookup`.

Connectors compiled before these declarations exist have `class: irreversible` and no undo: unknown is treated as the worst case.

## What this does not do

It does not make an irreversible effect reversible, detect effects a harness makes outside the gateway (egress policy is what stops those), or undo effects of another person's task. Compensation is a new effect in the world, recorded as such.
