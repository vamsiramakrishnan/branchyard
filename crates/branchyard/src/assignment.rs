//! What a delegating parent assigned a child, given to the child in its
//! instructions every turn: the paths it owns ([`crate::scope`]) and the
//! brief the parent shares with all its children (`Activity::Brief`
//! entries on the parent's log).
//!
//! It is read afresh for each turn, so what a parent adds reaches the
//! child's next turn without a message; a running turn is sent a new brief
//! entry by steering, where its harness takes it.

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
    if let Some(brief) = record
        .info
        .parent
        .as_deref()
        .and_then(|parent| crate::delegation::brief_entries(&store, parent).ok())
        .and_then(|entries| shared_brief(&entries))
    {
        parts.push(brief);
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// The block that carries a parent's brief `entries`; `None` without one.
pub(crate) fn shared_brief(entries: &[String]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    Some(format!(
        "## The shared brief\n\
         Your parent gives this to every child it delegates to, you included; what it adds \
         later appears here too. Follow it unless your task says otherwise.\n\n{}",
        entries.join("\n\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_brief_block_carries_every_entry_in_order() {
        assert_eq!(shared_brief(&[]), None);
        let block = shared_brief(&["Use the Parser trait.".into(), "No new crates.".into()]);
        let block = block.unwrap();
        assert!(block.starts_with("## The shared brief\n"), "{block}");
        let (first, second) = (
            block.find("Use the Parser trait.").unwrap(),
            block.find("No new crates.").unwrap(),
        );
        assert!(first < second, "{block}");
    }
}
