//! Per-turn checkpoints, [`crate::Branch::rewind`] and
//! [`crate::Branch::fork_at`].
//!
//! Every turn that submitted its prompt ends with a snapshot, a commit on
//! `by/<name>` holding everything in the worktree. After it, the engine
//! points `refs/branchyard/<name>/<incarnation>/turn-<N>` at the branch's
//! head, as the journaled step `checkpoint`, and records an
//! [`Activity::Checkpoint`]. Nothing is copied: the ref names a commit the
//! branch already has. Turn 0 is the branch's base and has no ref.
//!
//! Turn numbers only grow. A rewind resets the branch and its worktree to
//! checkpoint N and leaves every checkpoint in place, so the next turn is
//! numbered after the highest one and a rewind to a later checkpoint undoes
//! it. Refs are deleted with the branch.
//!
//! A harness's session cannot be cut back to an earlier turn. A rewind
//! resumes the harness's own session only when that session ended exactly
//! at the checkpoint (no later turn continued it); a fork at a checkpoint
//! forks it natively under the same condition, when the harness can fork.
//! Otherwise the next turn starts a fresh session whose prompt begins with a
//! generated summary of the turns that led to the checkpoint, and the
//! [`SessionContinuity`] recorded says which, and why.
//!
//! A rewind is journaled under the branch's lease, intent before effect: the
//! intent names the commit, the session decision and the summary, so an
//! engine that stops mid-rewind leaves a step that recovery finishes the
//! same way (see [`recover`]).

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::json;

use branchyard_harness::profiles;

use crate::record::{self, Recorder};
use crate::state::{Begun, Fence, Lease, Record};
use crate::{
    git, Activity, BranchStatus, CandidateInfo, Checkpoint, Error, Event, RecordedEvent, Recovery,
    SessionContinuity, Yard,
};

/// The journaled step that writes a turn's checkpoint ref.
pub(crate) const STEP_CHECKPOINT: &str = "checkpoint";
/// The journaled step of a rewind, keyed by the lease it holds.
pub(crate) const STEP_REWIND: &str = "rewind";

const ROOT: &str = "refs/branchyard/";
/// Ends a generated summary; what follows is the turn's own prompt.
const TASK_HEADING: &str = "\n\n## Your task now\n";
/// Longest prompt and reply quoted per turn in a summary, in characters.
const QUOTE_MAX: usize = 1500;

/// `refs/branchyard/<name>/<incarnation>/turn-<turn>`.
pub(crate) fn ref_name(name: &str, incarnation: i64, turn: u32) -> String {
    format!("{ROOT}{name}/{incarnation}/turn-{turn}")
}

/// Abort the process at `point` when `BRANCHYARD_FAULT` names it: a crash
/// the durability tests inject between an intent and its effect. Never set
/// it outside a test.
pub(crate) fn fault(point: &str) {
    if std::env::var("BRANCHYARD_FAULT").is_ok_and(|v| v == point) {
        std::process::abort();
    }
}

/// `1-3, 5` for `[1, 2, 3, 5]`.
pub(crate) fn turn_list(turns: &[u32]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < turns.len() {
        let start = turns[i];
        let mut end = start;
        while i + 1 < turns.len() && turns[i + 1] == end + 1 {
            i += 1;
            end = turns[i];
        }
        parts.push(match start == end {
            true => start.to_string(),
            false => format!("{start}-{end}"),
        });
        i += 1;
    }
    parts.join(", ")
}

/// The checkpoints recorded in a branch's events, by turn; a turn recorded
/// twice (a replayed step) keeps its last record.
pub fn recorded(events: &[RecordedEvent]) -> Vec<Checkpoint> {
    let mut by_turn = BTreeMap::new();
    for event in events {
        if let Activity::Checkpoint(checkpoint) = &event.activity {
            by_turn.insert(checkpoint.turn, checkpoint.clone());
        }
    }
    by_turn.into_values().collect()
}

/// The checkpoints recorded in a branch's events, each with its turn's
/// prompt; `available` is assumed, since events cannot say whether the ref
/// still exists ([`crate::Branch::checkpoints`] checks it).
pub fn entries(events: &[RecordedEvent]) -> Vec<CheckpointEntry> {
    let texts = turn_texts(events);
    recorded(events)
        .into_iter()
        .map(|checkpoint| CheckpointEntry {
            prompt: texts.get(&checkpoint.turn).map(|t| t.prompt.clone()),
            checkpoint,
            available: true,
        })
        .collect()
}

/// A branch's checkpoints, from [`crate::Branch::checkpoints`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Checkpoints {
    pub branch: String,
    /// The branch's base: checkpoint 0.
    pub base: String,
    /// The checkpoint the branch is at; `None` when it is not at one, such
    /// as after a failed snapshot or for a branch created before
    /// checkpoints were recorded.
    pub current: Option<u32>,
    pub checkpoints: Vec<CheckpointEntry>,
}

/// One checkpoint, with the prompt of its turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CheckpointEntry {
    #[serde(flatten)]
    pub checkpoint: Checkpoint,
    /// The turn's prompt, without a generated summary it began with.
    pub prompt: Option<String>,
    /// Whether the ref still names the commit, so the checkpoint can be
    /// rewound or forked to.
    pub available: bool,
}

/// What [`crate::Branch::rewind`] did.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rewound {
    pub branch: String,
    pub from: Option<u32>,
    pub to: u32,
    pub commit: String,
    pub session: SessionContinuity,
}

pub(crate) fn list(yard: &Yard, name: &str) -> Result<Checkpoints, Error> {
    let store = yard.store();
    let record = store.read(name)?;
    let events = record::read(&store, name)?;
    let checkpoints = entries(&events)
        .into_iter()
        .map(|mut entry| {
            entry.available = git::commit(&yard.root, &entry.checkpoint.git_ref)?.as_deref()
                == Some(entry.checkpoint.commit.as_str());
            Ok(entry)
        })
        .collect::<Result<Vec<_>, Error>>()?;
    Ok(Checkpoints {
        branch: name.to_owned(),
        base: record.info.base,
        current: record.checkpoint,
        checkpoints,
    })
}

/// Record the checkpoint of the turn `record` just concluded, as the
/// journaled step `checkpoint`. A ref that cannot be written is a warning,
/// never the turn's failure.
pub(crate) fn record_turn(
    yard: &Yard,
    fence: &Fence,
    record: &mut Record,
    recorder: &mut Recorder,
) -> Result<(), Error> {
    let store = yard.store();
    let turn = record.info.turns;
    let intent = json!({ "turn": turn });
    if let Begun::Done(outcome) =
        store
            .backend()
            .begin_step(fence, fence.turn, STEP_CHECKPOINT, &intent)?
    {
        record.checkpoint = serde_json::from_value::<Checkpoint>(outcome)
            .ok()
            .map(|c| c.turn);
        return Ok(());
    }
    let git_ref = ref_name(&record.info.name, fence.incarnation, turn);
    let made = (|| {
        let head = git::commit(
            &yard.root,
            &format!("refs/heads/{}", record.info.git_branch),
        )?
        .ok_or_else(|| Error::Git(format!("{} does not exist", record.info.git_branch)))?;
        git::update_ref(&yard.root, &git_ref, &head)?;
        let (files_changed, insertions, deletions) =
            git::numstat(&yard.root, &record.info.base, &head)?;
        Ok::<_, Error>(Checkpoint {
            turn,
            commit: head,
            git_ref: git_ref.clone(),
            after: record.checkpoint,
            session: record.info.session.clone(),
            files_changed,
            insertions,
            deletions,
            sandbox: None,
        })
    })();
    match made {
        Ok(mut checkpoint) => {
            // The provider snapshot under this checkpoint, when the branch
            // keeps its sandbox and its provider can take one.
            let (snapshot, said) =
                crate::snapshots::snapshot_turn(yard, fence, record, turn, &checkpoint.commit);
            checkpoint.sandbox = snapshot.map(Box::new);
            recorder.record(Activity::Checkpoint(checkpoint.clone()))?;
            for activity in said {
                recorder.record(activity)?;
            }
            // The task's record beside it: the files and the conversation
            // as of this checkpoint (`crate::tasks`).
            if let Err(error) = crate::tasks::record_checkpoint(yard, record, &checkpoint) {
                recorder.record(Activity::Warning(format!(
                    "the task's record of turn {turn} was not written: {error}"
                )))?;
            }
            store.backend().finish_step(
                fence,
                fence.turn,
                STEP_CHECKPOINT,
                &serde_json::to_value(&checkpoint).unwrap_or_default(),
            )?;
            record.checkpoint = Some(turn);
        }
        Err(error) => {
            recorder.record(Activity::Warning(format!(
                "no checkpoint was recorded for turn {turn}: {error}"
            )))?;
            store.backend().finish_step(
                fence,
                fence.turn,
                STEP_CHECKPOINT,
                &json!({ "error": error.to_string() }),
            )?;
            record.checkpoint = None;
        }
    }
    Ok(())
}

/// Delete every checkpoint ref of `name`, and the task record beside each,
/// of any incarnation. Refs of a branch whose name continues `name/...` are
/// not touched.
pub(crate) fn remove_refs(root: &Path, name: &str) -> Result<(), Error> {
    let prefix = format!("{ROOT}{name}/");
    for (full, _) in git::refs(root, &prefix)? {
        let rest = &full[prefix.len()..];
        let ours = rest.split_once('/').is_some_and(|(incarnation, leaf)| {
            incarnation.parse::<i64>().is_ok()
                && leaf
                    .strip_prefix("turn-")
                    .or_else(|| leaf.strip_prefix("record-"))
                    .is_some_and(|n| n.parse::<u32>().is_ok())
        });
        if ours {
            git::delete_ref(root, &full)?;
        }
    }
    Ok(())
}

/// The commit of checkpoint `turn` of a branch whose record is `record`:
/// its base for 0, else the checkpoint's ref, which must still name the
/// commit its event recorded.
pub(crate) fn target_commit(
    yard: &Yard,
    record: &Record,
    list: &[Checkpoint],
    turn: u32,
) -> Result<String, Error> {
    let name = &record.info.name;
    if turn == 0 {
        return Ok(record.info.base.clone());
    }
    let Some(checkpoint) = list.iter().find(|c| c.turn == turn) else {
        let known = list.iter().map(|c| c.turn).collect::<Vec<_>>();
        return Err(Error::State(match known.is_empty() {
            true => format!("{name} has no checkpoint {turn}; it has no checkpoints yet"),
            false => format!(
                "{name} has no checkpoint {turn}; its checkpoints are 0 (its base) and {}",
                turn_list(&known)
            ),
        }));
    };
    match git::commit(&yard.root, &checkpoint.git_ref)? {
        Some(commit) if commit == checkpoint.commit => Ok(commit),
        _ => Err(Error::State(format!(
            "{name}'s checkpoint {turn} ({}) no longer names commit {}",
            checkpoint.git_ref, checkpoint.commit
        ))),
    }
}

/// The turns that led to checkpoint `turn`, oldest first, following each
/// checkpoint's `after` back to the base.
pub(crate) fn lineage(list: &[Checkpoint], turn: u32) -> Vec<u32> {
    let mut line = Vec::new();
    let mut at = Some(turn);
    while let Some(n) = at.filter(|n| *n != 0) {
        if line.contains(&n) {
            break;
        }
        match list.iter().find(|c| c.turn == n) {
            Some(checkpoint) => {
                line.push(n);
                at = checkpoint.after;
            }
            None => break,
        }
    }
    line.reverse();
    line
}

/// Whether the harness's own session can continue from checkpoint `turn`:
/// only when a session was recorded there, the harness `supported` it, and
/// no later turn continued that session. Otherwise a summary of its
/// lineage, and why.
pub(crate) fn continuity(
    list: &[Checkpoint],
    turn: u32,
    supported: Result<(), String>,
) -> SessionContinuity {
    if turn == 0 {
        return SessionContinuity::Fresh {
            reason: "checkpoint 0 is the branch's base; no turn ran before it".into(),
        };
    }
    let summary = |reason: String| SessionContinuity::Summary {
        turns: lineage(list, turn),
        reason,
    };
    let session = list
        .iter()
        .find(|c| c.turn == turn)
        .and_then(|c| c.session.clone());
    let Some(session) = session else {
        return summary(format!("no harness session was recorded at turn {turn}"));
    };
    if let Err(reason) = supported {
        return summary(reason);
    }
    let later: Vec<u32> = list
        .iter()
        .filter(|c| c.turn > turn && c.session.as_deref() == Some(session.as_str()))
        .map(|c| c.turn)
        .collect();
    if !later.is_empty() {
        return summary(format!(
            "its session {session} went on to turn {} and cannot be cut back to turn {turn}",
            turn_list(&later)
        ));
    }
    SessionContinuity::Native { session }
}

/// A turn's prompt and the harness's reply, as its events recorded them.
struct TurnText {
    prompt: String,
    reply: String,
}

fn turn_texts(events: &[RecordedEvent]) -> BTreeMap<u32, TurnText> {
    let mut texts = BTreeMap::new();
    let mut prompt = String::new();
    let mut reply = String::new();
    for event in events {
        match &event.activity {
            Activity::Prompt(text) => {
                prompt = own_prompt(text).to_owned();
                reply.clear();
            }
            Activity::Harness(Event::MessageDelta { text, .. }) => reply.push_str(text),
            Activity::Checkpoint(checkpoint) => {
                texts.insert(
                    checkpoint.turn,
                    TurnText {
                        prompt: prompt.clone(),
                        reply: reply.clone(),
                    },
                );
            }
            _ => {}
        }
    }
    texts
}

/// A prompt without the summary a fresh session's first prompt began with.
fn own_prompt(prompt: &str) -> &str {
    match prompt.rfind(TASK_HEADING) {
        Some(at) => &prompt[at + TASK_HEADING.len()..],
        None => prompt,
    }
}

fn quote(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(QUOTE_MAX) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

/// The summary a fresh session starts with: what each turn of the
/// checkpoint's lineage asked and answered, and what it changed.
pub(crate) fn summary(
    name: &str,
    events: &[RecordedEvent],
    list: &[Checkpoint],
    turn: u32,
    commit: &str,
    session: &SessionContinuity,
) -> Option<String> {
    let SessionContinuity::Summary { turns, reason } = session else {
        return None;
    };
    let texts = turn_texts(events);
    let short = commit.get(..10).unwrap_or(commit);
    let mut out = format!(
        "This continues the work of branch {name} from its checkpoint {turn} (commit {short}). \
         The harness's own session could not continue from there ({reason}), so this is a \
         fresh session. The working tree is exactly as turn {turn} left it. What happened in \
         the turns that led to it:\n"
    );
    if turns.is_empty() {
        out.push_str("\n(No earlier turns were recorded.)\n");
    }
    for n in turns {
        out.push_str(&format!("\n### Turn {n}\n"));
        if let Some(text) = texts.get(n) {
            out.push_str(&format!("Asked: {}\n", quote(&text.prompt)));
            if !text.reply.trim().is_empty() {
                out.push_str(&format!("Replied: {}\n", quote(&text.reply)));
            }
        }
        if let Some(checkpoint) = list.iter().find(|c| c.turn == *n) {
            out.push_str(&format!(
                "Afterwards: {} file(s) changed against the base, +{} -{}\n",
                checkpoint.files_changed, checkpoint.insertions, checkpoint.deletions
            ));
        }
    }
    Some(out)
}

/// A turn's prompt after a summary.
pub(crate) fn compose(context: &str, prompt: &str) -> String {
    format!("{}{TASK_HEADING}{prompt}", context.trim_end())
}

/// A rewind's journaled intent: everything it will do, decided before it
/// does any of it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Intent {
    to: u32,
    commit: String,
    from: Option<u32>,
    /// The branch's head before the rewind.
    head: String,
    session: SessionContinuity,
    context: Option<String>,
}

pub(crate) fn rewind(yard: &Yard, name: &str, to: u32) -> Result<Rewound, Error> {
    let store = yard.store();
    let (record, lease) = crate::ops::hold(yard, name)?;
    let fence = lease.fence().clone();
    match &record.info.status {
        BranchStatus::Running => return Err(Error::Running(name.to_owned())),
        BranchStatus::Waiting | BranchStatus::Blocked { .. } => {
            return Err(Error::Denied(format!(
                "{name} has not started, so it has no checkpoints"
            )))
        }
        BranchStatus::Merged { target, .. } => {
            return Err(Error::Denied(format!(
                "{name} was merged into {target}; rewinding it would leave the merge behind. \
                 Fork it at the checkpoint instead"
            )))
        }
        _ => {}
    }
    let events = record::read(&store, name)?;
    let list = recorded(&events);
    let commit = target_commit(yard, &record, &list, to)?;
    let worktree = record.info.worktree.clone();
    if !worktree.is_dir() {
        return Err(Error::State(format!(
            "{name}'s worktree {} is missing",
            worktree.display()
        )));
    }
    let on = git::run(&worktree, &["symbolic-ref", "--quiet", "HEAD"])
        .map(|r| r.trim() == format!("refs/heads/{}", record.info.git_branch))
        .unwrap_or(false);
    if !on {
        return Err(Error::Denied(format!(
            "{name}'s worktree is not on {}; check it out there before rewinding",
            record.info.git_branch
        )));
    }
    let mut dirty = git::status(&worktree)?;
    // Large files in a task's own repository, which `git status` cannot see.
    for path in crate::tasks::before_reset(yard, &worktree)? {
        dirty.push_str(&format!(" M {path}\n"));
    }
    if !dirty.trim().is_empty() {
        return Err(Error::Denied(format!(
            "{name}'s worktree has changes that are in no checkpoint, which a rewind would \
             discard; commit or remove them, or send a turn so they are checkpointed:\n{}",
            dirty.trim_end()
        )));
    }
    let head = git::commit(&worktree, "HEAD")?.unwrap_or_default();
    let profile = profiles::by_id(&record.info.profile)
        .ok_or_else(|| Error::UnknownHarness(record.info.profile.clone()))?;
    let supported = match profile.driver().capabilities().resume {
        true => Ok(()),
        false => Err(format!("{} cannot resume a session", profile.id)),
    };
    let session = continuity(&list, to, supported);
    let context = summary(name, &events, &list, to, &commit, &session);
    let intent = Intent {
        to,
        commit,
        from: record.checkpoint,
        head,
        session,
        context,
    };
    store.backend().begin_step(
        &fence,
        fence.turn,
        STEP_REWIND,
        &serde_json::to_value(&intent).unwrap_or_default(),
    )?;
    fault("rewind-after-intent");
    finish(yard, lease, record, &intent, None)
}

/// Carry out a journaled rewind and settle the branch. Repeating it is
/// harmless: the reset goes to the same commit.
fn finish(
    yard: &Yard,
    lease: Lease,
    mut record: Record,
    intent: &Intent,
    recovered: Option<String>,
) -> Result<Rewound, Error> {
    let store = yard.store();
    let fence = lease.fence().clone();
    reset(
        yard,
        &record.info.worktree,
        &intent.commit,
        recovered.is_some(),
    )?;
    fault("rewind-after-reset");
    let candidate = match git::same_tree(&yard.root, &record.info.base, &intent.commit)? {
        true => None,
        false => {
            let (files_changed, insertions, deletions) =
                git::numstat(&yard.root, &record.info.base, &intent.commit)?;
            Some(CandidateInfo {
                commit: intent.commit.clone(),
                files_changed,
                insertions,
                deletions,
            })
        }
    };
    record.info.status = match candidate {
        Some(_) => BranchStatus::Ready,
        None => BranchStatus::NoChanges,
    };
    record.info.candidate = candidate;
    record.info.session = match &intent.session {
        SessionContinuity::Native { session } => Some(session.clone()),
        _ => None,
    };
    record.info.stalled = false;
    record.context = intent.context.clone();
    record.checkpoint = Some(intent.to);
    // The kept sandbox holds the state after a later turn: it goes, and
    // the next turn's sandbox comes from checkpoint N's provider snapshot
    // when there is one (`crate::snapshots`).
    let discarded = crate::snapshots::discard_kept(
        yard,
        &record,
        &format!("the branch was rewound to checkpoint {}", intent.to),
    );
    record.sandbox_seed = match intent.to {
        0 => None,
        to => crate::snapshots::seed(&record, Some(to), &intent.commit),
    };
    store.backend().finish_step(
        &fence,
        fence.turn,
        STEP_REWIND,
        &json!({ "commit": intent.commit }),
    )?;
    let mut recorder = Recorder::fenced(&store, &fence, None);
    if let Some(reason) = recovered {
        recorder.record(Activity::Recovered {
            reason,
            killed: Vec::new(),
        })?;
    }
    recorder.record(Activity::Rewound {
        from: intent.from,
        to: intent.to,
        commit: intent.commit.clone(),
        session: intent.session.clone(),
    })?;
    for activity in discarded {
        recorder.record(activity)?;
    }
    recorder.finish(lease, &record)?;
    Ok(Rewound {
        branch: record.info.name,
        from: intent.from,
        to: intent.to,
        commit: intent.commit.clone(),
        session: intent.session.clone(),
    })
}

/// Reset the worktree, its index and its branch to `commit`, and remove
/// untracked files (ignored ones stay). Recovering, a lock a killed git left
/// is removed first: no other git runs there while the lease is held.
fn reset(yard: &Yard, worktree: &Path, commit: &str, recovering: bool) -> Result<(), Error> {
    if recovering {
        if let Ok(lock) = git::run(
            worktree,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "index.lock",
            ],
        ) {
            let _ = std::fs::remove_file(lock.trim_end_matches('\n'));
        }
    }
    // In a task's own repository, large files are pointers in the index,
    // marked so git leaves them alone; the reset may write them now, and
    // they are restored from the chunk store after it.
    crate::tasks::release(yard, worktree)?;
    git::run(worktree, &["reset", "--hard", "--quiet", commit])?;
    git::run(worktree, &["clean", "-f", "-d", "--quiet"])?;
    crate::tasks::after_checkout(yard, worktree)?;
    Ok(())
}

/// Finish a rewind whose engine stopped, from its journaled intent, under
/// the lease recovery took over.
pub(crate) fn recover(
    yard: &Yard,
    lease: Lease,
    record: Record,
    intent: &serde_json::Value,
    why: &str,
) -> Result<Recovery, Error> {
    let intent: Intent = serde_json::from_value(intent.clone())
        .map_err(|e| Error::State(format!("unreadable rewind intent: {e}")))?;
    let reason = format!(
        "{why}; a rewind to checkpoint {} had begun, and recovery finished it",
        intent.to
    );
    let name = record.info.name.clone();
    finish(yard, lease, record, &intent, Some(reason.clone()))?;
    let status = yard.store().read(&name)?.info.status;
    Ok(Recovery {
        branch: name,
        status,
        reason,
        killed: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint(turn: u32, after: Option<u32>, session: Option<&str>) -> Checkpoint {
        Checkpoint {
            turn,
            commit: format!("c{turn}"),
            git_ref: format!("refs/branchyard/b/1/turn-{turn}"),
            after,
            session: session.map(str::to_owned),
            files_changed: 1,
            insertions: 1,
            deletions: 0,
            sandbox: None,
        }
    }

    #[test]
    fn turn_lists_join_runs() {
        assert_eq!(turn_list(&[1, 2, 3, 5, 7, 8]), "1-3, 5, 7-8");
        assert_eq!(turn_list(&[4]), "4");
        assert_eq!(turn_list(&[]), "");
    }

    #[test]
    fn lineage_follows_after_across_rewinds() {
        // 1, 2, 3; rewound to 1; 4 after 1.
        let list = [
            checkpoint(1, Some(0), Some("s")),
            checkpoint(2, Some(1), Some("s")),
            checkpoint(3, Some(2), Some("s")),
            checkpoint(4, Some(1), Some("t")),
        ];
        assert_eq!(lineage(&list, 3), [1, 2, 3]);
        assert_eq!(lineage(&list, 4), [1, 4]);
        assert!(lineage(&list, 0).is_empty());
    }

    #[test]
    fn a_native_session_continues_only_from_where_it_ended() {
        let list = [
            checkpoint(1, Some(0), Some("s")),
            checkpoint(2, Some(1), Some("s")),
            checkpoint(3, Some(1), Some("t")),
        ];
        assert_eq!(
            continuity(&list, 2, Ok(())),
            SessionContinuity::Native {
                session: "s".into()
            }
        );
        match continuity(&list, 1, Ok(())) {
            SessionContinuity::Summary { turns, reason } => {
                assert_eq!(turns, [1]);
                assert!(reason.contains("went on to turn 2"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        match continuity(&list, 3, Err("x cannot fork a session".into())) {
            SessionContinuity::Summary { turns, reason } => {
                assert_eq!(turns, [1, 3]);
                assert_eq!(reason, "x cannot fork a session");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            continuity(&list, 0, Ok(())),
            SessionContinuity::Fresh { .. }
        ));
        let unrecorded = [checkpoint(1, Some(0), None)];
        assert!(matches!(
            continuity(&unrecorded, 1, Ok(())),
            SessionContinuity::Summary { .. }
        ));
    }

    #[test]
    fn a_summary_quotes_its_lineage_and_is_stripped_from_later_prompts() {
        let at = |n| RecordedEvent {
            at_ms: n,
            activity: Activity::Prompt(String::new()),
        };
        let mut events = vec![
            RecordedEvent {
                activity: Activity::Prompt("first ask".into()),
                ..at(1)
            },
            RecordedEvent {
                activity: Activity::Checkpoint(checkpoint(1, Some(0), Some("s"))),
                ..at(2)
            },
            RecordedEvent {
                activity: Activity::Prompt(compose("old summary", "second ask")),
                ..at(3)
            },
            RecordedEvent {
                activity: Activity::Checkpoint(checkpoint(2, Some(1), None)),
                ..at(4)
            },
        ];
        events.push(RecordedEvent {
            activity: Activity::Prompt("third".into()),
            ..at(5)
        });
        let list = recorded(&events);
        let session = continuity(&list, 2, Ok(()));
        let text = summary("b", &events, &list, 2, "0123456789abcdef", &session).unwrap();
        assert!(text.contains("### Turn 1\nAsked: first ask"), "{text}");
        assert!(text.contains("### Turn 2\nAsked: second ask"), "{text}");
        assert!(!text.contains("old summary"), "{text}");
        assert!(text.contains("commit 0123456789"), "{text}");
        assert_eq!(own_prompt(&compose(&text, "next")), "next");
    }
}
