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
//! message twice and never silently drops one. Without a running turn to
//! reach, a branch's pending messages are given to it at the start of its
//! next turn, prepended to the prompt as one delimited, size-bounded block;
//! whatever does not fit stays pending, with a note of how many messages are
//! still queued. A [`DeliveryHook`], set with [`crate::Yard::set_delivery_hook`],
//! lets a caller that keeps a branch's turn open — such as a steer channel —
//! try to deliver a message into that running turn right away; whatever it
//! does not accept is left pending for the turn-boundary delivery above.
//! Not implemented today: without a hook, every message waits for the
//! recipient's next turn to start.

use crate::state::Store;
use crate::{Error, Message, MessageKind, Yard};

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
/// running, such as through a steer channel. Wired in with
/// [`crate::Yard::set_delivery_hook`]; `None` means every message waits for
/// the branch's next turn to start, and turn-boundary delivery
/// ([`deliver_at_turn_start`]) is the only path.
pub trait DeliveryHook: Send + Sync {
    /// Try to deliver `message` to `branch`'s current turn right now.
    /// `true` acknowledges it: it will not be delivered again, at a turn
    /// boundary or otherwise. `false` leaves it pending.
    fn try_deliver(&self, branch: &str, message: &Message) -> bool;
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
/// [`deliver_at_turn_start`]. Errors from the hook or the store are not
/// fatal to sending the message, so they are swallowed here.
pub(crate) fn try_deliver_now(yard: &Yard, store: &Store, to: &str, message: &Message) {
    let Some(hook) = yard.delivery_hook() else {
        return;
    };
    if hook.try_deliver(to, message) {
        let _ = store.backend().mark_delivered(&[message.id]);
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
        let line = format!(
            "[#{}] {} from {}: {}\n",
            message.id, message.kind, message.from, message.text
        );
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

/// Deliver `branch`'s pending messages into the text it is about to submit:
/// acknowledge (mark delivered) exactly the ones included in the combined
/// prompt, so a crash before submission leaves them pending and one after
/// never delivers them again.
pub(crate) fn deliver_at_turn_start(
    store: &Store,
    branch: &str,
    prompt: &str,
) -> Result<String, Error> {
    let waiting = pending(store, branch)?;
    let (combined, included) = combine(prompt, &waiting);
    if !included.is_empty() {
        store.backend().mark_delivered(&included)?;
    }
    Ok(combined)
}

#[cfg(test)]
mod tests {
    use super::*;

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
