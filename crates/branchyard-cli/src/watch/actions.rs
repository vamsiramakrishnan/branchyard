//! The cockpit's action registry: every key `by watch` binds on the
//! selected branch, in one table ([`ACTIONS`]). Each row says what the key
//! asks first (nothing, a line of text, or a confirmation), what it runs,
//! on which branches it makes sense, and whether it works against a server.
//! The dashboard's key handling, its footer and its `?` sheet all read this
//! table, so a row is the whole of binding a key.
//!
//! Actions that change a branch run the existing `by` command as a child
//! process ([`Run::Background`], [`Run::Wait`]), with the same global flags
//! the dashboard was started with, so in remote mode they go through the
//! server's client exactly as typing the command would.
//!
//! # Binding a new command (integration point)
//!
//! The rows marked `Run::Pending` reserve keys for commands being added in
//! parallel: `p` (`by pr`), `o` (`by open`), `r` (`by rewind`), `c`
//! (`by compare`) and `t` (`by try`). Once the command exists, replace its
//! row's `run` with `Run::Background(&[...])` or `Run::Wait(&[...])` (an
//! argv template: `{branch}` and `{text}` are filled in), set `ask` and
//! `when`, and say whether it works remotely. The test
//! `every_command_row_parses_as_the_command_it_names` then checks the argv
//! against `by`'s real command line.

use branchyard::{BranchInfo, BranchStatus};

use crate::render;

/// Every action, by what it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActionId {
    Send,
    Steer,
    Resume,
    Cancel,
    Merge,
    Fork,
    Diff,
    Log,
    CopyName,
    CopyPath,
    Pr,
    Open,
    Rewind,
    Compare,
    Try,
}

/// What an action asks before it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ask {
    Nothing,
    /// A line of text, which fills `{text}`.
    Text {
        /// The input box's title; `{branch}` is filled in.
        title: &'static str,
    },
    /// A yes/no question; `{branch}` is filled in.
    Confirm {
        question: &'static str,
    },
}

/// A scrollable pane over the dashboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneKind {
    /// `by diff`: the candidate's diff against its base.
    Diff,
    /// `by log`: every recorded event, kept up to date.
    Log,
    /// The output of a command, such as a merge and its check.
    Output,
}

/// What to copy to the clipboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyWhat {
    Name,
    Path,
}

/// What an action runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Run {
    /// `by ARGV...` as a detached background process, for commands that
    /// run a turn: the dashboard carries on, and so does the command if the
    /// dashboard quits. Its output goes to a log file.
    Background(&'static [&'static str]),
    /// `by ARGV...`, waited for off the drawing thread; its output is the
    /// result shown.
    Wait(&'static [&'static str]),
    /// Load a pane.
    Pane(PaneKind),
    /// Copy through the terminal (OSC 52).
    Copy(CopyWhat),
    /// A key reserved for a command not in this build yet, named here.
    Pending(&'static str),
}

/// Whether an action works in remote mode (`by --remote URL watch`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Remote {
    /// Through the server's client, as the command itself does.
    Yes,
    /// Not against a server, for this reason.
    No(&'static str),
}

/// Whether an action applies to a branch: `Err` says why not.
pub type When = fn(&BranchInfo) -> Result<(), String>;

/// One key's binding.
#[derive(Clone, Copy, Debug)]
pub struct Action {
    pub key: char,
    pub id: ActionId,
    /// A word for the footer and the result line.
    pub name: &'static str,
    /// One line for the `?` sheet.
    pub help: &'static str,
    pub ask: Ask,
    pub run: Run,
    pub when: When,
    pub remote: Remote,
}

/// What `R` sends to an interrupted branch. `by send` resumes the branch's
/// native session, so the harness still has the conversation.
pub const RESUME_PROMPT: &str = "Your previous turn was interrupted before it finished. \
     Continue the task from where you left off, then summarize what you did.";

fn always(_: &BranchInfo) -> Result<(), String> {
    Ok(())
}

fn status(info: &BranchInfo) -> String {
    render::status_text(&info.status).0
}

fn not_running(info: &BranchInfo) -> Result<(), String> {
    match info.status {
        BranchStatus::Running => Err(format!(
            "{} has a turn running; S steers it, x cancels it",
            info.name
        )),
        BranchStatus::Waiting => Err(format!(
            "{} is waiting for its prerequisites and has no session yet",
            info.name
        )),
        _ => Ok(()),
    }
}

fn running(info: &BranchInfo) -> Result<(), String> {
    match info.status {
        BranchStatus::Running => Ok(()),
        _ => Err(format!(
            "{} has no turn running ({}); s sends a new prompt",
            info.name,
            status(info)
        )),
    }
}

fn interrupted(info: &BranchInfo) -> Result<(), String> {
    match info.status {
        BranchStatus::Interrupted => Ok(()),
        _ => Err(format!(
            "only an interrupted branch resumes; {} is {} (s sends a new prompt)",
            info.name,
            status(info)
        )),
    }
}

fn cancellable(info: &BranchInfo) -> Result<(), String> {
    match info.status {
        BranchStatus::Running | BranchStatus::Waiting => Ok(()),
        _ => Err(format!(
            "nothing to cancel: {} is {}",
            info.name,
            status(info)
        )),
    }
}

fn ready(info: &BranchInfo) -> Result<(), String> {
    match (&info.status, &info.candidate) {
        (BranchStatus::Ready, Some(_)) => Ok(()),
        _ => Err(format!(
            "only a ready branch merges; {} is {}",
            info.name,
            status(info)
        )),
    }
}

fn forkable(info: &BranchInfo) -> Result<(), String> {
    match info.status {
        BranchStatus::Waiting | BranchStatus::Blocked { .. } => Err(format!(
            "{} has not started, so there is nothing to fork",
            info.name
        )),
        _ => Ok(()),
    }
}

fn has_candidate(info: &BranchInfo) -> Result<(), String> {
    match info.candidate {
        Some(_) => Ok(()),
        None => Err(format!("{} has no candidate commit yet", info.name)),
    }
}

/// The registry. Order is the `?` sheet's order.
pub const ACTIONS: &[Action] = &[
    Action {
        key: 's',
        id: ActionId::Send,
        name: "send",
        help: "send a follow-up prompt (a new turn; by send)",
        ask: Ask::Text {
            title: "send to {branch}",
        },
        run: Run::Background(&["send", "--", "{branch}", "{text}"]),
        when: not_running,
        remote: Remote::Yes,
    },
    Action {
        key: 'S',
        id: ActionId::Steer,
        name: "steer",
        help: "add to the running turn without stopping it (by send --steer)",
        ask: Ask::Text {
            title: "steer {branch}'s running turn",
        },
        run: Run::Wait(&["send", "--steer", "--", "{branch}", "{text}"]),
        when: running,
        remote: Remote::Yes,
    },
    Action {
        key: 'R',
        id: ActionId::Resume,
        name: "resume",
        help: "resume an interrupted branch in its own session",
        ask: Ask::Nothing,
        run: Run::Background(&["send", "--", "{branch}", RESUME_PROMPT]),
        when: interrupted,
        remote: Remote::Yes,
    },
    Action {
        key: 'x',
        id: ActionId::Cancel,
        name: "cancel",
        help: "cancel the running turn and its descendants' (by cancel)",
        ask: Ask::Confirm {
            question: "Cancel {branch}'s running turn, and those of every branch it delegated to?",
        },
        run: Run::Wait(&["cancel", "--", "{branch}"]),
        when: cancellable,
        remote: Remote::Yes,
    },
    Action {
        key: 'm',
        id: ActionId::Merge,
        name: "merge",
        help: "merge the candidate into the checked-out branch after its check (by merge)",
        ask: Ask::Confirm {
            question: "Merge {branch} into the checked-out branch? Its check runs on the merge \
                       result first, and nothing changes unless it passes.",
        },
        run: Run::Wait(&["merge", "--", "{branch}"]),
        when: ready,
        remote: Remote::Yes,
    },
    Action {
        key: 'f',
        id: ActionId::Fork,
        name: "fork",
        help: "fork a new branch from this one's candidate and conversation (by fork)",
        ask: Ask::Text {
            title: "fork {branch} with the prompt",
        },
        run: Run::Background(&["fork", "--", "{branch}", "{text}"]),
        when: forkable,
        remote: Remote::Yes,
    },
    Action {
        key: 'd',
        id: ActionId::Diff,
        name: "diff",
        help: "the candidate's diff, in a scrollable pane (by diff)",
        ask: Ask::Nothing,
        run: Run::Pane(PaneKind::Diff),
        when: has_candidate,
        remote: Remote::Yes,
    },
    Action {
        key: 'l',
        id: ActionId::Log,
        name: "log",
        help: "every recorded event, following new ones (by log)",
        ask: Ask::Nothing,
        run: Run::Pane(PaneKind::Log),
        when: always,
        remote: Remote::Yes,
    },
    Action {
        key: 'y',
        id: ActionId::CopyName,
        name: "copy name",
        help: "copy the branch's name to the clipboard (OSC 52)",
        ask: Ask::Nothing,
        run: Run::Copy(CopyWhat::Name),
        when: always,
        remote: Remote::Yes,
    },
    Action {
        key: 'Y',
        id: ActionId::CopyPath,
        name: "copy path",
        help: "copy the worktree's path to the clipboard (OSC 52)",
        ask: Ask::Nothing,
        run: Run::Copy(CopyWhat::Path),
        when: always,
        remote: Remote::No("the worktree is on the server, not this machine"),
    },
    // Reserved for commands being added in parallel; see the module
    // documentation for how to bind them.
    Action {
        key: 'p',
        id: ActionId::Pr,
        name: "pr",
        help: "push and open or update a pull request",
        ask: Ask::Nothing,
        run: Run::Pending("by pr"),
        when: always,
        remote: Remote::Yes,
    },
    Action {
        key: 'o',
        id: ActionId::Open,
        name: "open",
        help: "open the worktree in an editor",
        ask: Ask::Nothing,
        run: Run::Pending("by open"),
        when: always,
        remote: Remote::No("the worktree is on the server, not this machine"),
    },
    Action {
        key: 'r',
        id: ActionId::Rewind,
        name: "rewind",
        help: "rewind the branch to an earlier turn",
        ask: Ask::Nothing,
        run: Run::Pending("by rewind"),
        when: always,
        remote: Remote::Yes,
    },
    Action {
        key: 'c',
        id: ActionId::Compare,
        name: "compare",
        help: "compare this branch with its fan-out siblings",
        ask: Ask::Nothing,
        run: Run::Pending("by compare"),
        when: always,
        remote: Remote::Yes,
    },
    Action {
        key: 't',
        id: ActionId::Try,
        name: "try",
        help: "try the branch in the main checkout",
        ask: Ask::Nothing,
        run: Run::Pending("by try"),
        when: always,
        remote: Remote::No("it changes the checkout on this machine"),
    },
];

/// The action bound to `key`.
pub fn by_key(key: char) -> Option<&'static Action> {
    ACTIONS.iter().find(|action| action.key == key)
}

/// The action with this ID.
pub fn by_id(id: ActionId) -> &'static Action {
    ACTIONS
        .iter()
        .find(|action| action.id == id)
        .expect("every ActionId has a row in ACTIONS")
}

/// `template` with `{branch}` and `{text}` filled in, one argument each.
pub fn argv(template: &[&str], branch: &str, text: &str) -> Vec<String> {
    template
        .iter()
        .map(|arg| match *arg {
            "{branch}" => branch.to_owned(),
            "{text}" => text.to_owned(),
            other => other.to_owned(),
        })
        .collect()
}

/// `{branch}` in a title or question.
pub fn fill(text: &str, branch: &str) -> String {
    text.replace("{branch}", branch)
}

/// Why `action` cannot run on `info` now, if it cannot.
pub fn refusal(action: &Action, info: &BranchInfo, remote: bool) -> Option<String> {
    if let (true, Remote::No(reason)) = (remote, action.remote) {
        return Some(format!("{} is local only: {reason}", action.name));
    }
    if let Run::Pending(command) = action.run {
        return Some(format!(
            "{} {}: `{command}` is not in this build yet; this key is reserved for it",
            action.key, action.name
        ));
    }
    (action.when)(info).err()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{self, Command};
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn info(status: BranchStatus, candidate: bool) -> BranchInfo {
        BranchInfo {
            name: "impl".into(),
            git_branch: "by/impl".into(),
            worktree: PathBuf::from("/w"),
            prompt: "p".into(),
            harness: "codex".into(),
            profile: "codex-app-server".into(),
            session: None,
            parent: None,
            children: Vec::new(),
            depth: 0,
            base: "b".into(),
            candidate: candidate.then(|| branchyard::CandidateInfo {
                commit: "c".repeat(40),
                files_changed: 1,
                insertions: 2,
                deletions: 3,
            }),
            status,
            turns: 1,
            cost_usd: None,
            created_at: 0,
            stalled: false,
            superseded_by: None,
        }
    }

    #[test]
    fn keys_and_ids_are_unique_and_do_not_shadow_navigation() {
        let keys: HashSet<char> = ACTIONS.iter().map(|a| a.key).collect();
        assert_eq!(keys.len(), ACTIONS.len());
        let ids: HashSet<ActionId> = ACTIONS.iter().map(|a| a.id).collect();
        assert_eq!(ids.len(), ACTIONS.len());
        for navigation in ['j', 'k', 'g', 'G', 'h', 'q', '/', '?'] {
            assert!(by_key(navigation).is_none(), "{navigation} is navigation");
        }
        for id in ids {
            assert_eq!(by_id(id).id, id);
        }
        // The slots the parallel commands will fill.
        for key in ['p', 'o', 'r', 'c', 't'] {
            assert!(matches!(by_key(key).unwrap().run, Run::Pending(_)), "{key}");
        }
    }

    /// Each command row, filled in, is exactly what typing the command
    /// would parse to, including text that looks like a flag.
    #[test]
    fn every_command_row_parses_as_the_command_it_names() {
        for action in ACTIONS {
            let template = match action.run {
                Run::Background(argv) | Run::Wait(argv) => argv,
                _ => continue,
            };
            let text = "-v looks like a flag";
            let mut full = vec!["by".to_owned()];
            full.extend(argv(template, "impl", text));
            let cli = args::parse_from(&full)
                .unwrap_or_else(|e| panic!("{}: {full:?}: {e}", action.name));
            let command = cli.command.unwrap();
            match (action.id, command) {
                (
                    ActionId::Send,
                    Command::Send {
                        branch,
                        prompt,
                        steer: false,
                        ..
                    },
                ) => assert_eq!((branch.as_str(), prompt.as_str()), ("impl", text)),
                (
                    ActionId::Resume,
                    Command::Send {
                        branch,
                        prompt,
                        steer: false,
                        ..
                    },
                ) => assert_eq!((branch.as_str(), prompt.as_str()), ("impl", RESUME_PROMPT)),
                (
                    ActionId::Steer,
                    Command::Send {
                        branch,
                        prompt,
                        steer: true,
                        ..
                    },
                ) => assert_eq!((branch.as_str(), prompt.as_str()), ("impl", text)),
                (
                    ActionId::Cancel,
                    Command::Cancel {
                        branch,
                        json: false,
                    },
                ) => {
                    assert_eq!(branch, "impl")
                }
                (ActionId::Merge, Command::Merge { branch, into: None }) => {
                    assert_eq!(branch, "impl")
                }
                (ActionId::Fork, Command::Fork { branch, prompt, .. }) => {
                    assert_eq!((branch.as_str(), prompt.as_str()), ("impl", text))
                }
                (id, command) => panic!("{id:?} parsed as {command:?}"),
            }
        }
    }

    #[test]
    fn actions_apply_only_where_they_make_sense() {
        let refused = |key: char, status: BranchStatus, candidate: bool, remote: bool| {
            refusal(by_key(key).unwrap(), &info(status, candidate), remote)
        };
        assert_eq!(refused('s', BranchStatus::Ready, true, false), None);
        assert!(refused('s', BranchStatus::Running, false, false)
            .unwrap()
            .contains("S steers it"));
        assert_eq!(refused('S', BranchStatus::Running, false, false), None);
        assert!(refused('S', BranchStatus::Ready, false, false).is_some());
        assert_eq!(refused('R', BranchStatus::Interrupted, false, false), None);
        assert!(refused('R', BranchStatus::Ready, true, false)
            .unwrap()
            .contains("only an interrupted branch resumes"));
        assert_eq!(refused('m', BranchStatus::Ready, true, false), None);
        assert!(refused('m', BranchStatus::NoChanges, false, false).is_some());
        assert_eq!(refused('x', BranchStatus::Running, false, false), None);
        assert!(refused('x', BranchStatus::Ready, false, false).is_some());
        assert!(refused('d', BranchStatus::Running, false, false).is_some());
        assert_eq!(refused('d', BranchStatus::Running, true, false), None);
        assert_eq!(refused('Y', BranchStatus::Ready, false, false), None);
        assert!(refused('Y', BranchStatus::Ready, false, true)
            .unwrap()
            .contains("local only"));
        assert!(refused('p', BranchStatus::Ready, true, false)
            .unwrap()
            .contains("`by pr` is not in this build yet"));
    }

    #[test]
    fn templates_fill_one_argument_each() {
        assert_eq!(
            argv(&["send", "--", "{branch}", "{text}"], "a b", "x {branch} y"),
            ["send", "--", "a b", "x {branch} y"]
        );
        assert_eq!(fill("send to {branch}", "impl"), "send to impl");
    }
}
