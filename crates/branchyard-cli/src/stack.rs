//! Stacked branches and pull requests: `by run --base BRANCH` starts a
//! branch on another branch's candidate ([`stack_task`]), `by pr` opens its
//! pull request against the parent's pull request head and lists the stack
//! in the body ([`plan`], [`listing`]), and once the parent's pull request
//! has merged the branch is rebased onto the branch it merged into and its
//! pull request retargeted ([`plan`] again, so `by pr` and `by pr --watch`
//! both do it). See `docs/pull-requests.md#stacked-pull-requests`.

use branchyard::{Branch, BranchInfo, PullRequestActivity, Restack, Restacked, StackLink, Yard};
use branchyard_workspace::Git;
use serde_json::{json, Value};

use crate::args::{PrArgs, TaskArgs};
use crate::commands::Failure;
use crate::errors::Report;
use crate::gh::Gh;
use crate::pr::{state, PrState};

/// `--base NAME` where NAME is a branch of this yard: the base becomes that
/// branch's candidate commit, and the branch is stacked on it. A base that
/// names no branch is a revision, as ever.
pub fn stack_task(task: &TaskArgs, yard: &Yard) -> Result<(TaskArgs, Option<StackLink>), Failure> {
    let Some(given) = task.base.as_deref() else {
        return Ok((task.clone(), None));
    };
    let branches = yard.branches()?;
    let Some(parent) = branches.iter().find(|b| b.name == given) else {
        return Ok((task.clone(), None));
    };
    let Some(candidate) = &parent.candidate else {
        return Err(Report::new(
            "no_candidate",
            format!("{given} has no candidate to stack a branch on"),
        )
        .next(format!(
            "by log {given} (wait for its turn to end), then run this again"
        ))
        .into());
    };
    let mut task = task.clone();
    task.base = Some(candidate.commit.clone());
    eprintln!(
        "by: stacked on {given} at {}",
        candidate.commit.get(..10).unwrap_or(&candidate.commit)
    );
    Ok((
        task,
        Some(StackLink {
            parent: given.to_owned(),
            base: candidate.commit.clone(),
        }),
    ))
}

/// Record on `branch` what it is stacked on.
pub fn link_stack(branch: &Branch, link: &StackLink) -> Result<(), Failure> {
    branch.record_pull_request(PullRequestActivity::Stacked(link.clone()))?;
    Ok(())
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

/// Work out the base of a stacked branch's pull request and, when the
/// parent's pull request has merged, rebase the branch onto what it merged
/// into (recording `Restacked`). A branch that is not stacked gets the
/// default plan. A conflict is recorded and reported, never forced.
pub fn plan(branch: &Branch, gh: &Gh, args: &PrArgs) -> Result<Option<String>, Failure> {
    let yard = branch.yard();
    let name = &branch.info().name;
    let Some(stack) = state(branch.info(), &branch.events()?).stack else {
        return Ok(None);
    };
    let parent_name = &stack.parent;
    let parent = yard.branch(parent_name).map_err(|_| {
        Report::new(
            "state",
            format!("{name} is stacked on {parent_name}, which no longer exists"),
        )
        .next(format!(
            "by pr {name} --base <branch> opens it against a branch of your choosing"
        ))
    })?;
    let parent_state = state(parent.info(), &parent.events()?);
    let Some(parent_pr) = parent_state.pull_request.clone() else {
        return Err(Report::new(
            "state",
            format!("{name} is stacked on {parent_name}, which has no pull request yet"),
        )
        .next(format!("by pr {parent_name}, then by pr {name}"))
        .into());
    };
    let view: Value = gh.json(
        &[
            "pr",
            "view",
            &parent_pr.number.to_string(),
            "--json",
            "state,baseRefName",
        ],
        false,
    )?;
    match text(&view["state"]).as_str() {
        "MERGED" => {}
        "CLOSED" => {
            return Err(Report::new(
                "state",
                format!(
                    "{parent_name}'s pull request #{} was closed without merging, so {name}'s \
                     base is gone",
                    parent_pr.number
                ),
            )
            .next(format!(
                "by pr {name} --base <branch> opens it against another branch"
            ))
            .into())
        }
        _ => {
            return Ok(Some(parent_pr.head));
        }
    }
    // The parent merged: sit on what it merged into.
    let onto = text(&view["baseRefName"]);
    if onto.is_empty() {
        return Err(Report::new(
            "command_failed",
            format!("gh gave no base for pull request #{}", parent_pr.number),
        )
        .into());
    }
    let root = yard.root();
    Git::new(root)
        .args(["fetch", "--quiet", "--no-tags", &args.git_remote, &onto])
        .run()
        .map_err(|e| {
            Report::new(
                "git",
                format!("could not fetch {onto} from {}: {e}", args.git_remote),
            )
            .next("git remote -v")
        })?;
    let onto_commit = Git::new(root)
        .args(["rev-parse", "--verify", "FETCH_HEAD^{commit}"])
        .run()
        .map_err(|e| Report::new("git", e.to_string()))?
        .trim()
        .to_owned();
    let from = branch
        .info()
        .candidate
        .as_ref()
        .map(|c| c.commit.clone())
        .unwrap_or_default();
    let outcome = branch.restack(&onto_commit)?;
    let to = match &outcome {
        Restacked::Done { to, .. } => to.clone(),
        _ => from.clone(),
    };
    match outcome {
        Restacked::Conflict(files) => {
            branch.record_pull_request(PullRequestActivity::RestackBlocked {
                parent: parent_name.clone(),
                onto: onto.clone(),
                files: files.clone(),
            })?;
            Err(Report::new(
                "conflict",
                format!(
                    "{parent_name}'s pull request #{} merged into {onto}, but {name} does not \
                     rebase onto it cleanly; nothing was changed",
                    parent_pr.number
                ),
            )
            .line("conflicts", files.join(", "))
            .next(format!(
                "by send {name} \"rebase onto {} and resolve the conflicts in {}\"",
                &args.git_remote_ref(&onto),
                files.join(", ")
            ))
            .next(format!("by pr {name} again once it is resolved"))
            .into())
        }
        Restacked::Done { .. } | Restacked::UpToDate => {
            branch.record_pull_request(PullRequestActivity::Restacked(Restack {
                parent: parent_name.clone(),
                onto: onto.clone(),
                onto_commit,
                from,
                to,
                new_parent: parent_state.stack.map(|s| s.parent),
            }))?;
            Ok(Some(onto))
        }
    }
}

impl PrArgs {
    /// `origin/main`: the remote-tracking name of `branch`.
    fn git_remote_ref(&self, branch: &str) -> String {
        format!("{}/{branch}", self.git_remote)
    }
}

/// Whether the remote branch `head` is still where `by pr` last put it
/// (`pushed`), so a rewritten history may replace it: a lease, checked with
/// `ls-remote`.
pub fn may_replace(root: &std::path::Path, remote: &str, head: &str, last: Option<&str>) -> bool {
    let Some(last) = last else { return false };
    Git::new(root)
        .args(["ls-remote", "--", remote])
        .arg(format!("refs/heads/{head}"))
        .run()
        .map(|out| out.split_whitespace().next() == Some(last))
        .unwrap_or(false)
}

/// The pull-request state of every branch stacked directly on `name`.
fn children(yard: &Yard, name: &str) -> Vec<(BranchInfo, PrState)> {
    let Ok(all) = yard.branches() else {
        return Vec::new();
    };
    all.into_iter()
        .filter_map(|info| {
            let branch = yard.branch(&info.name).ok()?;
            let st = state(branch.info(), &branch.events().ok()?);
            (st.stack.as_ref().is_some_and(|s| s.parent == name)).then_some((info, st))
        })
        .collect()
}

/// The stack a pull request belongs to, as Markdown lines for its body:
/// the branches below it (nearest the target first), then it, then the
/// branches stacked on it. Empty when the branch is in no stack.
pub fn listing(yard: &Yard, info: &BranchInfo, st: &PrState) -> Vec<String> {
    let kids = children(yard, &info.name);
    if st.stack.is_none() && kids.is_empty() {
        return Vec::new();
    }
    let mut below = Vec::new();
    let mut cursor = st.stack.clone();
    while let Some(link) = cursor {
        // A cycle cannot be made by `--base`, but a log can be edited.
        if below.len() > 32 {
            break;
        }
        let Some(branch) = yard.branch(&link.parent).ok() else {
            below.push(format!("`{}`", link.parent));
            break;
        };
        let st = state(branch.info(), &branch.events().unwrap_or_default());
        below.push(match &st.pull_request {
            Some(pr) => format!("{} `{}`", pr.url, link.parent),
            None => format!("`{}` (no pull request yet)", link.parent),
        });
        cursor = st.stack;
    }
    below.reverse();
    let mut lines = vec![
        "## Stack".to_owned(),
        String::new(),
        "This pull request is part of a stack. Merge from the bottom up; each pull request \
         is retargeted when the one below it merges."
            .to_owned(),
        String::new(),
    ];
    let mut n = 0;
    for line in below {
        n += 1;
        lines.push(format!("{n}. {line}"));
    }
    n += 1;
    lines.push(format!("{n}. **this pull request** (`{}`)", info.name));
    for (kid, kst) in kids {
        n += 1;
        lines.push(match &kst.pull_request {
            Some(pr) => format!("{n}. {} `{}`", pr.url, kid.name),
            None => format!("{n}. `{}` (no pull request yet)", kid.name),
        });
    }
    lines.push(String::new());
    lines
}

/// What `by show` says of a branch's stack: its JSON and one line. `None`
/// when it is in no stack.
pub fn show(yard: &Yard, info: &BranchInfo, st: &PrState) -> Option<(Value, String)> {
    let kids = children(yard, &info.name);
    if st.stack.is_none() && kids.is_empty() && st.restacked.is_none() {
        return None;
    }
    let kid_names: Vec<&str> = kids.iter().map(|(i, _)| i.name.as_str()).collect();
    let mut parts = Vec::new();
    if let Some(stack) = &st.stack {
        let pr = yard
            .branch(&stack.parent)
            .ok()
            .and_then(|b| state(b.info(), &b.events().ok()?).pull_request);
        parts.push(match pr {
            Some(pr) => format!("on {} (pull request #{})", stack.parent, pr.number),
            None => format!("on {} (no pull request yet)", stack.parent),
        });
    }
    if let Some(r) = &st.restacked {
        parts.push(format!(
            "rebased onto {} after {}'s pull request merged",
            r.onto, r.parent
        ));
    }
    if !kid_names.is_empty() {
        parts.push(format!("below {}", kid_names.join(", ")));
    }
    let value = json!({
        "parent": st.stack.as_ref().map(|s| s.parent.clone()),
        "base": st.stack.as_ref().map(|s| s.base.clone()),
        "children": kid_names,
        "restacked": st.restacked,
    });
    Some((value, parts.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_branch_may_be_replaced_only_where_it_was_left() {
        // No record of a push: never.
        assert!(!may_replace(std::path::Path::new("."), "origin", "x", None));
    }
}
