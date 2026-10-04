//! Harness-to-harness messages: a durable inbox in the store.
//!
//! A branch may `ask` a question or `report` to its parent; `escalate` goes
//! to the parent too, or further up an ancestor its rig seat's
//! `escalates_to` names ([`crate::Seat::escalates_to`]); an `answer` goes
//! from a branch to one of its own descendants, usually replying to a
//! question. [`authorize`] is the one authority check every surface goes
//! through; it follows the delegation tree exactly as spawning does.
//!
//! Every message is durable ([`crate::state::Backend`]'s `messages` rows,
//! both SQLite and PostgreSQL) and is also recorded as `Activity::Message`
//! on the sending and, when different, the receiving branch's event log, so
//! `by events`/`log` show it.
//!
//! Delivery follows Warp's `parent_bridge` mailbox: a message is *pending*
//! until it is acknowledged, and acknowledging it (marking it delivered) is
//! the same store write as recording it, so a crash never delivers a
//! message twice and never silently drops one. There are two paths, and
//! each records `Activity::MessagesDelivered` on the recipient's log naming
//! it:
//!
//! - **Steering** (the default [`DeliveryHook`], [`SteerDelivery`]): when
//!   the recipient has a running turn whose harness takes input mid-turn,
//!   the message is queued as steered input for that turn, linked to the
//!   message in the same store transaction. The engine running the turn
//!   marks the message delivered in the transaction that settles the input
//!   as written or accepted, and returns it to pending if the harness
//!   refuses it or the turn ends first.
//! - **Turn start**: otherwise, a branch's pending messages are given to it
//!   at the start of its next turn, prepended to the prompt as one
//!   delimited, size-bounded block; whatever does not fit stays pending,
//!   with a note of how many messages are still queued. They are marked
//!   delivered in the transaction that journals the turn's `submit` step
//!   ([`begin_submit`]), so they count delivered exactly when the prompt
//!   counts submitted. A message whose
//!   steered input is still queued for the turn that is starting is left
//!   to it, and marking a message delivered here unlinks it from any
//!   steered input, which the engine then refuses unwritten: never both.

use std::time::Duration;

use crate::state::Store;
use crate::{Error, Message, MessageKind, SteerState, Yard};

/// At most this many pending messages are rendered into one prompt; the
/// rest stay pending for a later turn.
const RENDER_MAX: usize = 20;
/// At most this many characters of pending messages are rendered at once
/// (excluding the wrapping tags and note), so one very large backlog cannot
/// blow out a turn's prompt.
const RENDER_CHARS: usize = 8_000;

/// Wraps the delivered block in a turn's prompt, so a harness (and a
/// person reading the transcript) can tell it from the task itself.
pub(crate) const OPEN_TAG: &str = "<branchyard-inbox>";
pub(crate) const CLOSE_TAG: &str = "</branchyard-inbox>";

/// Delivers a pending message into a branch's turn while it is already
/// running. Every [`Yard`] starts with [`SteerDelivery`]; replace it with
/// [`crate::Yard::set_delivery_hook`], or clear it with
/// [`crate::Yard::clear_delivery_hook`] so that every message waits for the
/// branch's next turn to start ([`begin_submit`]).
pub trait DeliveryHook: Send + Sync {
    /// Try to deliver `message` to `branch`'s current turn right now, from
    /// `yard`. `true` acknowledges it: it will not be delivered again, at a
    /// turn boundary or otherwise. `false` leaves it pending.
    fn try_deliver(&self, yard: &Yard, branch: &str, message: &Message) -> bool;
}

/// The default [`DeliveryHook`]: steer the message into the recipient's
/// running turn ([`crate::Yard::steer_as`], from the sender), and wait up
/// to [`SteerDelivery::wait`] for the engine running it to write it to the
/// harness. No running turn ([`Error::NotRunning`]), a harness that cannot
/// take input mid-turn ([`Error::Unsupported`]), or a refusal leaves the
/// message pending for the recipient's next turn start. A steer still
/// queued when the wait ends stays linked to the message, and the engine
/// marks the message delivered when it writes it.
#[derive(Clone, Debug)]
pub struct SteerDelivery {
    /// How long a sender waits for its message to reach the running turn.
    pub wait: Duration,
}

impl Default for SteerDelivery {
    fn default() -> SteerDelivery {
        SteerDelivery {
            wait: Duration::from_secs(2),
        }
    }
}

impl DeliveryHook for SteerDelivery {
    fn try_deliver(&self, yard: &Yard, branch: &str, message: &Message) -> bool {
        let text = render_running(message);
        let Ok(steer) = crate::steer::queue(yard, branch, &text, &message.from, Some(message.id))
        else {
            // `NotRunning`, `Unsupported`, too long: the next turn start.
            return false;
        };
        match yard.wait_steer(branch, steer.id, self.wait) {
            Ok(steer) => matches!(steer.state, SteerState::Delivered | SteerState::Accepted),
            Err(_) => false,
        }
    }
}

/// One message as steered into a running turn: the same delimited block as
/// at a turn's start.
pub(crate) fn render_running(message: &Message) -> String {
    format!(
        "{OPEN_TAG}\nA message arrived during your turn. Reply to a question with \
         `by answer <id> \"...\"`.\n\n{}{CLOSE_TAG}",
        line(message)
    )
}

fn line(message: &Message) -> String {
    format!(
        "[#{}] {} from {}: {}\n",
        message.id, message.kind, message.from, message.text
    )
}

/// Whether `from` may send a message of `kind` to `to`, given the
/// delegation tree: a `question` or a `report` go only to `from`'s parent;
/// an `escalation` goes to the parent too, or further up an ancestor whose
/// seat `from`'s seat's `escalates_to` names; an `answer` goes from an
/// ancestor to one of its own descendants.
pub(crate) fn authorize(
    store: &Store,
    from: &str,
    kind: MessageKind,
    to: &str,
) -> Result<(), Error> {
    if from == to {
        return Err(Error::Denied(format!("{from} cannot message itself")));
    }
    match kind {
        MessageKind::Answer => {
            if crate::delegation::is_ancestor(store, from, to)? {
                Ok(())
            } else {
                Err(Error::Denied(format!(
                    "{from} may only answer its own descendants, not {to}"
                )))
            }
        }
        MessageKind::Question | MessageKind::Report => {
            let parent = store.read(from)?.info.parent;
            match parent.as_deref() {
                Some(p) if p == to => Ok(()),
                Some(p) => Err(Error::Denied(format!(
                    "{from} may only {kind} its parent, {p}"
                ))),
                None => Err(Error::Denied(format!("{from} has no parent to {kind}"))),
            }
        }
        MessageKind::Escalation => {
            let parent = store.read(from)?.info.parent;
            if parent.as_deref() == Some(to) {
                return Ok(());
            }
            if crate::delegation::is_ancestor(store, to, from)? && escalates_to(store, from, to)? {
                return Ok(());
            }
            Err(Error::Denied(format!(
                "{from} may only escalate to its parent{}",
                match parent {
                    Some(p) => format!(", {p}, or an ancestor its rig seat's escalates_to names"),
                    None => String::new(),
                }
            )))
        }
    }
}

/// Whether `from`'s rig seat lists `to`'s seat in its `escalates_to`.
fn escalates_to(store: &Store, from: &str, to: &str) -> Result<bool, Error> {
    let from_seats = store.read(from)?.grant.and_then(|g| g.seats);
    let to_seats = store.read(to)?.grant.and_then(|g| g.seats);
    Ok(match (from_seats, to_seats) {
        (Some(f), Some(t)) => f.escalates_to.contains(&t.seat),
        _ => false,
    })
}

/// The one call site a [`DeliveryHook`] reaches: right after `message` is
/// sent, try to deliver it into `to`'s turn if one is already running.
/// Acknowledges (marks delivered) what the hook accepts; anything it does
/// not accept, or no hook being set, leaves the message pending for
/// [`begin_submit`]. Errors from the hook or the store are not
/// fatal to sending the message, so they are swallowed here. For
/// [`SteerDelivery`] the engine has already marked it, so this is a no-op.
pub(crate) fn try_deliver_now(yard: &Yard, store: &Store, to: &str, message: &Message) {
    let Some(hook) = yard.delivery_hook() else {
        return;
    };
    if hook.try_deliver(yard, to, message) {
        branchyard_support::best_effort(
            "store.backend.mark_delivered",
            store.backend().mark_delivered(&[message.id]),
        );
    }
}

/// Every message pending delivery to `branch`, oldest first.
pub(crate) fn pending(store: &Store, branch: &str) -> Result<Vec<Message>, Error> {
    Ok(store
        .backend()
        .inbox(branch)?
        .into_iter()
        .filter(|m| !m.delivered)
        .collect())
}

/// Whether `message` is carried by steered input still queued for engine
/// call `turn` of `branch`: that input delivers it, so the turn's start
/// must not.
fn steering(store: &Store, branch: &str, turn: u64, message: &Message) -> Result<bool, Error> {
    let Some(steer) = store.backend().message_steer(message.id)? else {
        return Ok(false);
    };
    Ok(store
        .backend()
        .steer(branch, steer)?
        .is_some_and(|row| row.turn == turn && row.state == SteerState::Pending))
}

/// `prompt` with as many of `waiting` prepended as fit within the render
/// bounds, oldest first, and the ids actually included, to acknowledge.
/// Messages left out stay pending for a later turn, noted as still queued.
pub(crate) fn combine(prompt: &str, waiting: &[Message]) -> (String, Vec<u64>) {
    if waiting.is_empty() {
        return (prompt.to_owned(), Vec::new());
    }
    let mut body = String::new();
    let mut included = Vec::new();
    let mut chars = 0usize;
    for message in waiting.iter().take(RENDER_MAX) {
        let line = line(message);
        if !included.is_empty() && chars + line.chars().count() > RENDER_CHARS {
            break;
        }
        chars += line.chars().count();
        body.push_str(&line);
        included.push(message.id);
    }
    let left = waiting.len() - included.len();
    let mut block = format!(
        "{OPEN_TAG}\nMessages that arrived since your last turn. Reply to a question with \
         `by answer <id> \"...\"`.\n\n{body}"
    );
    if left > 0 {
        block.push_str(&format!(
            "...and {left} more message{} queued for a later turn.\n",
            if left == 1 { "" } else { "s" }
        ));
    }
    block.push_str(CLOSE_TAG);
    (format!("{block}\n\n{prompt}"), included)
}

/// `branch`'s pending messages combined into the prompt the engine call
/// `turn` is about to submit, and the ids of those it includes. Messages
/// steered input queued for this same turn carries are left to it. Marks
/// nothing: [`begin_submit`] acknowledges them with the submit's intent.
pub(crate) fn compose_turn_start(
    store: &Store,
    branch: &str,
    turn: u64,
    prompt: &str,
) -> Result<(String, Vec<u64>), Error> {
    let mut waiting = Vec::new();
    for message in pending(store, branch)? {
        if !steering(store, branch, turn, &message)? {
            waiting.push(message);
        }
    }
    Ok(combine(prompt, &waiting))
}

/// A turn's prompt as submitted: the task with its pending messages
/// prepended, and the ids of the messages it carries.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Submission {
    pub prompt: String,
    pub delivered: Vec<u64>,
}

/// Deliver `branch`'s pending messages into the fenced turn's prompt:
/// journal its `submit` step, whose intent holds the combined prompt and
/// the message ids, and acknowledge (mark delivered) exactly those messages
/// in the same store transaction. Any failure before that commit leaves
/// them pending; once it commits they count delivered, as the prompt counts
/// submitted, so recovery never gives either again.
pub(crate) fn begin_submit(
    store: &Store,
    fence: &crate::state::Fence,
    branch: &str,
    prompt: &str,
) -> Result<Submission, Error> {
    let (prompt, delivered) = compose_turn_start(store, branch, fence.turn, prompt)?;
    let intent = serde_json::json!({ "prompt": prompt, "messages": delivered });
    let begun = store.backend().begin_step_delivering(
        fence,
        fence.turn,
        crate::engine::STEP_SUBMIT,
        &intent,
        &delivered,
    )?;
    if !matches!(begun, crate::state::Begun::Fresh) {
        return Err(Error::State(format!(
            "{branch}'s turn {} already journaled its submit; it is never submitted twice",
            fence.turn
        )));
    }
    Ok(Submission { prompt, delivered })
}

/// Undo [`begin_submit`] when the harness refused the prompt with nothing
/// written: forget the `submit` step and return its messages to pending, in
/// one transaction.
pub(crate) fn abandon_submit(
    store: &Store,
    fence: &crate::state::Fence,
    submission: &Submission,
) -> Result<(), Error> {
    store.backend().abandon_step_delivering(
        fence,
        fence.turn,
        crate::engine::STEP_SUBMIT,
        &submission.delivered,
    )
}

/// Block up to `wait` for an answer to question `id`, recording the wait
/// durably while it lasts so the asking branch's turn is not taken for
/// stalled ([`waiting_for_answer`]) in any process watching it.
pub(crate) fn wait_for_answer(
    store: &Store,
    id: u64,
    wait: Duration,
) -> Result<Option<Message>, Error> {
    let until = branchyard_support::time::now_ms()
        .saturating_add(u64::try_from(wait.as_millis()).unwrap_or(u64::MAX));
    store.backend().set_awaiting(id, Some(until))?;
    let answer = store.wait(wait, || store.backend().answer_to(id));
    // A waiter that dies before this still stops counting at its deadline.
    branchyard_support::best_effort(
        "store.backend.set_awaiting",
        store.backend().set_awaiting(id, None),
    );
    answer
}

/// Whether `branch` is blocked waiting for an answer to one of its
/// questions: a stall exclusion, like a running child.
pub(crate) fn waiting_for_answer(store: &Store, branch: &str) -> bool {
    store
        .backend()
        .awaiting_answer(branch, branchyard_support::time::now_ms())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Acquired, Fence};

    struct Temp(std::path::PathBuf);
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A store with branch `parent` running a turn, and one message pending
    /// for it.
    fn running(name: &str) -> (Temp, Store, Fence, u64) {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "branchyard-inbox-unit-{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let store = Store::open(&dir).unwrap();
        let record: crate::state::Record = serde_json::from_value(serde_json::json!({
            "info": {
                "name": "parent", "git_branch": "by/parent", "worktree": "/w",
                "prompt": "p", "harness": "h", "profile": "p", "session": null,
                "parent": null, "base": "b", "candidate": null,
                "status": {"state": "running"}, "turns": 0, "cost_usd": null,
                "created_at": 0
            },
            "created_ms": 0, "check": null, "command": null, "home": null,
            "cost_baseline": null
        }))
        .unwrap();
        assert!(store.reserve("parent").unwrap());
        let fence = match store
            .backend()
            .acquire(&record, store.owner(), Duration::from_secs(30))
            .unwrap()
        {
            Acquired::Granted(fence) => fence,
            Acquired::Held(row) => panic!("held by {row:?}"),
        };
        let sent = store
            .backend()
            .send_message(&message(0, MessageKind::Question, "rename it?"))
            .unwrap();
        (Temp(dir), store, fence, sent.id)
    }

    fn delivered(store: &Store, id: u64) -> bool {
        store.backend().message(id).unwrap().unwrap().delivered
    }

    /// Review finding: messages were marked delivered before the `submit`
    /// step was journaled, so a failure in between lost them.
    #[test]
    fn a_failure_before_the_submit_is_journaled_leaves_messages_pending() {
        let (_t, store, fence, id) = running("fail");
        // The turn loses its lease: journaling the submit is refused.
        store.backend().finish(&fence, None, None).unwrap();
        assert!(matches!(
            begin_submit(&store, &fence, "parent", "go"),
            Err(Error::Fenced(_))
        ));
        assert!(!delivered(&store, id), "the message was lost");
        assert!(store
            .backend()
            .steps("parent", fence.turn)
            .unwrap()
            .is_empty());
    }

    /// A crash right after the submit was journaled: the step names the
    /// prompt and its messages, and they count delivered, so recovery
    /// neither submits the prompt again nor gives them again.
    #[test]
    fn a_journaled_submit_counts_its_messages_delivered() {
        let (_t, store, fence, id) = running("journaled");
        let submission = begin_submit(&store, &fence, "parent", "go").unwrap();
        assert_eq!(submission.delivered, [id]);
        assert!(submission.prompt.ends_with("go"));
        let steps = store.backend().steps("parent", fence.turn).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].step, crate::engine::STEP_SUBMIT);
        assert_eq!(steps[0].intent["prompt"], submission.prompt.as_str());
        assert_eq!(steps[0].intent["messages"], serde_json::json!([id]));
        assert!(delivered(&store, id));
        // The next turn start has nothing left to give.
        let (_, again) = compose_turn_start(&store, "parent", fence.turn, "next").unwrap();
        assert!(again.is_empty());
    }

    /// A prompt the harness refused with nothing written: the step is
    /// forgotten and its messages are pending again, but a message another
    /// path delivered meanwhile is not.
    #[test]
    fn an_abandoned_submit_returns_its_messages_to_pending() {
        let (_t, store, fence, id) = running("abandoned");
        let submission = begin_submit(&store, &fence, "parent", "go").unwrap();
        assert!(delivered(&store, id));
        abandon_submit(&store, &fence, &submission).unwrap();
        assert!(!delivered(&store, id));
        assert!(store
            .backend()
            .steps("parent", fence.turn)
            .unwrap()
            .is_empty());
        let (_, again) = compose_turn_start(&store, "parent", fence.turn, "go").unwrap();
        assert_eq!(again, [id]);
    }

    fn message(id: u64, kind: MessageKind, text: &str) -> Message {
        Message {
            id,
            from: "kid".into(),
            to: "parent".into(),
            kind,
            text: text.into(),
            in_reply_to: None,
            at_ms: 0,
            delivered: false,
        }
    }

    #[test]
    fn every_pending_message_fits_when_small() {
        let waiting = vec![
            message(1, MessageKind::Question, "should I rename the module?"),
            message(2, MessageKind::Report, "tests pass"),
        ];
        let (combined, included) = combine("do the task", &waiting);
        assert_eq!(included, [1, 2]);
        assert!(combined.starts_with(OPEN_TAG));
        assert!(combined.ends_with("do the task"));
        assert!(combined.contains("[#1] question from kid"));
        assert!(combined.contains("[#2] report from kid"));
        assert!(!combined.contains("more message"));
    }

    #[test]
    fn a_large_backlog_is_bounded_with_a_note() {
        let waiting: Vec<Message> = (1..=(RENDER_MAX as u64 + 5))
            .map(|id| message(id, MessageKind::Report, "update"))
            .collect();
        let (combined, included) = combine("go", &waiting);
        assert_eq!(included.len(), RENDER_MAX);
        assert!(combined.contains("...and 5 more messages queued"));
    }

    #[test]
    fn no_pending_messages_leaves_the_prompt_untouched() {
        let (combined, included) = combine("go", &[]);
        assert_eq!(combined, "go");
        assert!(included.is_empty());
    }
}
