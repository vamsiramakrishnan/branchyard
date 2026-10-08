//! Integrating delegated children into their parent, several at once and
//! idempotently, and reconciling recorded statuses with git.
//!
//! [`merge_many`] merges every named branch's candidate, in order, in one
//! temporary worktree, runs the branches' checks once on the result, and
//! moves the target with one compare-and-swap
//! ([`branchyard_workspace::Repository::integrate_many`]). Each branch's
//! merge is journaled as [`crate::ops::merge`] journals one (`merge <target>
//! <candidate>`, turn 0, intent before the effect), so an engine that
//! stops after the swap is recognised on the next attempt: a pending step
//! whose candidate the target contains is recorded as done.
//!
//! A candidate the target already contains is not an error: the branch is
//! recorded as merged through the commit that brought it in (`already`,
//! `via`). [`reconcile_children`] applies the same rule to a parent's
//! `ready` children whenever the parent's branch moves, however it moved
//! (an integration, a merge its harness ran itself, a turn's checkpoint).

use std::collections::{BTreeMap, BTreeSet};

use branchyard_support::best_effort;
use branchyard_workspace::{Candidate, Check, Commit, DiffStat, IntegrationError};
use serde_json::json;

use crate::ops::{self, CHECK_TIMEOUT};
use crate::record::Recorder;
use crate::state::{Begun, Lease, Record};
use crate::{git, names, Activity, BranchStatus, Error, Merged, MergedAll, RecordedEvent, Yard};
use branchyard_support::time::now_ms;

/// Integrate `names`' candidates into `target`, all or none; see the
/// module documentation. A branch already recorded as merged into
/// `target`, or whose candidate `target` already contains, is reported
/// with `already` and changes nothing.
pub(crate) fn merge_many(yard: &Yard, names: &[String], target: &str) -> Result<MergedAll, Error> {
    if names.is_empty() {
        return Err(Error::Denied(
            "name at least one branch to integrate".into(),
        ));
    }
    let mut seen = BTreeSet::new();
    let mut held: Vec<(Record, Lease)> = Vec::new();
    for name in names {
        if !seen.insert(name.as_str()) {
            return Err(Error::Denied(format!("{name} is named twice")));
        }
        held.push(ops::hold(yard, name)?);
    }
    let store = yard.store();
    let expected = git::local_branch(&yard.root, target)?
        .ok_or_else(|| Error::Git(format!("no local branch named {target}")))?;
    // What each branch needs: `Some` is settled already, `None` is merged
    // in this call.
    let mut settled: Vec<Option<Merged>> = Vec::new();
    let mut begun: Vec<String> = vec![String::new(); held.len()];
    for (index, (record, lease)) in held.iter().enumerate() {
        let name = &record.info.name;
        let Some(candidate) = record.info.candidate.clone() else {
            abandon(&store, &held, &begun);
            return Err(Error::NoCandidate(name.clone()));
        };
        if let BranchStatus::Merged {
            target: into,
            commit,
        } = &record.info.status
        {
            if into == target {
                settled.push(Some(Merged {
                    branch: name.clone(),
                    target: target.to_owned(),
                    previous: expected.clone(),
                    commit: commit.clone(),
                    already: true,
                    via: Some(describe(yard, commit)),
                }));
                continue;
            }
        }
        let step = format!("merge {target} {}", candidate.commit);
        let intent =
            json!({ "target": target, "candidate": candidate.commit, "expected": expected });
        let recorded = match store
            .backend()
            .begin_step(lease.fence(), 0, &step, &intent)?
        {
            Begun::Done(outcome) => serde_json::from_value::<Merged>(outcome).ok(),
            Begun::Pending(_) | Begun::Fresh => None,
        };
        if recorded.is_none() {
            begun[index] = step;
        }
        settled.push(recorded);
    }

    // Everything still to merge, in the order named.
    let mut candidates = Vec::new();
    let mut checks: Vec<Vec<String>> = Vec::new();
    let mut by_git_branch: BTreeMap<String, String> = BTreeMap::new();
    for ((record, _), done) in held.iter().zip(&settled) {
        if done.is_some() {
            continue;
        }
        let Some(candidate) = record.info.candidate.as_ref() else {
            continue;
        };
        let branch = names::validate(&record.info.name)?;
        by_git_branch.insert(branch.branch(), record.info.name.clone());
        candidates.push(Candidate {
            branch,
            base: Commit(record.info.base.clone()),
            head: Commit(candidate.commit.clone()),
            stat: DiffStat {
                files_changed: candidate.files_changed as usize,
                insertions: candidate.insertions as usize,
                deletions: candidate.deletions as usize,
            },
        });
        if let Some(check) = &record.check {
            if !checks.contains(check) {
                checks.push(check.clone());
            }
        }
    }
    let checks: Vec<Check> = checks
        .into_iter()
        .map(|argv| Check {
            argv,
            timeout: CHECK_TIMEOUT,
        })
        .collect();
    let integrated = match candidates.is_empty() {
        true => None,
        false => {
            let result = {
                let _lock = git::lock();
                yard.repo
                    .integrate_many(&candidates, target, &Commit(expected.clone()), &checks)
            };
            match result {
                Ok(integrated) => Some(integrated),
                Err(error) => {
                    // Refused before the target moved: nothing happened.
                    abandon(&store, &held, &begun);
                    return Err(many_error(error, target, &by_git_branch));
                }
            }
        }
    };
    let after = integrated
        .as_ref()
        .map_or_else(|| expected.clone(), |i| i.merged.0.clone());
    let merges: BTreeMap<String, Option<String>> = integrated
        .iter()
        .flat_map(|i| &i.candidates)
        .map(|c| (c.branch.clone(), c.merge.as_ref().map(|m| m.0.clone())))
        .collect();

    let mut branches = Vec::new();
    for ((mut record, lease), done) in held.into_iter().zip(settled) {
        let name = record.info.name.clone();
        let merged = match done {
            Some(merged) => merged,
            None => {
                let git_branch = record.info.git_branch.clone();
                let head = record
                    .info
                    .candidate
                    .as_ref()
                    .map(|c| c.commit.clone())
                    .unwrap_or_default();
                let merged = match merges.get(&git_branch).cloned().flatten() {
                    Some(commit) => Merged {
                        branch: name.clone(),
                        target: target.to_owned(),
                        previous: expected.clone(),
                        commit,
                        already: false,
                        via: None,
                    },
                    None => {
                        let via = brought_in_by(yard, &head, target)?.unwrap_or(after.clone());
                        Merged {
                            branch: name.clone(),
                            target: target.to_owned(),
                            previous: expected.clone(),
                            via: Some(describe(yard, &via)),
                            commit: via,
                            already: true,
                        }
                    }
                };
                let step = format!("merge {target} {head}");
                store.backend().finish_step(
                    lease.fence(),
                    0,
                    &step,
                    &serde_json::to_value(&merged).unwrap_or_default(),
                )?;
                merged
            }
        };
        let changed = !matches!(&record.info.status,
            BranchStatus::Merged { target: into, .. } if into == target);
        if changed {
            record.info.status = BranchStatus::Merged {
                target: target.to_owned(),
                commit: merged.commit.clone(),
            };
            let mut recorder = Recorder::fenced(&store, lease.fence(), None);
            if let Some(via) = merged.via.as_ref().filter(|_| merged.already) {
                recorder.record(Activity::Warning(format!(
                    "{target} already contained its candidate, brought in by {via}; recorded \
                     as merged without a merge of its own"
                )))?;
            }
            for activity in crate::snapshots::discard_kept(yard, &record, "the branch was merged") {
                recorder.record(activity)?;
            }
            let event = RecordedEvent {
                at_ms: now_ms(),
                activity: Activity::Status(record.info.status.clone()),
            };
            lease.finish(Some(&record), Some(&event))?;
        } else {
            lease.finish(None, None)?;
        }
        branches.push(merged);
    }
    if let Some(integrated) = &integrated {
        // A checkout of the target that could not follow is said where its
        // owner looks: on the first branch's log.
        if let (false, Some(first)) = (integrated.stale_checkouts.is_empty(), branches.first()) {
            if let Ok(mut recorder) = Recorder::open(&store, &first.branch, None) {
                for worktree in &integrated.stale_checkouts {
                    best_effort(
                        "record a stale checkout",
                        recorder.record(Activity::Warning(format!(
                            "{} still has {target}'s previous files; run `git read-tree -m -u \
                             {} {}` there",
                            worktree.display(),
                            integrated.previous,
                            integrated.merged
                        ))),
                    );
                }
            }
        }
    }
    Ok(MergedAll {
        target: target.to_owned(),
        previous: expected,
        commit: after,
        branches,
    })
}

/// Abandon the merge steps this call began: the target did not move.
fn abandon(store: &crate::state::Store, held: &[(Record, Lease)], begun: &[String]) {
    for ((_, lease), step) in held.iter().zip(begun) {
        if !step.is_empty() {
            best_effort(
                "abandon the merge step",
                store.backend().abandon_step(lease.fence(), 0, step),
            );
        }
    }
}

fn many_error(error: IntegrationError, target: &str, names: &BTreeMap<String, String>) -> Error {
    let name = |git_branch: &str| {
        names
            .get(git_branch)
            .cloned()
            .unwrap_or_else(|| git_branch.to_owned())
    };
    match error {
        IntegrationError::ConflictWith {
            candidate,
            merged,
            files,
        } => Error::ConflictBetween {
            branch: name(&candidate),
            target: target.to_owned(),
            merged: merged.iter().map(|m| name(m)).collect(),
            files,
        },
        other => ops::integration_error(other, target),
    }
}

/// The commit on `target`'s first-parent line that brought `commit` in.
fn brought_in_by(yard: &Yard, commit: &str, target: &str) -> Result<Option<String>, Error> {
    yard.repo
        .brought_in_by(&Commit(commit.to_owned()), target)
        .map(|found| found.map(|c| c.0))
        .map_err(git::error)
}

/// `commit`, short, with its subject.
fn describe(yard: &Yard, commit: &str) -> String {
    match git::run(&yard.root, &["log", "-1", "--format=%h %s", commit]) {
        Ok(line) if !line.trim().is_empty() => line.trim().to_owned(),
        _ => commit.to_owned(),
    }
}

/// Mark merged each of `parent`'s settled children whose candidate
/// `parent`'s git branch now contains, through the commit that brought it
/// in, and let what depends on them advance. A child another engine holds
/// is left for the next look. Returns the children marked.
pub(crate) fn reconcile_children(yard: &Yard, parent: &str) -> Vec<String> {
    let store = yard.store();
    let Ok(Some(record)) = store.backend().read(parent) else {
        return Vec::new();
    };
    if record.info.children.is_empty() {
        return Vec::new();
    }
    let target = record.info.git_branch.clone();
    let Ok(Some(head)) = git::local_branch(&yard.root, &target) else {
        return Vec::new();
    };
    let mut marked = Vec::new();
    for child in &record.info.children {
        let Ok(Some(found)) = store.backend().read(child) else {
            continue;
        };
        let reconcilable = matches!(
            found.info.status,
            BranchStatus::Ready | BranchStatus::Interrupted
        );
        let Some(candidate) = found.info.candidate.as_ref().filter(|_| reconcilable) else {
            continue;
        };
        let contained = git::test(
            &yard.root,
            &["merge-base", "--is-ancestor", &candidate.commit, &head],
        )
        .unwrap_or(false);
        if !contained {
            continue;
        }
        if let Some(done) = best_effort(
            "record a child its parent already contains as merged",
            mark_contained(yard, child, &target),
        ) {
            if done {
                marked.push(child.clone());
            }
        }
    }
    for child in &marked {
        crate::graph::settled(yard, child, None);
    }
    marked
}

/// Record `name` as merged into `target`, which contains its candidate.
/// False when it changed meanwhile, or another engine holds it.
fn mark_contained(yard: &Yard, name: &str, target: &str) -> Result<bool, Error> {
    let (mut record, lease) = match ops::hold(yard, name) {
        Ok(held) => held,
        Err(Error::Running(_)) => return Ok(false),
        Err(error) => return Err(error),
    };
    let Some(candidate) = record.info.candidate.clone() else {
        lease.finish(None, None)?;
        return Ok(false);
    };
    if !matches!(
        record.info.status,
        BranchStatus::Ready | BranchStatus::Interrupted
    ) {
        lease.finish(None, None)?;
        return Ok(false);
    }
    let Some(via) = brought_in_by(yard, &candidate.commit, target)? else {
        lease.finish(None, None)?;
        return Ok(false);
    };
    let store = yard.store();
    let mut recorder = Recorder::fenced(&store, lease.fence(), None);
    recorder.record(Activity::Warning(format!(
        "{target} contains its candidate, brought in by {}; recorded as merged",
        describe(yard, &via)
    )))?;
    record.info.status = BranchStatus::Merged {
        target: target.to_owned(),
        commit: via,
    };
    let event = RecordedEvent {
        at_ms: now_ms(),
        activity: Activity::Status(record.info.status.clone()),
    };
    lease.finish(Some(&record), Some(&event))?;
    Ok(true)
}
