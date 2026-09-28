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
//! # Binding a command
//!
//! A row's `run` is `Run::Background(&[...])` or `Run::Wait(&[...])` (an
//! argv template: `{branch}` and `{text}` are filled in), or
//! `Run::Toggle` for a command with an on and an off form (`t`: `by try`
//! and `by try --off`); `ask`, `when` and `remote` say what it asks, where
//! it applies and whether it works against a server. The test
//! `every_command_row_parses_as_the_command_it_names` checks every argv
//! against `by`'s real command line. `o` ([`Run::Open`]) runs the editor
//! in-process through `open::plan`, so the dashboard can leave its screen
//! for an editor that takes over the terminal.

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
    PrWatch,
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
    /// A checkpoint's turn number (`{text}`), typed under the branch's
    /// checkpoint list ([`PaneKind::Checkpoints`]), then a yes/no question
    /// naming it; `{branch}` and `{text}` are filled in.
    Checkpoint {
        title: &'static str,
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
    /// `by show`'s checkpoint list, shown above `r`'s input box.
    Checkpoints,
    /// `by compare`: the branch beside its siblings ([`siblings`]).
    Compare,
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
    /// `on` (waited for) unless the branch is the one being tried, then
    /// `off`, asking `off_question` instead of the row's question.
    Toggle {
        on: &'static [&'static str],
        off: &'static [&'static str],
        off_question: &'static str,
    },
    /// `by open`: the worktree in the editor `open::plan` picks, run by
    /// the dashboard itself; a terminal editor gets the screen until it
    /// exits.
    Open,
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

fn pushable(info: &BranchInfo) -> Result<(), String> {
    not_running(info)?;
    match (&info.status, &info.candidate) {
        (BranchStatus::Merged { .. }, _) => Err(format!(
            "{} is merged here already; by pr pushes a branch's candidate for review",
            info.name
        )),
        (_, None) => Err(format!("{} has no candidate commit to push", info.name)),
        _ => Ok(()),
    }
}

fn rewindable(info: &BranchInfo) -> Result<(), String> {
    not_running(info)?;
    match info.status {
        BranchStatus::Merged { .. } => Err(format!(
            "{} is merged; fork it at a checkpoint instead (by fork --at)",
            info.name
        )),
        _ => Ok(()),
    }
}

/// What the local-only commands say in remote mode.
const ON_THIS_MACHINE: &str = "the worktree is on the server, not this machine";

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
    Action {
        key: 'p',
        id: ActionId::Pr,
        name: "pr",
        help: "check, push and open or update the pull request (by pr)",
        ask: Ask::Confirm {
            question: "Push {branch}'s candidate and open or update its pull request? Its check \
                       runs on the candidate first; the push and gh use your credentials.",
        },
        run: Run::Wait(&["pr", "--", "{branch}"]),
        when: pushable,
        remote: Remote::No("by pr pushes from this machine's repository with your gh login"),
    },
    Action {
        key: 'P',
        id: ActionId::PrWatch,
        name: "pr watch",
        help: "by pr, then feed CI and reviews back as turns (by pr --watch)",
        ask: Ask::Confirm {
            question: "Push {branch}, open or update its pull request, and keep following it in \
                       the background, sending failed CI checks and review comments into it as \
                       new turns?",
        },
        run: Run::Background(&["pr", "--watch", "--", "{branch}"]),
        when: pushable,
        remote: Remote::No("by pr pushes from this machine's repository with your gh login"),
    },
    Action {
        key: 'o',
        id: ActionId::Open,
        name: "open",
        help: "open the worktree in $VISUAL or $EDITOR (by open)",
        ask: Ask::Nothing,
        run: Run::Open,
        when: always,
        remote: Remote::No(ON_THIS_MACHINE),
    },
    Action {
        key: 'r',
        id: ActionId::Rewind,
        name: "rewind",
        help: "reset the branch to a checkpoint from its list (by rewind)",
        ask: Ask::Checkpoint {
            title: "rewind {branch} to checkpoint",
            question: "Reset {branch} and its worktree to checkpoint {text}? Later checkpoints \
                       stay, so you can rewind forward again.",
        },
        run: Run::Wait(&["rewind", "--yes", "--to", "{text}", "--", "{branch}"]),
        when: rewindable,
        remote: Remote::No(ON_THIS_MACHINE),
    },
    Action {
        key: 'c',
        id: ActionId::Compare,
        name: "compare",
        help: "compare with its siblings, in a pane (by compare)",
        ask: Ask::Nothing,
        run: Run::Pane(PaneKind::Compare),
        when: always,
        remote: Remote::Yes,
    },
    Action {
        key: 't',
        id: ActionId::Try,
        name: "try",
        help: "try its changes in this checkout; again to restore (by try)",
        ask: Ask::Confirm {
            question: "Apply {branch}'s changes to this checkout to try them? A try of another \
                       branch is turned off first; t on {branch} again restores the checkout.",
        },
        run: Run::Toggle {
            on: &["try", "--", "{branch}"],
            off: &["try", "--off"],
            off_question: "Turn off the try of {branch} and restore the checkout as it was?",
        },
        when: has_candidate,
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

/// The branches `c` compares `branch` with, `branch` first: its parent's
/// other children, or for a top-level branch from `by fan` (named
/// `NAME-<harness>`), the others of that fan-out with the same prompt.
/// Just `branch` when it has none.
pub fn siblings(infos: &[BranchInfo], branch: &str) -> Vec<String> {
    let Some(me) = infos.iter().find(|i| i.name == branch) else {
        return vec![branch.to_owned()];
    };
    let fan = |info: &BranchInfo| {
        [&info.harness, &info.profile].iter().find_map(|id| {
            info.name
                .strip_suffix(id.as_str())
                .and_then(|n| n.strip_suffix('-'))
                .filter(|n| !n.is_empty())
                .map(str::to_owned)
        })
    };
    let mine = fan(me);
    let mut out = vec![me.name.clone()];
    for info in infos.iter().filter(|i| i.name != me.name) {
        let sibling = match &me.parent {
            Some(parent) => info.parent.as_ref() == Some(parent),
            None => {
                info.parent.is_none()
                    && mine.is_some()
                    && fan(info) == mine
                    && info.prompt == me.prompt
            }
        };
        if sibling {
            out.push(info.name.clone());
        }
    }
    out
}

/// The argv `action` runs on `branch`: the `off` form of a toggle when
/// `off`. `None` for actions that run no `by`.
pub fn command(action: &Action, branch: &str, text: &str, off: bool) -> Option<Vec<String>> {
    let template = match action.run {
        Run::Background(template) | Run::Wait(template) => template,
        Run::Toggle { off: template, .. } if off => template,
        Run::Toggle { on, .. } => on,
        Run::Pane(_) | Run::Copy(_) | Run::Open => return None,
    };
    Some(argv(template, branch, text))
}

/// A checkpoint number typed into `r`'s box, or why it is not one.
pub fn checkpoint_number(text: &str) -> Result<u32, String> {
    text.trim().parse::<u32>().map_err(|_| {
        format!(
            "{:?} is not a checkpoint: type its turn number (0 is the base)",
            text.trim()
        )
    })
}

/// Why `action` cannot run on `info` now, if it cannot.
pub fn refusal(action: &Action, info: &BranchInfo, remote: bool) -> Option<String> {
    if let (true, Remote::No(reason)) = (remote, action.remote) {
        return Some(format!("{} is local only: {reason}", action.name));
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
        // The keys the commands from the parallel branches are bound to.
        for (key, id) in [
            ('p', ActionId::Pr),
            ('P', ActionId::PrWatch),
            ('o', ActionId::Open),
            ('r', ActionId::Rewind),
            ('c', ActionId::Compare),
            ('t', ActionId::Try),
        ] {
            assert_eq!(by_key(key).unwrap().id, id);
        }
    }

    /// Each command row, filled in, is exactly what typing the command
    /// would parse to, including text that looks like a flag.
    #[test]
    fn every_command_row_parses_as_the_command_it_names() {
        let mut rows = Vec::new();
        for action in ACTIONS {
            let text = match action.ask {
                Ask::Checkpoint { .. } => "3",
                _ => "-v looks like a flag",
            };
            for off in [false, true] {
                if let Some(filled) = command(action, "impl", text, off) {
                    rows.push((action, text, off, filled));
                }
            }
        }
        for (action, text, off, filled) in rows {
            let mut full = vec!["by".to_owned()];
            full.extend(filled);
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
                (ActionId::Pr, Command::Pr { branch, pr }) => {
                    assert_eq!(branch, "impl");
                    assert!(!pr.into_inner().watch);
                }
                (ActionId::PrWatch, Command::Pr { branch, pr }) => {
                    assert_eq!(branch, "impl");
                    assert!(pr.into_inner().watch);
                }
                (
                    ActionId::Rewind,
                    Command::Rewind {
                        branch,
                        to: 3,
                        yes: true,
                        json: false,
                    },
                ) => assert_eq!(branch, "impl"),
                (
                    ActionId::Try,
                    Command::Try {
                        branch,
                        off: o,
                        status: false,
                        force: false,
                        json: false,
                    },
                ) => {
                    assert_eq!(o, off);
                    assert_eq!(branch.as_deref(), (!off).then_some("impl"));
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
        assert_eq!(refused('p', BranchStatus::Ready, true, false), None);
        assert!(refused('p', BranchStatus::Ready, false, false)
            .unwrap()
            .contains("no candidate commit to push"));
        assert!(refused(
            'p',
            BranchStatus::Merged {
                target: "main".into(),
                commit: "c".into()
            },
            true,
            false
        )
        .unwrap()
        .contains("merged here already"));
        assert!(refused('r', BranchStatus::Running, true, false).is_some());
        assert_eq!(refused('r', BranchStatus::Interrupted, false, false), None);
        assert_eq!(refused('c', BranchStatus::Waiting, false, true), None);
        for key in ['p', 'P', 'o', 'r', 't'] {
            assert!(refused(key, BranchStatus::Ready, true, true)
                .unwrap()
                .contains("local only"));
        }
        assert!(refused('t', BranchStatus::Running, false, false)
            .unwrap()
            .contains("no candidate commit"));
    }

    #[test]
    fn siblings_are_the_fan_out_or_the_parent_s_other_children() {
        let named = |name: &str, harness: &str, parent: Option<&str>, prompt: &str| {
            let mut i = info(BranchStatus::Ready, true);
            i.name = name.into();
            i.harness = harness.into();
            i.parent = parent.map(Into::into);
            i.prompt = prompt.into();
            i
        };
        let infos = vec![
            named("speed-codex", "codex", None, "p"),
            named("speed-claude", "claude", None, "p"),
            named("speed-gemini", "gemini", None, "other"),
            named("lone", "codex", None, "p"),
            named("a", "codex", Some("lead"), "x"),
            named("b", "claude", Some("lead"), "y"),
            named("c", "codex", Some("other"), "x"),
        ];
        assert_eq!(
            siblings(&infos, "speed-claude"),
            ["speed-claude", "speed-codex"]
        );
        assert_eq!(siblings(&infos, "b"), ["b", "a"]);
        assert_eq!(siblings(&infos, "lone"), ["lone"]);
        assert_eq!(siblings(&infos, "gone"), ["gone"]);
        assert_eq!(checkpoint_number(" 2 "), Ok(2));
        assert!(checkpoint_number("-1")
            .unwrap_err()
            .contains("not a checkpoint"));
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
