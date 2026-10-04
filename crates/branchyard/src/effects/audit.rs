//! The gateway's audit log as a second source for the ledger.
//!
//! A call that went through a turn's proxy carries its entry's id as its
//! idempotency key, which Anvil's audit line records as `ledger_id`; when
//! its answer was lost (`unknown`), the line with that id settles it. A
//! line with `staged_for` performed a draft, not the effect, and settles
//! nothing. A call that did not go through the proxy (a
//! sandboxed turn without a proxy address, a harness that found the
//! gateway another way) is recorded from its audit line when the line
//! says the operation's effect class: after the fact, never approved,
//! and said so. A line that says neither is only a `connector_call` event.

use serde_json::Value;

use super::ask::note;
use super::mcp::EffectMeta;
use super::proxy::{described, task_of};
use super::{request_digest, EffectActivity, EffectClass, EffectEntry, EffectMove, EffectState};
use crate::Yard;

/// A ULID whose random part comes from `seed`: the same line always makes
/// the same id, so a line read twice is recorded once.
fn ulid_from(ms: u64, seed: &[u8]) -> String {
    let hash = blake3::hash(seed);
    let mut random = [0u8; 10];
    random.copy_from_slice(&hash.as_bytes()[..10]);
    branchyard_support::ulid_from_parts(ms, random)
}

fn text<'a>(line: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| line.get(*k).and_then(Value::as_str))
}

/// Feed one audit line of `branch`'s, read at `at_ms`, to the ledger.
pub(crate) fn observe(yard: &Yard, branch: &str, at_ms: u64, line: &Value) {
    let ledger = yard.store();
    let decision = text(line, &["decision"]).unwrap_or("");
    let status = line
        .get("upstream_status")
        .or_else(|| line.get("status"))
        .and_then(Value::as_i64);
    if line.get("dry_run") == Some(&Value::Bool(true))
        || line.get("staged_for").is_some_and(|s| !s.is_null())
    {
        return;
    }
    let errored = line.get("error_code").is_some_and(|c| !c.is_null());
    let succeeded =
        decision == "allowed" && !errored && status.is_none_or(|s| (200..300).contains(&s));
    let key = text(line, &["ledger_id", "idempotency_key"]).or_else(|| {
        line.get("_meta")
            .and_then(|m| m.get("idempotency_key"))
            .and_then(Value::as_str)
    });
    if let Some(entry) = key.and_then(|k| ledger.effects().effect(k).ok().flatten()) {
        if entry.state != EffectState::Unknown {
            return;
        }
        let change = match succeeded {
            true => EffectMove::to(EffectState::Confirmed).detail(format!(
                "the gateway's audit log shows it happened ({})",
                status.map_or("allowed".to_owned(), |s| s.to_string())
            )),
            false => EffectMove::to(EffectState::Failed).detail(format!(
                "the gateway's audit log shows it did not happen ({decision}{}{})",
                text(line, &["error_code"])
                    .map(|c| format!(", {c}"))
                    .unwrap_or_default(),
                status.map(|s| format!(", {s}")).unwrap_or_default()
            )),
        };
        if let Ok(Some(done)) =
            ledger
                .effects()
                .move_effect(&entry.id, &[EffectState::Unknown], &change, at_ms)
        {
            note(yard, &done.branch, EffectActivity::of(&done));
        }
        return;
    }
    // Not through the proxy: only a line that says its effect is recorded.
    let effect = line.get("effect").filter(|e| e.is_object());
    let class = effect
        .and_then(|e| e.get("class"))
        .or_else(|| line.get("effect_class"))
        .and_then(Value::as_str)
        .and_then(|c| EffectClass::parse(c).ok());
    let Some(class) = class else {
        return;
    };
    if class == EffectClass::Read || decision != "allowed" {
        return;
    }
    let Ok(record) = ledger.read(branch) else {
        return;
    };
    let connector = text(line, &["connector", "bundle"])
        .unwrap_or("")
        .to_owned();
    let operation = text(line, &["operation", "operation_id", "tool"])
        .unwrap_or("")
        .to_owned();
    let id = ulid_from(at_ms, line.to_string().as_bytes());
    let turn = text(line, &["by_turn"])
        .and_then(|t| t.parse().ok())
        .or_else(|| {
            line.get("by_turn")
                .and_then(Value::as_u64)
                .map(|t| t as u32)
        })
        .unwrap_or(0);
    let entry = EffectEntry {
        id: id.clone(),
        task: task_of(yard, &record),
        branch: branch.to_owned(),
        turn,
        subject: text(line, &["sub"]).unwrap_or("").to_owned(),
        connector: connector.clone(),
        operation: operation.clone(),
        operation_id: text(line, &["operation"]).map(str::to_owned),
        account: text(line, &["account"]).map(str::to_owned),
        class,
        state: EffectState::Begun,
        request_digest: text(line, &["input_sha256", "input_hash"]).map_or_else(
            || request_digest(&connector, &operation, &Value::Null),
            str::to_owned,
        ),
        undo: None,
        compensate: None,
        undo_unavailable: None,
        approval: None,
        decided: None,
        undo_approval: None,
        deletion: super::is_deletion_name(&operation),
        staged: None,
        lookup: None,
        declared: false,
        detail: None,
        upstream_key: None,
        created_ms: at_ms,
        updated_ms: at_ms,
    };
    let meta = effect
        .and_then(|e| EffectMeta::read(Some(&serde_json::json!({"_meta": {"effect": e}})), None));
    let state = match succeeded {
        true => EffectState::Confirmed,
        false => EffectState::Unknown,
    };
    let mut change = match &meta {
        Some(meta) => described(meta, state),
        None => EffectMove::to(state),
    };
    change.detail = Some(
        "recorded from the gateway's audit log: the call did not go through the ledger's proxy, \
         so it was neither approved nor written before it was made"
            .into(),
    );
    let mut entry = entry;
    change.apply(&mut entry, at_ms);
    // A line read twice makes the same id, and is recorded once.
    if ledger.effects().open_effect(&entry).is_ok() {
        note(yard, branch, EffectActivity::of(&entry));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_always_makes_the_same_id() {
        let a = ulid_from(1_000, b"line");
        assert_eq!(a, ulid_from(1_000, b"line"));
        assert_ne!(a, ulid_from(1_000, b"other line"));
        assert_eq!(a.len(), 26);
    }
}
