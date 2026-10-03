# Effects, approvals and undo

> **Status.** Branchyard's side is built and tested hermetically against a mock gateway (3 October 2026, branch `agent/effects-16`); Anvil's side is being built in parallel to the same contract. This page is the contract both sides build to; sections say which side owns what. [Branchyard side](#branchyard-side) says what was built, how each effect is written to the ledger before its call, and the wire as Branchyard reads it; [What is not done](#what-is-not-done) lists the limits.

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

## Branchyard side

Built in [`branchyard::effects`](../crates/branchyard/src/effects/mod.rs), with the policy in [`branchyard_provision::approvals`](../crates/branchyard-provision/src/approvals.rs), `by approvals`, `by effects` and `by undo` in [`effects_cmd.rs`](../crates/branchyard-cli/src/effects_cmd.rs), and the server's routes in [`effects_routes.rs`](../crates/branchyard-server/src/effects_routes.rs).

### Begun before the call

Harnesses call the gateway themselves, with Anvil's packaged SDKs and CLIs. So that every effectful call is written to the ledger before it is made, each turn of a branch with connectors gets a **ledger proxy** of its own: an HTTP reverse proxy in the engine's process, like the [model gateway](model-gateway.md), on loopback, for as long as the turn runs. The harness is given the proxy as `ANVIL_GATEWAY_URL`; the token is unchanged, and its audience is still the gateway's own URL, which the proxy forwards to. A turn's network policy allows the proxy, not the gateway, so a confined harness cannot reach the gateway around it.

The proxy passes everything through as it is (`initialize`, `tools/list`, session deletes, Anvil's other shapes), except a `tools/call`:

1. It must carry this turn's token. The operation is classified before the call from what `tools/list` declares (the proxy lists the gateway's tools once per turn, with the turn's token): `_meta.effect.class`, else `read` for a tool annotated `readOnlyHint`, else `irreversible`. A read passes through. So does any call on a connector the grant only lets read: the gateway refuses its writes itself.
2. The approval policy decides (below). `block` answers a tool error, `approval_blocked`, and records `blocked` on the branch. `ask` stores an ask and waits. `stage` stages the call.
3. A call that goes ahead is opened in the ledger as `begun`, committed (SQLite `synchronous=FULL`; PostgreSQL a durable commit), **before** it is forwarded. A ledger that cannot be written refuses the call; it is never made unrecorded. The entry's id is sent as `Idempotency-Key` and `_meta.idempotency_key`.
4. The answer finishes the entry: `confirmed` with the gateway's `_meta.effect` (or `X-Anvil-Effect`); `confirmed`, `irreversible` and without undo when the gateway described nothing; `failed` on a JSON-RPC error or a tool error; `unknown` when the answer was lost after the call was sent, or a 5xx came without one. The harness gets the gateway's answer as it was.

An engine that stops between steps 3 and 4 leaves the entry `begun`. Recovery (every `Yard::open`, a server's recovery interval) moves a `begun` entry whose turn is no longer running to `unknown`, without network. Reconciliation then asks the upstream; it never calls the operation again.

A sandboxed turn reaches the proxy at `[connectors] effects_sandbox_host` (a server's `connectors.effects_sandbox_host`), the proxy binding `effects_listen`. Without one, a sandboxed turn calls the gateway directly and says so in a warning; its calls then reach the ledger only from the audit log, after the fact and unapproved. `effects_proxy = false` turns the proxy off for every turn.

### The audit log as a second source

When the gateway's audit log is read (`connector_call` events), a line whose `idempotency_key` names an `unknown` entry settles it: `confirmed` when it was allowed with a 2xx (or no status), else `failed`. A line without a ledger entry that says its effect (`effect.class`, or `effect_class`) is recorded as an entry of its own, `confirmed` (or `unknown` without a 2xx), never approved, with a detail saying it did not go through the proxy; the line's own text makes its id, so a line read twice is recorded once. A line that says neither is only a `connector_call` event.

### The wire, as Branchyard reads it

What Branchyard sends and reads, for Anvil to match:

| Where | What |
|---|---|
| `tools/list`, each tool | `annotations.readOnlyHint`; `_meta.effect`: `class`, `deletion` (bool), `draft` (bool or an object), `lookup` (`{"operation", "arguments"}`), `operation` (the AIR id, also matched by policy patterns) |
| `tools/call` request | `params._meta.idempotency_key` and the `Idempotency-Key` header (the entry's id); `params._meta.stage: true` for the draft form; `params._meta.promote: "<handle>"` to perform a draft |
| `tools/call` result | `result._meta.effect` (or the `X-Anvil-Effect` header, JSON): `class`; `undo` (`null` for none, or `{"operation", "arguments", "kind": "inverse"\|"compensate", "deadline_ms"?, "summary"?}`); `deadline_ms` (milliseconds since the epoch); `idempotency_key` (the key used upstream); `summary` (one line for people, such as `message in #board`); `staged.handle` for a draft |
| A lookup's result | `result._meta.effect.lookup`: `found` (bool), and when found, the call's own `class` and `undo` |
| Audit lines | `idempotency_key`; `effect.class` (and `effect.undo`) for a call that did not go through the proxy |

A lookup is the declared operation called with its fixed `arguments` and `idempotency_key` set to the entry's id. An inverse is its `undo.operation` called with `undo.arguments` and the key `<entry id>-undo`, so a retried undo is done once.

### Approvals

Each tool and each connector operation resolves to `allow`, `ask`, `block` or `stage`. The layers, in order:

| Layer | Locally | On a server |
|---|---|---|
| An administrator's locked policy | `Yard::use_approvals` (`ApprovalSettings::admin`) | `approvals.admin` in the configuration file |
| The seat's or rig's | `Provisioning::approvals`: a rig seat's `approvals`, `provision.approvals` | the same, over HTTP |
| The person's | `[approvals]` in `branchyard.toml` or your user file | `approvals.people.<principal>` |
| The preset's | `--permissions`: `read-only` and `edit-worktree` block every effectful class; `full` keeps the defaults | `policy.preset` |

The first of the seat, person and preset layers with a matching rule or class setting decides; without one, the class's default does (the table above). A deletion (declared, or an operation whose last word is delete, remove, destroy, purge, erase, trash or unlink) is then asked at least. Last, the administrator's policy is a floor: the result is the stricter of the two, and only an administrator's `deletion` changes how deletions are treated. Strictness, loosest first: `allow`, `stage`, `ask`, `block`.

```toml
[approvals]
rules = { "github:issues.*" = "allow", "gmail:*" = "stage", "Bash" = "ask" }
classes = { compensable = "allow" }
```

A pattern names a tool (`Bash`, `mcp__*`) or `connector:operation`; both sides are globs (`*`, `?`), an operation glob may leave out the service prefix, and the most specific pattern wins (the most literal characters), the stricter between equals. A delegated child's approvals are its own (or its seat's) held within its parent's: they may only be stricter.

For **tools**, approvals only tighten: a tool the turn's permission policy denies stays denied, whatever an approval says (a planning turn stays read-only); an `ask` or `stage` on a tool the policy allowed asks a person (`DecisionSource::Approval`). A tool no layer names is left to the policy.

**Asks.** An ask is an `ApprovalAsk` in the store (`approval_asks`, `by_approval_asks`): what it is about, the call's arguments or the tool's input, the policy's decision and layer, and a deadline (the turn's budget). It is recorded on the branch (`Activity::Effect`, `asked`), escalated to a delegating parent's inbox, shown as a notice (`approval`) on the companion page and by push. Any surface answers it once:

| Surface | How |
|---|---|
| `by approvals [ls [--all]]`, `allow ID`, `deny ID [--reason TEXT]`, or `--branch B` for the oldest waiting on B | `Yard::answer_approval`; locally, with `--remote`, or in a delegating parent's shell |
| `by watch` | `A` allows the selected branch's oldest waiting approval, `D` denies it with a reason |
| The companion page | the Approvals view: Allow or Deny, as the token's principal, surface `companion` |
| HTTP | `GET /v1/repos/{repo}/approvals[?all=true]`, `POST …/approvals/{id}/allow` and `…/deny` (`ApprovalAnswerRequest`: `reason`, `surface`), as the caller |
| A delegating parent | `Delegate::answer_approval`, the MCP `answer_approval` tool, `by approvals allow` in its shell; only an ancestor |

The waiting turn sees the answer at once in the same process, and within 100 ms from another. A denied call answers a tool error, `approval_denied`. When the turn's budget runs out, or the turn ends, the ask is answered `expired` (denied).

### Staged effects

A `stage` decision opens the entry `staged` with an ask to promote it. An operation that declares a draft form is called with `_meta.stage: true` and its draft's handle kept; one without is held in the outbox (the ask keeps the call's arguments) and the harness is told it is staged and has not happened. Allowing the ask, `by effects promote ID` or `POST …/effects/{id}/promote` performs it (`begun` first, as any effect, under the same key), with `_meta.promote` for a draft; denying it fails the entry, which never happened. `by merge --promote-effects` performs a branch's staged effects before merging it.

### Reconciliation

`by effects reconcile` (`Yard::reconcile_effects`, `POST …/effects/reconcile`), the gateway's supervisor every 30 seconds, and a server on its recovery interval: `begun` entries whose turn ended become `unknown`; `unknown` ones with a declared lookup are looked up through the gateway with a token for the branch's grant (five minutes, `by_turn` the entry's turn), `found` is `confirmed` and not found `failed`; those without a lookup, or whose lookup cannot answer, stay `unknown` and are listed with why. A `confirmed` entry whose undo's deadline passed becomes `expired`.

### Undo

```
by undo BRANCH [--to TURN] [--plan] [--only ID...] [--yes] [--json]
```

`--to` is the checkpoint (default 0, the branch's base); effects of later turns are planned. The plan is printed as in [Undo](#undo) above, each line with the entry's short id, and asks which upstream effects to undo (empty for every reversible one and every staged call, `all` adds the compensable ones, `none`, or ids); `--yes` takes the default and `--only` names them; without a terminal one of the two is needed. Then the branch is rewound to the checkpoint (files and conversation, exactly), and each chosen inverse is performed through the gateway under the branch's grant, approved by you (`undo_approval` on the entry) and allowed by the policy (a `block` refuses it). Its outcome is recorded on the original entry: `undone`, `compensated`, `undo_failed` with the upstream's answer (a lost answer too: it may have happened), or `expired`, without a call, when its deadline passed. A staged call is discarded (`failed`). `--plan` changes nothing. On a server, `GET …/branches/{b}/undo?to=N` is the plan and `POST …/branches/{b}/undo` (`UndoRequest`: `to`, `only`) performs the upstream part; files are rewound where the branch runs.

`by rewind` says what a rewind leaves upstream when later turns had effects, with the `by undo` that would plan them.

### What you see

`by effects [--branch B] [--json]` lists the ledger; `by effects show ID` an entry and its events. `by show` adds an `effects` line (and `effects` in `--json`: entries by state, approvals waiting); `by log` an `effect` line per change and ask; `by watch` shows the latest as what the branch is doing. `/metrics` adds `branchyard_effects{repo, class, state}` and `branchyard_approvals_pending{repo}`.

### Stores

SQLite: `effects` (the projection), `effect_events` (append-only) and `approval_asks`, created in the schema block. PostgreSQL: `by_effects`, `by_effect_events` and `by_approval_asks`, each made on its own when the catalog says it is missing. Each change writes its event and the projection in one transaction, from an expected state, so two finishers cannot both win; the conformance check (`crate::conformance::effects`) runs on SQLite, PostgreSQL and an in-memory backend, and checks that the events project to what is stored.

### Tests

- Policy: resolution order, the administrator's floor, specificity, deletions, children only stricter (`branchyard-provision`, 6); presets' approvals.
- Store: the conformance check on SQLite, PostgreSQL and memory: opened once, moves only from the named states, one winner among four racing handles, events in order projecting to the stored entry, asks answered once.
- Engine (`crates/branchyard/tests/effects.rs`, 12, a mock gateway on loopback and the fake ACP agent calling it as the SDKs do): an effect begun before its call (the mock reads the ledger when the call arrives) and finished from `_meta.effect` over an event stream, a read not ledgered; missing metadata is irreversible; an administrator's block holds over a person's allow; a deletion asks and is answered through the SDK as from the API, once; a denied ask never calls; a tool approval asks and never loosens a deny; a child's ask escalated to its parent and answered by it, an outsider refused; a lost answer unknown, never retried, settled by its lookup; a draft staged then promoted under the same key, and an outbox call made when its ask is allowed; an undo plan grouped by class with partial undo, a compensation and a discarded draft; an expired deadline and an inverse that fails; the audit log settling an unknown entry and recording a call around the proxy once.
- CLI (`crates/branchyard-cli/tests/effects.rs`, 2, the built `by`): `by effects`, `by show`, `by undo --plan` (text and JSON), `by rewind`'s note, `by undo --yes` rewinding and undoing; an ask answered by another `by`, once; a `by run` killed after the gateway did the effect leaving the entry unknown, then `by effects reconcile` confirming it through the lookup.
- Server (`crates/branchyard-server/tests/effects.rs`, 1): the administrator's lock over a principal's policy, an ask answered through the API as the caller from the companion surface, the ledger and an undo through the API, `/metrics`; the configuration file's `approvals` and `connectors.effects_*` (unit).

### What is not done

- Anvil's side is not built yet: the tests use a mock gateway that speaks [the wire](#the-wire-as-branchyard-reads-it) as described here. Until Anvil declares `_meta.effect`, every write is `irreversible` with no undo, and is staged by default; allow it with a rule to make it.
- The proxy reads each answer whole before giving it to the harness (up to 16 MB): a streamed tool result arrives at once.
- A harness's own HTTP client may time out while an ask waits; the turn's budget bounds the wait, not the client's.
- Approvals for tools resolve on the engine's thread, which waits for the answer; a cancel is noticed between polls.
- Effects of a delegated child are its own branch's: undoing a parent does not plan its children's.
- An audit line without `idempotency_key` that says its effect class is recorded as a call around the proxy even when it went through it; Anvil's lines must name the key they were given.
- No local administrator: a lock exists on a server, or through the SDK.
- Sandboxed turns: the proxy's address for a guest (`effects_sandbox_host`) was not tried on a KVM host or a Substrate cluster.


## What this does not do

It does not make an irreversible effect reversible, detect effects a harness makes outside the gateway (egress policy is what stops those), or undo effects of another person's task. Compensation is a new effect in the world, recorded as such.
