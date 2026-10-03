# Effects, approvals and undo

> **Status.** Branchyard's side is built and tested hermetically against a mock gateway that speaks Anvil's wire, and against Anvil's own github-mini gateway in an opt-in test (3 October 2026, branch `agent/effects-16`). Anvil's side is built (its ADR-0030 and `docs/branchyard.md`). This page is the contract both sides build to; sections say which side owns what. [Branchyard side](#branchyard-side) says what was built, how each effect is written to the ledger before its call, and the wire both sides speak; [What is not done](#what-is-not-done) lists the limits.

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

Each tool, and each connector operation, resolves to `allow`, `ask`, `block` or `stage` from, in order: an administrator's locked policy (cannot be loosened below), the seat's or rig's policy, the person's policy, the preset. Defaults follow the class:

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

**Anvil (AIR and gateway)**, as built (Anvil's ADR-0030 and its `docs/branchyard.md`, "Effects and undo", as of Anvil `60a2cba`):

1. Each effectful operation declares `effect.class` and, where one exists, `effect.inverse` (the inverse operation and an argument mapping from the original request and response, with an optional `deadline`); `effect.compensate` likewise; `effect.lookup` for reconciliation by key or id; `effect.draft` for a draft form with its `promote` and `discard` calls. An operation that declares nothing is `read` if it is a read and `irreversible` otherwise.
2. `tools/list` publishes each tool's AIR operation id (`_meta["anvil/operation_id"]`) and, for an operation that declares one, the declaration (`_meta["anvil/effect_class"]`, `_meta["anvil/effect_contract"]`).
3. Every call reports its effect, `_meta.effect` (MCP) or the `X-Anvil-Effect` header (REST): the class, the AIR operation, the key that went upstream, the resolved `undo` with the gateway's `tool` to call, `deadline_ms`, a `compensate` for after the deadline, a concrete `lookup`, and `staged` for a draft. A failed call still reports its class, key and lookup.
4. The gateway takes `_meta.idempotency_key` (MCP) or `Idempotency-Key` (REST) and sends it upstream where the operation declares a carrier; its audit line records it as `ledger_id`.
5. An undo, a promotion or a discard is an ordinary call the caller makes, under the same grant; Anvil never undoes anything itself.

**Branchyard:**

1. Writes the ledger entry (`begun`) before the call, with the ID as the idempotency key; finishes it from the effect report.
2. Decides allow, ask, block or stage before the call; answers come from any surface.
3. Plans and performs undo from the ledger; reconciles `unknown` through the lookup.

An operation that declares nothing is `irreversible` with no undo: unknown is treated as the worst case.

## Branchyard side

Built in [`branchyard::effects`](../crates/branchyard/src/effects/mod.rs), with the policy in [`branchyard_provision::approvals`](../crates/branchyard-provision/src/approvals.rs), `by approvals`, `by effects` and `by undo` in [`effects_cmd.rs`](../crates/branchyard-cli/src/effects_cmd.rs), and the server's routes in [`effects_routes.rs`](../crates/branchyard-server/src/effects_routes.rs).

### Begun before the call

Harnesses call the gateway themselves, with Anvil's packaged SDKs and CLIs. So that every effectful call is written to the ledger before it is made, each turn of a branch with connectors gets a **ledger proxy** of its own: an HTTP reverse proxy in the engine's process, like the [model gateway](model-gateway.md), on loopback, for as long as the turn runs. The harness is given the proxy as `ANVIL_GATEWAY_URL`; the token is unchanged, and its audience is still the gateway's own URL, which the proxy forwards to. A turn's network policy allows the proxy, not the gateway, so a confined harness cannot reach the gateway around it.

The proxy passes everything through as it is (`initialize`, `tools/list`, session deletes, Anvil's other routes), except a call: MCP's `tools/call`, or Anvil's REST route `POST /call/<tool>` (`{"arguments", "stage"?}`), which the proxy ledgers the same way, so REST is no way around it.

1. It must carry this turn's token. The operation is classified before the call from what `tools/list` declares (the proxy lists the gateway's tools once per turn, with the turn's token): `_meta["anvil/effect_class"]`, else `_meta.effect.class`, else `read` for a tool annotated `readOnlyHint`, else `irreversible`. A deletion is told by its tool's name or AIR operation id (`delete`, `remove`, `trash` …). A read passes through. So does any call on a connector the grant only lets read: the gateway refuses its writes itself.
2. The approval policy decides (below). `block` answers a tool error, `approval_blocked`, and records `blocked` on the branch. `ask` stores an ask and waits. `stage` stages the call.
3. A call that goes ahead is opened in the ledger as `begun`, committed (SQLite `synchronous=FULL`; PostgreSQL a durable commit), **before** it is forwarded. A ledger that cannot be written refuses the call; it is never made unrecorded. The entry's id is sent as `Idempotency-Key` (and `_meta.idempotency_key` over MCP). The entry keeps the lookup the tool's contract declares, resolved from the call's arguments and that key, for an answer lost entirely.
4. The answer finishes the entry: `confirmed` with the gateway's effect report (`_meta.effect`, or `X-Anvil-Effect` over REST); `confirmed`, `irreversible` and without undo when the gateway described nothing; `failed` on a JSON-RPC error, a tool error or a REST error envelope (keeping the report's lookup); `unknown` when the answer was lost after the call was sent, or a 5xx came without one. The harness gets the gateway's answer as it was. The proxy's own refusals are tool errors over MCP and `{"error"}` envelopes over REST (`403` for a block or a denial); a call held in the outbox is answered `202` over REST.

An engine that stops between steps 3 and 4 leaves the entry `begun`. Recovery (every `Yard::open`, a server's recovery interval) moves a `begun` entry whose turn is no longer running to `unknown`, without network. Reconciliation then asks the upstream; it never calls the operation again.

A sandboxed turn reaches the proxy at `[connectors] effects_sandbox_host` (a server's `connectors.effects_sandbox_host`), the proxy binding `effects_listen`. Without one, a sandboxed turn calls the gateway directly and says so in a warning; its calls then reach the ledger only from the audit log, after the fact and unapproved. `effects_proxy = false` turns the proxy off for every turn.

### The audit log as a second source

When the gateway's audit log is read (`connector_call` events), a line whose `ledger_id` names an `unknown` entry settles it: `confirmed` when it was allowed with no `error_code` and a 2xx (or no status), else `failed`. A line with `staged_for` performed a draft, not the effect, and settles nothing; a `dry_run` line neither. A line without a ledger entry that names its `effect_class` (other than `read`) is recorded as an entry of its own, `confirmed` (or `unknown` without a 2xx), never approved, with a detail saying it did not go through the proxy; the line's own text makes its id, so a line read twice is recorded once. A line that says neither is only a `connector_call` event.

### The wire, as Branchyard reads it

It matches what Anvil ships (ADR-0030; Anvil's `docs/branchyard.md`, "Effects and undo"). The mock gateway the tests use speaks it, and `crates/branchyard-cli/tests/anvil_e2e.rs` runs it against Anvil's own `examples/github-mini` gateway.

| Where | What |
|---|---|
| `tools/list`, each tool | `annotations.readOnlyHint`; `_meta["anvil/operation_id"]` (the AIR id, also matched by policy patterns); `_meta["anvil/effect_class"]`; `_meta["anvil/effect_contract"]`: `{class, inverse?, compensate?, lookup?, draft?}`, whose mappings name operations by AIR id and take values from `request.<path>`, `response.<path>`, `idempotency_key` or `{const}`. A `draft` means the operation has a draft form. `_meta.effect.class` is read when there is no `anvil/effect_class` |
| A call | MCP: `params._meta.idempotency_key` and the `Idempotency-Key` header (the entry's id); `params._meta.stage: true` for the draft form. REST: `POST /call/<tool>` with `{"arguments", "stage"?}` and the `Idempotency-Key` header |
| Its effect report | `result._meta.effect`, or the `X-Anvil-Effect` header: `class`; `operation`; `idempotency_key` (the key that went upstream, or `null`); `undo` (`null`, or `{kind: "inverse"\|"compensate", operation, tool, arguments}`); `deadline_ms`; `compensate` (the same, with its own `deadline_ms`); `lookup` (`{by, operation, tool, arguments}` or `null`); `undo_unavailable` (why `undo` is `null`); `staged` (`{draft_operation, handle, promote, discard, unavailable?}`, each follow-up a `{operation, tool, arguments}` or `null`) |
| A lookup's answer | an answer is found; a `not_found` tool error, or an empty answer, is not found; anything else cannot tell |
| Audit lines | `ledger_id`, `effect_class`, `staged_for`, `error_code`, `decision`, `upstream_status` |

Every follow-up calls the `tool` the report named, with its `arguments`, and `confirm: true` (under the key its input schema names) when that tool requires confirmation: Branchyard's own approval decided it. An undo carries the key `<entry id>-undo` and a discard `<entry id>-discard`, so a retried one is done once; a promotion carries the entry's id. A lookup from the tools/list contract is resolved only when every value it needs comes from the request or the key and its operation's tool is listed; otherwise only the audit log can settle a lost answer.

### Staged effects

A `stage` decision opens the entry `staged` with an ask to promote it. An operation whose contract declares a draft form is called with `stage: true` (`_meta.stage` over MCP, the body's `stage` over REST), and the report's `staged` (the draft's handle, its promote and discard calls) is kept; one without is held in the outbox (the ask keeps the call's arguments) and the harness is told it is staged and has not happened. Allowing the ask, `by effects promote ID` or `POST …/effects/{id}/promote` performs it (`begun` first, as any effect, under the entry's id as key): a draft by calling `staged.promote.tool` with its arguments, refused when the gateway gave no promote call; an outbox call by making it. Denying it fails the entry, which never happened, and discards a draft with `staged.discard.tool`; a draft without a discard call is said to be still upstream. `by merge --promote-effects` performs a branch's staged effects before merging it.

### Reconciliation

`by effects reconcile` (`Yard::reconcile_effects`, `POST …/effects/reconcile`), the gateway's supervisor every 30 seconds, and a server on its recovery interval: `begun` entries whose turn ended become `unknown`; `unknown` ones with a lookup (the report's, or the contract's resolved when the call was made) are looked up by calling its `tool` with its `arguments` through the gateway with a token for the branch's grant (five minutes, `by_turn` the entry's turn): an answer is `confirmed`, `not_found` or nothing is `failed`; those without a lookup, or whose lookup cannot answer, stay `unknown` and are listed with why. A `confirmed` entry whose undo's deadline passed, with no compensation that still works, becomes `expired`.

### Undo

```
by undo BRANCH [--to TURN] [--plan] [--only ID...] [--yes] [--json]
```

`--to` is the checkpoint (default 0, the branch's base); effects of later turns are planned. The plan is printed as in [Undo](#undo) above, each line with the entry's short id, and asks which upstream effects to undo (empty for every reversible one and every staged call, `all` adds the compensable ones, `none`, or ids); `--yes` takes the default and `--only` names them; without a terminal one of the two is needed. Then the branch is rewound to the checkpoint (files and conversation, exactly), and each chosen inverse is performed through the gateway under the branch's grant, approved by you (`undo_approval` on the entry) and allowed by the policy (a `block` refuses it). The call is the report's `undo.tool`; past the inverse's deadline the report's `compensate` is offered instead (in the compensable group). Its outcome is recorded on the original entry: `undone`, `compensated`, `undo_failed` with the upstream's answer (a lost answer too: it may have happened), or `expired`, without a call, when its deadline passed and there is no compensation. A staged call is discarded (`failed`): an outbox call by dropping it, a draft by calling its `discard.tool`; a draft without a discard call is reported as not discardable and stays staged. An effect without an undo says why when the gateway did (`undo_unavailable`). `--plan` changes nothing. On a server, `GET …/branches/{b}/undo?to=N` is the plan and `POST …/branches/{b}/undo` (`UndoRequest`: `to`, `only`) performs the upstream part; files are rewound where the branch runs.

`by rewind` says what a rewind leaves upstream when later turns had effects, with the `by undo` that would plan them.

### What you see

`by effects [--branch B] [--json]` lists the ledger; `by effects show ID` an entry and its events. `by show` adds an `effects` line (and `effects` in `--json`: entries by state, approvals waiting); `by log` an `effect` line per change and ask; `by watch` shows the latest as what the branch is doing. `/metrics` adds `branchyard_effects{repo, class, state}` and `branchyard_approvals_pending{repo}`.

### Stores

SQLite: `effects` (the projection), `effect_events` (append-only) and `approval_asks`, created in the schema block. PostgreSQL: `by_effects`, `by_effect_events` and `by_approval_asks`, each made on its own when the catalog says it is missing. Each change writes its event and the projection in one transaction, from an expected state, so two finishers cannot both win; the conformance check (`crate::conformance::effects`) runs on SQLite, PostgreSQL and an in-memory backend, and checks that the events project to what is stored.

### Tests

- Policy: resolution order, the administrator's floor, specificity, deletions, children only stricter (`branchyard-provision`, 6); presets' approvals.
- Store: the conformance check on SQLite, PostgreSQL and memory: opened once, moves only from the named states, one winner among four racing handles, events in order projecting to the stored entry, asks answered once.
- Wire (unit, `effects::mcp`, `effects::proxy`): effect reports from `_meta` and the header; `tools/list` contracts and the fallback; Anvil's mapping grammar and lookups resolved from it; found and `not_found`; confirmation filled from a schema; REST tool names and staging.
- Engine (`crates/branchyard/tests/effects.rs`, 17, a mock gateway on loopback speaking Anvil's wire and the fake ACP agent calling it as the SDKs do): an effect begun before its call (the mock reads the ledger when the call arrives) and finished from `_meta.effect` over an event stream, its undo and compensation named by tool, a read not ledgered; a REST call ledgered and staged like an MCP one, a blocked one refused `403`; missing metadata is irreversible; an administrator's block holds over a person's allow; a deletion asks and is answered through the SDK as from the API, once; a denied ask never calls; a tool approval asks and never loosens a deny; a child's ask escalated to its parent and answered by it, an outsider refused; a lost answer unknown, never retried, settled by the lookup resolved from the tool's contract; a draft staged then promoted with its promote call under the same key, and an outbox call made when its ask is allowed; a denied draft discarded with its discard call, and one without a discard call reported as not discardable; an undo plan grouped by class with partial undo, a confirmed compensation and a discarded outbox call; an expired deadline and an inverse that fails; past the deadline, the compensation instead; the audit log settling an unknown entry by `ledger_id`, skipping a draft's line, and recording a call around the proxy once.
- Against Anvil (`crates/branchyard-cli/tests/anvil_e2e.rs`, ignored; needs `node`, `python3` and a built Anvil at `ANVIL_BIN`, and skips saying why otherwise): Anvil's `github-mini` gateway behind the proxy, comments made over MCP and REST ledgered with Anvil's reported inverse, a release staged as Anvil's draft, then `by undo` deleting both comments and discarding the draft upstream.
- CLI (`crates/branchyard-cli/tests/effects.rs`, 2, the built `by`): `by effects`, `by show`, `by undo --plan` (text and JSON), `by rewind`'s note, `by undo --yes` rewinding and undoing; an ask answered by another `by`, once; a `by run` killed after the gateway did the effect leaving the entry unknown, then `by effects reconcile` confirming it through the lookup.
- Server (`crates/branchyard-server/tests/effects.rs`, 1): the administrator's lock over a principal's policy, an ask answered through the API as the caller from the companion surface, the ledger and an undo through the API, `/metrics`; the configuration file's `approvals` and `connectors.effects_*` (unit).

### What is not done

- A connector compiled without effect declarations has every write `irreversible` with no undo, staged by default; allow it with a rule to make it.
- An `ask` on a REST call waits as on MCP; an outbox call held for a REST harness is answered `202`, which Anvil's REST route itself never answers.
- The proxy reads each answer whole before giving it to the harness (up to 16 MB): a streamed tool result arrives at once.
- A harness's own HTTP client may time out while an ask waits; the turn's budget bounds the wait, not the client's.
- Approvals for tools resolve on the engine's thread, which waits for the answer; a cancel is noticed between polls.
- Effects of a delegated child are its own branch's: undoing a parent does not plan its children's.
- An audit line without `ledger_id` that names its effect class is recorded as a call around the proxy; a line for a call that went through it always carries its `ledger_id`.
- No local administrator: a lock exists on a server, or through the SDK.
- Sandboxed turns: the proxy's address for a guest (`effects_sandbox_host`) was not tried on a KVM host or a Substrate cluster.


## What this does not do

It does not make an irreversible effect reversible, detect effects a harness makes outside the gateway (egress policy is what stops those), or undo effects of another person's task. Compensation is a new effect in the world, recorded as such.
