# Branch lifecycle

Three features learned from [Scion](comparison.md#scion) (see the [absorption plan](comparison.md#scion-1)): a running turn that has gone quiet is *stalled*; an operator can be notified of a repository's activity outside the SSE feed; and a branch whose harness or model needs to change can be *reincarnated* onto its latest work with a fresh session, rather than sent to on its old one. All three are tested hermetically against the fake ACP agent, without a real harness or network beyond loopback.

## Stall detection

A running turn with no harness protocol activity for its `Budget::stall_after` window is *stalled*: something the harness or the connection to it, not Branchyard's engine, has gone quiet. "Activity" is any event the driver reports — a message, a tool call, a permission request, its answer, a turn boundary — so a harness that is genuinely thinking but still emitting nothing (some drivers stream nothing between a tool call and its result) can still register as stalled; raise the window past that harness's normal gaps.

```rust
use branchyard::{Budget, StallAction};

yard.task("...")
    .budget(Budget::default().stall_after(Duration::from_secs(300)).stall_action(StallAction::Interrupt))
    .run()?;
```

```sh
by run "..." --stall-after 5 --stall-action interrupt   # minutes
```

- **Recorded once.** The transition is `Activity::Stalled { since_ms }`, appended to the branch's event log like any other activity; new activity afterward appends `Activity::Resumed` and clears it. Both are ordinary entries in `by log`, the SSE feed, and a webhook delivery tagged `stall`.
- **Live, not only after the fact.** While the turn is stalled, `BranchInfo::stalled` and `Inspection::stalled` are `true` for a live reader — `by ls`, `by show`, `by inspect`, `Yard::branch` in another process — not only once the turn ends. This is a plain fenced write of the branch's record under the turn's lease, the same write `create` already does at acquisition; not a new journaled step, since [recovery](durability.md#recovery) never needs to reconstruct it. Once the turn ends, for any reason, its final record always has `stalled: false` — the field describes a running turn.
- **`stall_action`:** `notify` (the default) records the transition and keeps the turn running exactly as before. `interrupt` records it, then interrupts the turn the same way a budget limit does: the harness gets an interrupt, has 30 seconds to end the turn itself, and the branch ends `Interrupted`.
- **Excluded, so the four things a slow but healthy turn is normally doing are never mistaken for a stall:**
  - **Answering a permission request.** The engine delivers a policy's answer to the harness synchronously, inside its own event loop; while that call blocks — an `--ask` policy waiting on a person, say — the loop is not polling at all, so no stall check runs during it. The idle window is measured from when the answer was delivered, not from when the request arrived, so a long wait for a person never counts against the next check either.
  - **Waiting on a child.** A branch delegating to a child (`spawn`, `send` or `wait_subtree`, all of which block the calling turn) keeps its own status `Running` for as long as it waits, so the engine excludes a branch with any directly delegated child whose own status is `Running`, read fresh from the store on every check — not from the turn's own snapshot of its record, which would miss a child spawned during the very turn being checked.
  - **Waiting for an answer.** A harness blocked in `by ask --wait` (or `branchyard.ask(wait=…)`, the MCP `ask` tool's `wait_seconds`, `Delegate::ask` with a wait) is waiting on its parent, not stuck. The wait runs wherever the call is served — the process running the turn, through the broker, or `by serve` for `by ask --remote --wait` — so it is recorded durably: the question's row in the store's `messages` table carries the waiter's deadline (`awaiting_until_ms`) while the call blocks, and is cleared when it returns. The engine excludes a branch that has sent a question with no answer yet and a waiter whose deadline has not passed, read fresh from the store on every check, like a running child. A waiter that dies without clearing its row stops counting at its deadline; once the answer arrives or the wait ends, the turn's idle time counts again from its last activity.
  - **Waiting for background tasks.** A turn the driver holds open after the harness answered it, for background tasks the harness still reports running (Claude Code's `Bash` with `run_in_background`, a monitor, a subagent; `Driver::holding`), is waiting on that work: a long command in the background says nothing while it runs. Only the hold is excluded, not a turn that merely has background tasks while the model works, nor the wait for the follow-up cycle once they ended. The whole hold, the wait for the follow-up cycle included (`Driver::held`), is bounded by the turn's `max_duration`, or, with none, by 30 minutes from the turn's result, after which the turn is interrupted ([Claude Code](harness-integration.md#claude-code)).

`Budget::stall_after` is the caller's own choice for a turn, not part of a [delegation envelope](delegation.md#the-envelope): a parent narrows a child's cost, turns and duration, but never its stall window.

## Webhook notifications

An operator running `by serve` can have a target notified of a served repository's activity — status changes, stalls, permission requests — without polling the SSE feed themselves. See [server: webhooks](server.md#webhooks) for the flags, the JSON config, the envelope shape and the error handling, and [durability: webhook cursors](durability.md#webhook-cursors) for what makes delivery survive a restart. In short: `by serve --webhook URL [--webhook-secret FILE] [--webhook-events KINDS] [--webhook-insecure]`; each delivery is HMAC-SHA256 signed and carries its feed position as a dedupe key; delivery is at-least-once, retried with backoff, and a target that keeps refusing is dead-lettered to the server's log without blocking anything else.

This is a server-only addition: there is no SDK, local-CLI or delegation surface for it, since it notifies something outside the process running the engine, not a caller inside it. A caller that wants activity inside its own process already has `Yard::events_since`/`wait_for_events`; a caller in another process has the SSE feed or `by watch`.

### Webhook failure direction

A webhook is a best-effort, advisory channel: **its failure never fails or delays an operation, a turn, or the feed.** [`webhook::spawn`](../crates/branchyard-server/src/webhook.rs) runs each configured target on its own `tokio::spawn`ed task, reading the served repository's feed by cursor; nothing an operation, a turn's engine, or the feed itself does waits on that task, calls into it synchronously, or holds a lock it also needs. A target that hangs after accepting the connection, resets the connection, or answers `500` forever only ever slows *that task's own* retries (bounded backoff, `REQUEST_TIMEOUT` per attempt, a dead-letter note to the server's log after `MAX_ATTEMPTS`, both overridable for tests) — it never slows the operation whose activity it was delivering, a different branch's turn, or the feed's cursor for another webhook or a live SSE reader. This mirrors Straitjacket's own relay design, which answers a cross-harness call at most once and never blocks the primary call on it (`docs/comparison.md` Absorption plan → Straitjacket); Branchyard's webhook direction is the same idea applied to its notification channel, not ported code.

Adversarial tests in [`crates/branchyard-server/tests/webhook.rs`](../crates/branchyard-server/tests/webhook.rs) assert this directly, each timing the operation rather than only checking it eventually succeeds:

- `a_hanging_receiver_never_delays_the_operation_or_the_feed`: a receiver that reads a delivery in full and never answers, holding the connection open. The operation whose activity it was delivering completes well under the connection's stuck lifetime, and a second branch's operation completes just as fast while the first delivery's connection is still open.
- `a_receiver_that_resets_the_connection_never_delays_the_operation`: a receiver that reads a delivery and closes the connection without writing a response at all (a connection reset, from the client's side, looks the same whether closed gracefully or reset). The operation completes promptly, and a later operation is unaffected by a webhook that only ever fails this way.
- `a_receiver_that_never_succeeds_is_dead_lettered_and_the_cursor_still_advances`: a receiver answering `500` forever exhausts its retries and is dead-lettered, but the cursor still moves past it, so a second branch's activity is still attempted (not merely queued) within a reasonable time.
- `a_failing_receiver_is_retried_and_still_delivers`: two failures followed by success still gets the delivery through, without anything upstream noticing the retries happened.

Any future best-effort or advisory channel (a notification hook, a future audit sink) should keep this same shape: its own task, its own retry and give-up policy, and no call from the primary path into it that could block on it.

## Reincarnation

A branch's harness, model, or profile sometimes needs to change under work that is otherwise going fine — a new harness version fixes a bug the branch was working around, a cheaper model would do for what is left, a harness known to fork sessions turns out not to for this working directory. `Branch::send` cannot change harness, and `Branch::fork` keeps the same conversation (or refuses, when the harness cannot fork). Reincarnation is the third option: a new branch from the old one's latest candidate, always with a **fresh session**, whose first prompt is a **generated handoff brief** rather than something you write.

```rust
let reborn = branch.reincarnate(TaskOptions {
    harness: Some("codex".into()),           // omit to keep its own
    provision: Some(Provisioning { model: Some("gpt-5-codex".into()), ..Default::default() }),
    ..TaskOptions::default()
})?;
```

```sh
by reincarnate flaky --harness codex --model gpt-5-codex
```

The new branch:

- **Starts from the old branch's latest candidate**, like a fork (refused with `no_candidate` if there is none).
- **Always begins a fresh session.** Unlike `fork`, there is no `fresh_session` flag and no attempt at a native fork: reincarnation exists precisely for the cases a continued conversation cannot cross (a different harness, a model change some harnesses key their session to, a harness whose fork does not survive a new working directory), so it never tries to carry the old session over.
- **Gets a generated handoff brief as its first prompt**, not one you write: the original task, how many turns it took, its last message before the candidate, the candidate's diffstat and commit, why it was reincarnated (its last status, if not simply `Ready`), and, when the harness changed, a line naming the switch. Building it costs one read of the old branch's event log ([`run::handoff_brief`](../crates/branchyard/src/run.rs)); nothing about it is configurable yet beyond what `TaskOptions` already changes (a different `check`, `provision`, provider, and so on, each inherited from the old branch exactly as `fork` inherits them unless you override it).
- **Marks the old branch `superseded_by` the new one's name**, best-effort: an ordinary out-of-turn record write, like the ones delegation notes make, so it races harmlessly with anything else touching the old branch concurrently and never blocks the new branch from starting. `by show` prints it as "reincarnated as".

Reincarnation is not a delegation operation — it is available on any branch you own, not only a descendant a turn delegated to — so it has no MCP tool, Python or `Delegate` surface; act with your own authority (the SDK, `by`, or the server's `POST …/branches/{b}/reincarnate`) the way you would to fork or remove a branch. See [surfaces](surfaces.md#added-with-lifecycle-features) for the full table.
