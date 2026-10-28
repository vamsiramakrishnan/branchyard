//! Frozen cross-harness results delivered without interrupting native work.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state::Record;
use crate::storage::{ArtifactRow, Grants, Lineage, Share};
use crate::{ArtifactRef, BranchStatus, Error, Message, MessageKind, Yard};

/// One producer incarnation and its accepted current-turn result event.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceVersion {
    /// Never-reused producer identity in this repository.
    pub incarnation: i64,
    /// Producer turn containing the result.
    pub turn: u64,
    /// Per-branch event sequence of the accepted result.
    pub result_event: u64,
}

/// Relay one settled producer's result and selected artifacts to a coworker.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffRequest {
    /// Producer branch whose current-turn result is frozen.
    pub from: String,
    /// Existing recipient branch; handing off never starts another turn.
    pub to: String,
    /// Artifact IDs to share explicitly with this recipient.
    #[serde(default)]
    pub artifacts: Vec<String>,
    /// Context from the relaying lead, kept beside the frozen result.
    #[serde(default)]
    pub note: String,
    /// Refuse a producer version that differs from this reviewed version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<SourceVersion>,
}

/// An immutable result snapshot carried by one durable inbox message.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Handoff {
    /// Producer branch name, for display; version binds its incarnation.
    pub source: String,
    /// Producer identity, turn and accepted result event.
    pub version: SourceVersion,
    /// Accepted result JSON, copied rather than read again at delivery.
    pub result: Value,
    /// Candidate commit at handoff, when the producer has code changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
    /// Store-provided metadata for the explicitly selected artifacts.
    pub artifacts: Vec<ArtifactRef>,
}

/// Checked inputs revalidated and committed together by either database.
#[derive(Clone, Debug)]
pub(crate) struct HandoffCommit {
    /// A delegated lead's name and incarnation; None is operator authority.
    pub actor: Option<(String, i64)>,
    /// Recipient incarnation, fencing removal and reuse of its name.
    pub recipient_incarnation: i64,
    /// Source event count observed alongside the accepted result.
    pub source_event_count: u64,
    /// Stable hash of the actor, endpoints, frozen result, artifacts and note.
    pub dedupe_key: String,
    /// The message to insert atomically with its artifact grants.
    pub message: Message,
}

impl HandoffCommit {
    /// Recheck every mutable fact under the database transaction.
    pub(crate) fn validate(
        &self,
        source: &Record,
        event_count: u64,
        lineage: &Lineage,
        artifacts: &[(ArtifactRow, Vec<Share>)],
    ) -> Result<(), Error> {
        let snapshot = self.message.handoff.as_ref().ok_or_else(|| {
            Error::State("a handoff transaction requires its frozen payload".into())
        })?;
        let version = &snapshot.version;
        let sender = self
            .actor
            .as_ref()
            .map_or(snapshot.source.as_str(), |(name, _)| name.as_str());
        if !self.message.next_turn
            || self.message.from != sender
            || self.message.to == snapshot.source
        {
            return Err(Error::Denied(
                "a handoff must retain its authorized sender and next-turn delivery".into(),
            ));
        }
        if version.result_event == 0 || version.result_event > event_count {
            return Err(Error::Denied(
                "a handoff must identify an accepted source result event".into(),
            ));
        }
        if lineage.id(&snapshot.source)? != version.incarnation
            || lineage.id(&self.message.to)? != self.recipient_incarnation
        {
            return Err(Error::Denied(
                "a handoff endpoint was removed or replaced".into(),
            ));
        }
        check_actor(
            lineage,
            self.actor.as_ref(),
            version.incarnation,
            self.recipient_incarnation,
        )?;
        if source.info.name != snapshot.source
            || u64::from(source.info.turns) != version.turn
            || event_count != self.source_event_count
            || source.info.candidate.as_ref().map(|c| &c.commit) != snapshot.candidate.as_ref()
            || !finished(&source.info.status)
        {
            return Err(Error::Denied(
                "the handoff source changed; inspect its current result and retry".into(),
            ));
        }
        if let Some(contract) = &source.result_contract {
            let violations = contract.violations(&snapshot.result)?;
            if !violations.is_empty() {
                return Err(Error::Denied(format!(
                    "the source result does not match its contract: {}",
                    violations.join("; ")
                )));
            }
        }
        if artifacts.len() != snapshot.artifacts.len() {
            return Err(Error::Denied(
                "a selected handoff artifact no longer exists".into(),
            ));
        }
        for frozen in &snapshot.artifacts {
            let (stored, shares) = artifacts
                .iter()
                .find(|(row, _)| row.artifact.id == frozen.id)
                .ok_or_else(|| {
                    Error::Denied(format!("handoff artifact {} no longer exists", frozen.id))
                })?;
            if stored.artifact != *frozen {
                return Err(Error::Denied(format!(
                    "handoff artifact {} changed",
                    frozen.id
                )));
            }
            let grants = Grants::of_artifact(stored, shares);
            if !grants.readable(lineage, version.incarnation)
                || self
                    .actor
                    .as_ref()
                    .is_some_and(|(_, id)| !grants.readable(lineage, *id))
            {
                return Err(Error::Denied(format!(
                    "handoff artifact {} is not readable by its source and lead",
                    frozen.id
                )));
            }
        }
        Ok(())
    }
}

fn check_actor(
    lineage: &Lineage,
    actor: Option<&(String, i64)>,
    source: i64,
    recipient: i64,
) -> Result<(), Error> {
    if let Some((name, incarnation)) = actor {
        if lineage.id(name)? != *incarnation
            || *incarnation == source
            || *incarnation == recipient
            || !lineage.descends(*incarnation, source)
            || !lineage.descends(*incarnation, recipient)
        {
            return Err(Error::Denied(
                "only a common ancestor may relay a handoff between its descendants".into(),
            ));
        }
    }
    Ok(())
}

fn finished(status: &BranchStatus) -> bool {
    matches!(
        status,
        BranchStatus::Ready | BranchStatus::NoChanges | BranchStatus::Merged { .. }
    )
}

/// Freeze, authorize and enqueue one handoff; never steer or run a turn.
pub(crate) fn relay(
    yard: &Yard,
    actor: Option<&str>,
    request: HandoffRequest,
) -> Result<Message, Error> {
    let store = yard.store();
    let source = store.read(&request.from)?;
    store.read(&request.to)?;
    if request.from == request.to {
        return Err(Error::Denied(
            "a handoff needs distinct source and recipient branches".into(),
        ));
    }
    let lineage = Lineage::load(&store)?;
    let actor = actor
        .map(|name| -> Result<_, Error> { Ok((name.to_owned(), lineage.id(name)?)) })
        .transpose()?;
    let recipient_incarnation = lineage.id(&request.to)?;
    check_actor(
        &lineage,
        actor.as_ref(),
        lineage.id(&request.from)?,
        recipient_incarnation,
    )?;
    if !finished(&source.info.status) {
        return Err(Error::Denied(
            "a handoff source must have finished its current turn".into(),
        ));
    }
    if request.note.len() > 16 * 1024 || request.artifacts.len() > 64 {
        return Err(Error::Denied(
            "a handoff accepts at most 64 artifacts and a 16 KiB note".into(),
        ));
    }
    let events = crate::record::read(&store, &request.from)?;
    let (result_event, result) = crate::contract::current_event(&events).ok_or_else(|| {
        Error::Denied("the source has no accepted result in its current turn".into())
    })?;
    if let Some(contract) = &source.result_contract {
        let violations = contract.violations(result)?;
        if !violations.is_empty() {
            return Err(Error::Denied(format!(
                "the source result does not match its contract: {}",
                violations.join("; ")
            )));
        }
    }
    let version = SourceVersion {
        incarnation: lineage.id(&request.from)?,
        turn: u64::from(source.info.turns),
        result_event,
    };
    if request
        .expected
        .as_ref()
        .is_some_and(|expected| *expected != version)
    {
        return Err(Error::Denied(
            "the handoff source differs from the expected version".into(),
        ));
    }
    let mut ids = request.artifacts;
    ids.sort();
    ids.dedup();
    let mut selected = Vec::new();
    for id in ids {
        let row = crate::storage::readable_artifact(&store, &request.from, &id)?;
        selected.push((row, store.storage().artifact_shares(&id)?));
    }
    let snapshot = Handoff {
        source: request.from.clone(),
        version,
        result: result.clone(),
        candidate: source
            .info
            .candidate
            .as_ref()
            .map(|candidate| candidate.commit.clone()),
        artifacts: selected
            .iter()
            .map(|(row, _)| row.artifact.clone())
            .collect(),
    };
    let message = Message {
        id: 0,
        from: actor
            .as_ref()
            .map_or(request.from, |(name, _)| name.clone()),
        to: request.to,
        kind: MessageKind::Answer,
        text: request.note,
        in_reply_to: None,
        at_ms: 0,
        delivered: false,
        handoff: Some(snapshot),
        next_turn: true,
    };
    let key_bytes = serde_json::to_vec(&(&actor, recipient_incarnation, &message))
        .map_err(|e| Error::State(format!("cannot encode a handoff: {e}")))?;
    let prepared = HandoffCommit {
        actor,
        recipient_incarnation,
        source_event_count: events.len() as u64,
        dedupe_key: blake3::hash(&key_bytes).to_hex().to_string(),
        message,
    };
    prepared.validate(&source, events.len() as u64, &lineage, &selected)?;
    let message = store.backend().commit_handoff(&prepared)?;
    store.notify();
    Ok(message)
}
