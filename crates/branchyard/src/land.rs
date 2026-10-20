//! Landing: the engine integrates a delegated child into its parent once
//! the child's work is ready, so a parent delegates and is resumed instead
//! of integrating each child itself.
//!
//! A child spawned with `land` (`by spawn --land`, a plan's `"land":
//! true`) is landed while its parent waits on its children
//! ([`crate::BranchStatus::WaitingOnChildren`]) or is idle (its turn ended
//! `ready` or `no_changes`, as when it delegated from outside a harness):
//! when the child's turn ends `ready` with a candidate, the engine
//! integrates it into the parent's git branch as `by integrate` would, the
//! check on the exact merge included, and the dependents waiting for it to
//! be integrated start. A parent running a turn receives it when that turn
//! ends. A parent whose
//! turn ends while such a child is ready waits on it too, so its work lands
//! before the parent is woken ([`crate::wake::park`]). The parent's wake
//! then has the landed work in its branch and its candidate, and says what
//! landed and what did not ([`wake_lines`]).
//!
//! Children that share their parent's check (they inherited it) land
//! together, all or none, once none of the others left to land with it
//! still runs: a whole-suite check passes only with all of them.
//!
//! A child given a reviewer ([`crate::review`]) is reviewed first, each
//! time it is ready with a candidate no reviewer approved.
//!
//! A child that cannot land is sent a turn to make it land, at most
//! [`FIXES`] times: its work conflicts with the parent's branch (merge the
//! parent's branch and resolve), its own check fails on the merge (fix it),
//! it changed paths outside those it owns ([`crate::scope`]; undo them), or
//! its reviewer asked for changes (make them, or say why not).
//! After that, or when integrating failed for another reason, it is left
//! for its parent, and the wake says why. Children sharing a check that
//! fails on them together are left for the parent at once, since nothing
//! says which of them is at fault.
//!
//! The parent's lease is held while its children land, so no turn or wake
//! starts and nothing settles it in between. Whoever looks at the parent
//! next ([`crate::wake::look`]) lands what is ready first, then wakes it if
//! it waits.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::delegation::Delegate;
use crate::projection::lock;
use crate::record::Recorder;
use crate::review::Verdict;
use crate::state::{Record, Store};
use crate::{git, Activity, BranchStatus, CandidateInfo, Error, RecordedEvent, TaskOptions, Yard};
use branchyard_support::time::now_ms;

/// Turns the engine sends one child to make its work land before it leaves
/// the child for its parent.
pub const FIXES: u32 = 2;

/// Characters of a failed check's output a fix turn quotes.
const OUTPUT_SHOWN: usize = 2000;

/// Characters of a reviewer's findings a landing note quotes; the fix turn
/// has them all.
const NOTE_SHOWN: usize = 600;

/// What became of a child its parent left to the engine to land, as
/// `inspect` shows it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Landing {
    /// Turns the engine sent it to make its work land.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fixes: u32,
    /// Its latest landing: where it landed, or why it has not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The engine stopped trying: its parent decides what to do with it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub left: bool,
    /// The candidate its reviewer approved, so it is not reviewed again
    /// until its work changes ([`crate::review`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed: Option<String>,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// Whether `record` was left to the engine to land.
pub(crate) fn wanted(record: &Record) -> bool {
    record.grant.as_ref().is_some_and(|grant| grant.land)
}

/// Whether the engine lands `record` now: its parent left it to the
/// engine, its turn ended `ready` with a candidate, and it was not left for
/// its parent.
pub(crate) fn pending(record: &Record) -> bool {
    wanted(record)
        && record.info.status == BranchStatus::Ready
        && record.info.candidate.is_some()
        && !record.landing.as_ref().is_some_and(|landing| landing.left)
}

/// Whether `record` is idle with its turn ended (`ready` or `no_changes`),
/// not waiting on its children: what is ready under it lands without its
/// being woken, as when it delegated from outside a harness (`by delegate
/// --parent`, the SDK) or its envelope allows it no wakes.
pub(crate) fn idle(record: &Record) -> bool {
    matches!(
        record.info.status,
        BranchStatus::Ready | BranchStatus::NoChanges
    )
}

/// Whether children may land into `record` now: it waits on them, or it is
/// idle.
fn receiving(record: &Record) -> bool {
    crate::wake::is_parked(&record.info.status) || idle(record)
}

/// The children of `parent` waiting to land, oldest first.
pub(crate) fn pending_children(store: &Store, parent: &Record) -> Vec<String> {
    parent
        .info
        .children
        .iter()
        .filter(|child| {
            store
                .backend()
                .read(child)
                .ok()
                .flatten()
                .is_some_and(|record| pending(&record))
        })
        .cloned()
        .collect()
}

/// Land what is ready under parked `parent`, and again while that makes
/// progress (a turn sent to fix a child may end meanwhile, leaving it ready
/// again). A failure is recorded on the parent's log; what it left pending
/// is landed at the next look.
pub(crate) fn settle(yard: &Yard, parent: &str, given: Option<&TaskOptions>) {
    for _ in 0..=FIXES + 1 {
        match sweep(yard, parent, given) {
            Ok(true) => continue,
            Ok(false) => break,
            Err(error) => {
                note(
                    &yard.store(),
                    parent,
                    parent,
                    format!("landing its children failed: {error}"),
                    true,
                );
                break;
            }
        }
    }
}

/// One pass over `parent`'s children waiting to land, holding its lease.
/// True when it made progress: a child landed, was sent a turn to fix its
/// work, or was left for the parent.
fn sweep(yard: &Yard, parent: &str, given: Option<&TaskOptions>) -> Result<bool, Error> {
    let store = yard.store();
    let Some(record) = store.backend().read(parent)? else {
        return Ok(false);
    };
    if !receiving(&record) || pending_children(&store, &record).is_empty() {
        return Ok(false);
    }
    let (held, lease) = match crate::ops::hold(yard, parent) {
        Ok(held) => held,
        // Another look is landing them, its wake is starting, or a turn of
        // it is.
        Err(Error::Running(_)) => return Ok(false),
        Err(error) => return Err(error),
    };
    if !receiving(&held) {
        lease.finish(None, None)?;
        return Ok(false);
    }
    let landed = land(yard, &held, given);
    // Its branch moved: its candidate is what its branch now holds, so it
    // is truthful whether it is woken or settles as its turn ended.
    let moved = match refreshed(yard, &held) {
        Ok(moved) => moved,
        Err(error) => {
            lease.finish(None, None)?;
            return Err(error);
        }
    };
    match moved {
        Some((record, candidate)) => {
            // An idle branch that had nothing to integrate has now.
            if record.info.status != held.info.status {
                let status = RecordedEvent {
                    at_ms: now_ms(),
                    activity: Activity::Status(record.info.status.clone()),
                };
                store.append(parent, &status, Some(lease.fence()))?;
            }
            let event = RecordedEvent {
                at_ms: now_ms(),
                activity: Activity::Snapshot(candidate),
            };
            lease.finish(Some(&record), Some(&event))?;
        }
        None => lease.finish(None, None)?,
    }
    landed
}

/// Land each batch of `parent`'s children that is ready; see [`sweep`].
fn land(yard: &Yard, parent: &Record, given: Option<&TaskOptions>) -> Result<bool, Error> {
    let store = yard.store();
    let options = parent_options(yard, &parent.info.name, given);
    let delegate =
        crate::delegation::trusted(yard, &parent.info.name, options.clone().unwrap_or_default())?;
    let can_run = options.is_some();
    let children: Vec<Record> = parent
        .info
        .children
        .iter()
        .filter_map(|child| store.backend().read(child).ok().flatten())
        .collect();
    let mut progress = false;
    for batch in batches(&children) {
        // A change outside the paths it owns keeps a child out, whatever
        // its check says; the rest of its batch waits for it.
        let target_head = git::local_branch(yard.root(), &parent.info.git_branch)?
            .ok_or_else(|| Error::Git(format!("{} is missing", parent.info.git_branch)))?;
        let mut strays = Vec::new();
        for child in &batch {
            let Some(candidate) = &child.info.candidate else {
                continue;
            };
            let chain = crate::scope::chain(&store, child)?;
            let outside = crate::scope::outside(
                yard,
                &chain,
                &child.info.base,
                &candidate.commit,
                &target_head,
                &[],
            )?;
            if !outside.is_empty() {
                strays.push((
                    *child,
                    Fix::Scope {
                        paths: outside,
                        scope: crate::scope::describe(&chain),
                    },
                ));
            }
        }
        if !strays.is_empty() {
            for (child, why) in strays {
                progress |= fix(yard, &delegate, can_run, parent, child, why)?;
            }
            continue;
        }
        // A reviewer reads each child's work before it lands, unless one
        // approved this candidate already; the rest of its batch waits, and
        // an approved batch lands on the next pass, from fresh records.
        let mut reviewing = false;
        for child in &batch {
            let (Some(spec), Some(candidate)) = (crate::review::of(child), &child.info.candidate)
            else {
                continue;
            };
            let approved = child.landing.as_ref().and_then(|l| l.reviewed.as_deref());
            if approved == Some(candidate.commit.as_str()) {
                continue;
            }
            reviewing = true;
            if !can_run {
                continue;
            }
            match crate::review::run(yard, &delegate, parent, child, spec) {
                Ok((Verdict::Approve, reviewer)) => {
                    let text = format!("its reviewer on {} approved it", spec.harness);
                    let commit = candidate.commit.clone();
                    update_landing(yard, &child.info.name, |l| {
                        l.reviewed = Some(commit);
                        l.note = Some(text.clone());
                    })?;
                    note(
                        &store,
                        &parent.info.name,
                        &child.info.name,
                        format!("{text} (its review: by log {reviewer})"),
                        false,
                    );
                    progress = true;
                }
                Ok((Verdict::Changes(findings), _)) => {
                    let why = Fix::Review {
                        harness: spec.harness.clone(),
                        findings,
                    };
                    progress |= fix(yard, &delegate, can_run, parent, child, why)?;
                }
                Err(error) => {
                    leave(
                        yard,
                        parent,
                        child,
                        format!("its review could not be done: {error}"),
                    )?;
                    progress = true;
                }
            }
        }
        if reviewing {
            continue;
        }
        let names: Vec<String> = batch.iter().map(|r| r.info.name.clone()).collect();
        // Merged directly, not through the parent's own `integrate`: an
        // attempt that fails is the engine's, not a call the parent made.
        match crate::integrate::merge_many(yard, &names, &parent.info.git_branch) {
            Ok(merged) => {
                for branch in &merged.branches {
                    let short = &branch.commit[..branch.commit.len().min(10)];
                    let text = format!("landed in {} at {short}", merged.target);
                    if let Some(child) = batch.iter().find(|r| r.info.name == branch.branch) {
                        update_landing(yard, &child.info.name, |l| {
                            l.note = Some(text.clone());
                            l.left = false;
                        })?;
                    }
                    note(&store, &parent.info.name, &branch.branch, text, false);
                }
                // As integrating does: what waited for them to be
                // integrated starts, the outcome store learns the merge, and
                // other children the branch now contains are merged too.
                for name in &names {
                    crate::graph::settled(yard, name, options.as_ref());
                    branchyard_support::best_effort(
                        "record the fleet outcome",
                        crate::fleet::observe(yard, name, None),
                    );
                }
                crate::integrate::reconcile_children(yard, &parent.info.name);
                progress = true;
            }
            // Something started a turn of one of them: it lands when it
            // settles again.
            Err(Error::Running(_)) => {}
            Err(Error::Conflict { files }) => match batch.as_slice() {
                [child] => {
                    progress |= fix(
                        yard,
                        &delegate,
                        can_run,
                        parent,
                        child,
                        Fix::Conflict { files },
                    )?;
                }
                many => {
                    for child in many {
                        leave(
                            yard,
                            parent,
                            child,
                            format!(
                                "merging it with the siblings it shares its check with \
                                 conflicted in {}",
                                crate::scope::named(&files)
                            ),
                        )?;
                    }
                    progress = true;
                }
            },
            Err(Error::ConflictBetween { branch, files, .. }) => {
                match batch.iter().find(|r| r.info.name == branch) {
                    Some(child) => {
                        progress |= fix(
                            yard,
                            &delegate,
                            can_run,
                            parent,
                            child,
                            Fix::Conflict { files },
                        )?;
                    }
                    None => {
                        for child in &batch {
                            leave(
                                yard,
                                parent,
                                child,
                                format!("{branch} conflicted in {}", crate::scope::named(&files)),
                            )?;
                        }
                        progress = true;
                    }
                }
            }
            Err(error @ (Error::CheckFailed { .. } | Error::CheckTimedOut { .. })) => {
                let (summary, tail) = check_failure(&error, &parent.info.git_branch);
                match batch.as_slice() {
                    [child] => {
                        progress |= fix(
                            yard,
                            &delegate,
                            can_run,
                            parent,
                            child,
                            Fix::Check { summary, tail },
                        )?;
                    }
                    many => {
                        for child in many {
                            let others: Vec<&str> = many
                                .iter()
                                .filter(|r| r.info.name != child.info.name)
                                .map(|r| r.info.name.as_str())
                                .collect();
                            leave(
                                yard,
                                parent,
                                child,
                                format!(
                                    "{summary} with {} merged together, the siblings it shares \
                                     its check with, so which of them is at fault is yours to \
                                     find: integrate them yourself once fixed",
                                    others.join(", ")
                                ),
                            )?;
                        }
                        progress = true;
                    }
                }
            }
            Err(error) => {
                for child in &batch {
                    leave(
                        yard,
                        parent,
                        child,
                        format!("integrating it failed: {error}"),
                    )?;
                }
                progress = true;
            }
        }
    }
    Ok(progress)
}

/// The children ready to land, in the batches that land together, in the
/// order of `children`: those that inherited their parent's check, which
/// they pass only together, in one batch once none of the others left to
/// land with that check still runs; every other child alone.
fn batches(children: &[Record]) -> Vec<Vec<&Record>> {
    let mut out: Vec<Vec<&Record>> = Vec::new();
    let mut grouped: BTreeSet<Vec<String>> = BTreeSet::new();
    for child in children.iter().filter(|r| pending(r)) {
        let shared = match (&child.check, child.check_inherited) {
            (Some(check), true) => check.clone(),
            _ => {
                out.push(vec![child]);
                continue;
            }
        };
        if !grouped.insert(shared.clone()) {
            continue;
        }
        let siblings = children
            .iter()
            .filter(|r| wanted(r) && r.check_inherited && r.check.as_ref() == Some(&shared));
        if siblings
            .clone()
            .any(|r| crate::wake::unsettled(&r.info.status))
        {
            continue;
        }
        out.push(siblings.filter(|r| pending(r)).collect());
    }
    out
}

/// Why a child cannot land, and what its fix turn asks of it.
enum Fix {
    /// Its work conflicts with the parent's branch in these files.
    Conflict { files: Vec<String> },
    /// Its check failed on its work merged into the parent's branch.
    Check { summary: String, tail: String },
    /// It changed these paths outside the scope `scope` describes.
    Scope { paths: Vec<String>, scope: String },
    /// Its reviewer, on `harness`, asked for these changes.
    Review {
        harness: String,
        findings: Vec<String>,
    },
}

impl Fix {
    /// One line for the parent's log and the child's landing note.
    fn reason(&self, target: &str) -> String {
        match self {
            Fix::Conflict { files } => {
                format!(
                    "it conflicts with {target} in {}",
                    crate::scope::named(files)
                )
            }
            Fix::Check { summary, .. } => summary.clone(),
            Fix::Scope { paths, scope } => format!(
                "it changes {} outside the paths it owns ({scope})",
                crate::scope::named(paths)
            ),
            Fix::Review { harness, findings } => {
                let all = findings.join("; ");
                let shown = match all.chars().count() > NOTE_SHOWN {
                    true => format!("{}…", all.chars().take(NOTE_SHOWN).collect::<String>()),
                    false => all,
                };
                format!("its reviewer on {harness} asked for changes: {shown}")
            }
        }
    }

    /// The turn the engine sends the child, the `n`th it sends it.
    fn prompt(&self, target: &str, base: &str, n: u32) -> String {
        let (what, how) = match self {
            Fix::Conflict { files } => (
                format!(
                    "your work conflicts with your parent's branch {target} in {}",
                    files.join(", ")
                ),
                format!(
                    "Merge your parent's branch into yours and resolve the conflicts, keeping \
                     what both sides meant: run `git merge {target}` in your worktree, fix each \
                     conflicted file, and commit the merge. Then run `by check` and fix what it \
                     reports."
                ),
            ),
            Fix::Check { summary, tail } => (
                format!(
                    "{}. Its output ended:\n\n{tail}",
                    summary.replacen("its check", "your check", 1)
                ),
                "Fix it in your worktree, and run `by check` until it passes.".to_owned(),
            ),
            Fix::Scope { paths, scope } => (
                format!(
                    "you changed {} outside the paths you own ({scope})",
                    paths.join(", ")
                ),
                format!(
                    "Undo those changes: for each file, run `git checkout {base} -- FILE`, or \
                     `git rm FILE` for one you added, and commit. If your task needs them, say \
                     what and why in your final message; your parent will make them or give them \
                     to whoever owns them."
                ),
            ),
            Fix::Review { harness, findings } => (
                format!(
                    "your reviewer on {harness} asked for changes before it lands:\n\n{}",
                    findings
                        .iter()
                        .enumerate()
                        .map(|(i, finding)| format!("{}. {finding}", i + 1))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
                "Make each change in your worktree, and run `by check`. If a finding is wrong, \
                 leave that code as it is and say why in your final message: your next reviewer \
                 reads it with your work."
                    .to_owned(),
            ),
        };
        format!(
            "<branchyard-land>\n\
             Your parent left your work to the engine to integrate into its branch, and it could \
             not land: {what}\n\n\
             {how}\n\n\
             Then end your turn: the engine integrates your work again. This is turn {n} of at \
             most {FIXES} the engine sends you to make your work land; after that, your parent \
             decides.\n\
             </branchyard-land>"
        )
    }
}

/// A check failure on a merge into `target`, as one line and its output's
/// tail.
fn check_failure(error: &Error, target: &str) -> (String, String) {
    let (summary, tail) = match error {
        Error::CheckTimedOut {
            timeout,
            output_tail,
            ..
        } => (
            format!(
                "its check timed out after {}s on its work merged into {target}",
                timeout.as_secs()
            ),
            output_tail.as_str(),
        ),
        Error::CheckFailed { output_tail, .. } => (
            format!("its check failed on its work merged into {target}"),
            output_tail.as_str(),
        ),
        other => (format!("its check failed: {other}"), ""),
    };
    let tail = tail.trim_end();
    let count = tail.chars().count();
    let tail = match count > OUTPUT_SHOWN {
        true => format!(
            "…{}",
            tail.chars().skip(count - OUTPUT_SHOWN).collect::<String>()
        ),
        false => tail.to_owned(),
    };
    (summary, tail)
}

/// Send `child` a turn to make its work land, or leave it for its parent
/// once it was sent [`FIXES`]. False when nothing changed: no engine here
/// can run its turn, and the next look that can does it.
fn fix(
    yard: &Yard,
    delegate: &Delegate,
    can_run: bool,
    parent: &Record,
    child: &Record,
    why: Fix,
) -> Result<bool, Error> {
    let target = &parent.info.git_branch;
    let reason = why.reason(target);
    let landing = child.landing.clone().unwrap_or_default();
    if landing.fixes >= FIXES {
        leave(
            yard,
            parent,
            child,
            format!(
                "{reason}, after {} turns the engine sent it to fix that",
                landing.fixes
            ),
        )?;
        return Ok(true);
    }
    if !can_run {
        return Ok(false);
    }
    let n = landing.fixes + 1;
    update_landing(yard, &child.info.name, |l| {
        l.fixes = n;
        l.note = Some(reason.clone());
        l.left = false;
    })?;
    match delegate.send(&child.info.name, &why.prompt(target, &child.info.base, n)) {
        Ok(_) => {
            note(
                &yard.store(),
                &parent.info.name,
                &child.info.name,
                format!("{reason}; sent it a turn to fix that ({n} of {FIXES})"),
                false,
            );
            Ok(true)
        }
        Err(error) => {
            leave(
                yard,
                parent,
                child,
                format!("{reason}, and the turn to fix that could not start: {error}"),
            )?;
            Ok(true)
        }
    }
}

/// Stop landing `child`: its parent decides, and its wake says `why`.
fn leave(yard: &Yard, parent: &Record, child: &Record, why: String) -> Result<(), Error> {
    update_landing(yard, &child.info.name, |l| {
        l.note = Some(why.clone());
        l.left = true;
    })?;
    note(
        &yard.store(),
        &parent.info.name,
        &child.info.name,
        format!("left for {}: {why}", parent.info.name),
        true,
    );
    Ok(())
}

/// Change the landing on `name`'s record as it is now, under its lease. A
/// child that is running again is left as it is: its next settle lands it.
fn update_landing(yard: &Yard, name: &str, change: impl FnOnce(&mut Landing)) -> Result<(), Error> {
    let (mut record, lease) = match crate::ops::hold(yard, name) {
        Ok(held) => held,
        Err(Error::Running(_)) => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut landing = record.landing.take().unwrap_or_default();
    change(&mut landing);
    record.landing = Some(landing);
    lease.finish(Some(&record), None)
}

/// Record a landing on `parent`'s log, as its other delegation operations
/// are recorded.
fn note(store: &Store, parent: &str, branch: &str, outcome: String, refused: bool) {
    if let Ok(mut recorder) = Recorder::open(store, parent, None) {
        branchyard_support::best_effort(
            "record the landing",
            recorder.record(Activity::Delegation {
                tool: "land".into(),
                branch: branch.to_owned(),
                outcome,
                refused,
            }),
        );
    }
}

/// The options the engine runs a child's turn with for `parent`: those the
/// parent's own graph was applied with in this process, its parked turn's,
/// else the caller's. `None` when this process has none.
fn parent_options(yard: &Yard, parent: &str, given: Option<&TaskOptions>) -> Option<TaskOptions> {
    if let Some(found) = lock(&yard.hub.graph_options).get(parent) {
        return Some(found.clone());
    }
    if let Some(found) = lock(&yard.hub.wake_options).get(parent) {
        return Some(crate::graph::child_options(found));
    }
    given.map(crate::graph::child_options)
}

/// `parent` as it should be stored now that its branch may have moved: its
/// candidate the branch's head against its base, and its status (or, while
/// it waits on its children, the status it settles to if it is not woken)
/// `ready` when it has one. `None` when its branch did not move past its
/// candidate.
fn refreshed(yard: &Yard, parent: &Record) -> Result<Option<(Record, CandidateInfo)>, Error> {
    let Some(head) = git::local_branch(yard.root(), &parent.info.git_branch)? else {
        return Ok(None);
    };
    if parent.info.candidate.as_ref().map(|c| c.commit.as_str()) == Some(head.as_str())
        || parent.info.base.is_empty()
        || git::same_tree(yard.root(), &parent.info.base, &head)?
    {
        return Ok(None);
    }
    let (files_changed, insertions, deletions) =
        git::numstat(yard.root(), &parent.info.base, &head)?;
    let candidate = CandidateInfo {
        commit: head,
        files_changed,
        insertions,
        deletions,
    };
    let mut record = yard.store().read(&parent.info.name)?;
    record.info.candidate = Some(candidate.clone());
    let settles = match record.parked.as_mut() {
        Some(parked) => &mut parked.ended,
        None => &mut record.info.status,
    };
    if matches!(
        settles,
        BranchStatus::NoChanges | BranchStatus::Merged { .. }
    ) {
        *settles = BranchStatus::Ready;
    }
    Ok(Some((record, candidate)))
}

/// The lines a parent's wake says about the children it left to land and
/// waited on (`waited`), so a child landed before an earlier wake is not
/// told again; none when there are none.
pub(crate) fn wake_lines(store: &Store, parent: &Record, waited: &[String]) -> Option<String> {
    let mut landed = Vec::new();
    let mut left = Vec::new();
    let mut waiting = Vec::new();
    for child in parent.info.children.iter().filter(|c| waited.contains(c)) {
        let Ok(Some(record)) = store.backend().read(child) else {
            continue;
        };
        if !wanted(&record) {
            continue;
        }
        let note = record
            .landing
            .as_ref()
            .and_then(|l| l.note.clone())
            .unwrap_or_default();
        match (&record.info.status, &record.landing) {
            (BranchStatus::Merged { .. }, _) => landed.push(child.clone()),
            (_, Some(landing)) if landing.left => left.push(format!("{child} ({note})")),
            (status, _) if *status == BranchStatus::Ready => {
                waiting.push(format!("{child} (ready, not landed yet)"))
            }
            (status, _) => waiting.push(format!(
                "{child} ({}, nothing to land)",
                crate::wake::status_text(status)
            )),
        }
    }
    if landed.is_empty() && left.is_empty() && waiting.is_empty() {
        return None;
    }
    let mut text = String::new();
    if !landed.is_empty() {
        text.push_str(&format!(
            "Landed for you, so they are in your branch and its candidate now: {}.\n",
            landed.join(", ")
        ));
    }
    if !left.is_empty() {
        text.push_str(&format!("Left for you, not landed: {}.\n", left.join("; ")));
    }
    if !waiting.is_empty() {
        text.push_str(&format!(
            "Not landed, and not left to you: {}.\n",
            waiting.join("; ")
        ));
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_fix_says_what_failed_and_how_to_make_it_land() {
        let conflict = Fix::Conflict {
            files: vec!["a.rs".into()],
        };
        assert_eq!(
            conflict.reason("by/root"),
            "it conflicts with by/root in a.rs"
        );
        let text = conflict.prompt("by/root", "1234", 1);
        assert!(text.starts_with("<branchyard-land>\n"), "{text}");
        assert!(text.contains("`git merge by/root`"), "{text}");
        assert!(text.contains("turn 1 of at most 2"), "{text}");
        assert!(text.ends_with("</branchyard-land>"), "{text}");

        let check = Fix::Check {
            summary: "its check failed on its work merged into by/root".into(),
            tail: "1 test failed".into(),
        };
        let text = check.prompt("by/root", "1234", 2);
        assert!(
            text.contains("your check failed on its work merged"),
            "{text}"
        );
        assert!(text.contains("1 test failed"), "{text}");

        let scope = Fix::Scope {
            paths: vec!["docs/x.md".into()],
            scope: "src".into(),
        };
        assert_eq!(
            scope.reason("by/root"),
            "it changes docs/x.md outside the paths it owns (src)"
        );
        assert!(scope
            .prompt("by/root", "1234", 1)
            .contains("`git checkout 1234 -- FILE`"));
    }

    #[test]
    fn a_long_check_output_is_cut_from_the_front() {
        let error = Error::CheckFailed {
            output_tail: format!("start{}end", "x".repeat(OUTPUT_SHOWN)),
            checks: Vec::new(),
            shared: None,
            own_work: Vec::new(),
        };
        let (summary, tail) = check_failure(&error, "by/root");
        assert_eq!(summary, "its check failed on its work merged into by/root");
        assert!(tail.starts_with('…') && tail.ends_with("end"), "{tail}");
        assert_eq!(tail.chars().count(), OUTPUT_SHOWN + 1);
    }
}
