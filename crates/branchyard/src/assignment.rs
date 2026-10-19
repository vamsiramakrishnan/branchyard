//! What a delegating parent assigned a child, given to the child in its
//! instructions every turn: the paths it owns ([`crate::scope`]).
//!
//! It is read afresh for each turn, so what a parent changes reaches the
//! child's next turn without a message.

use crate::state::Record;
use crate::Yard;

/// The text `record`'s next turn is given, or `None` when its parent
/// assigned it nothing beyond its prompt. Read failures leave a part out:
/// the turn still runs, as it would without one.
pub(crate) fn for_turn(yard: &Yard, record: &Record) -> Option<String> {
    let store = yard.store();
    let mut parts: Vec<String> = Vec::new();
    if let Some(scope) = crate::scope::chain(&store, record)
        .ok()
        .and_then(|chain| crate::scope::assignment(&chain))
    {
        parts.push(scope);
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}
