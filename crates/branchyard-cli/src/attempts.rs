//! Checkpoints, `by rewind`, `by try` and `by compare`: choosing among
//! attempts and moving a branch through its turns. Each command is a thin
//! call into the SDK; the rendering helpers are public so `by watch` can
//! reuse them.

use std::io::{self, BufRead, Write};

use branchyard::{
    Activity, Attempt, Branch, BranchInfo, BranchStatus, Checkpoint, CheckpointEntry, Checkpoints,
    RemoveOptions, TryState,
};
use branchyard_client::Repo;
use serde_json::json;

use crate::commands::{self, open, print, Env, Failure, Outcome, Target};
use crate::json;
use crate::render::{self, Cell, Column, Style, Tone};

fn style(env: &Env) -> Style {
    Style { color: env.color }
}

fn short(commit: &str) -> &str {
    commit.get(..10).unwrap_or(commit)
}

/// Ask `question` on the terminal; true only for an explicit yes. Without a
/// terminal nothing is asked and the answer is no.
pub fn confirm(env: &Env, question: &str) -> bool {
    if !(env.stdin_tty && env.stderr_tty) {
        return false;
    }
    eprint!("{question} [y/N] ");
    branchyard_support::best_effort("flush the prompt", io::stderr().flush());
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn local_only(target: &Target, what: &str) -> Result<(), Failure> {
    match target {
        Target::Local => Ok(()),
        Target::Remote(_) => Err(crate::errors::Report::usage(format!(
            "{what} is not available in remote mode"
        ))
        .next("run it on the server's host, without --remote")
        .into()),
    }
}

/// A branch's checkpoints: from the SDK locally, from its events remotely
/// (where whether each ref still exists is not known, and is shown as
/// available).
pub fn checkpoints(target: &Target, info: &BranchInfo) -> Result<Checkpoints, Failure> {
    match target {
        Target::Local => Ok(open()?.branch(&info.name)?.checkpoints()?),
        Target::Remote(remote) => remote_checkpoints(&remote.repo, info),
    }
}

/// [`checkpoints`] from the server's records of the branch.
pub fn remote_checkpoints(repo: &Repo, info: &BranchInfo) -> Result<Checkpoints, Failure> {
    let events = repo.events(&info.name, 0)?.events;
    let current = events.iter().rev().find_map(|e| match &e.activity {
        Activity::Checkpoint(c) => Some(c.turn),
        Activity::Rewound { to, .. } => Some(*to),
        _ => None,
    });
    Ok(Checkpoints {
        branch: info.name.clone(),
        base: info.base.clone(),
        current: current.or(Some(0)),
        checkpoints: branchyard::checkpoint_entries(&events),
    })
}

/// The `checkpoints` section of `by show`.
pub fn checkpoint_lines(list: &Checkpoints, style: Style) -> String {
    let mut out = format!("\n{}\n", style.paint(Tone::Dim, "checkpoints"));
    let mark = |turn: u32| match list.current == Some(turn) {
        true => style.paint(Tone::Green, "*"),
        false => " ".to_owned(),
    };
    out.push_str(&format!("  {} 0  {}  base\n", mark(0), short(&list.base)));
    for entry in &list.checkpoints {
        out.push_str(&format!(
            "  {} {}\n",
            mark(entry.checkpoint.turn),
            entry_text(entry, style)
        ));
    }
    if list.current.is_none() {
        out.push_str("  (the branch is not at a recorded checkpoint)\n");
    }
    out
}

fn entry_text(entry: &CheckpointEntry, style: Style) -> String {
    let c: &Checkpoint = &entry.checkpoint;
    let after = match c.after {
        Some(after) if after + 1 != c.turn => format!(", after {after}"),
        _ => String::new(),
    };
    let prompt = entry
        .prompt
        .as_deref()
        .map(|p| {
            format!(
                "  {}",
                render::truncate(&p.split_whitespace().collect::<Vec<_>>().join(" "), 60)
            )
        })
        .unwrap_or_default();
    let gone = match entry.available {
        true => String::new(),
        false => style.paint(Tone::Red, "  (ref missing)"),
    };
    format!(
        "{}  {}  {} file(s) +{} -{}{after}{prompt}{gone}",
        c.turn,
        short(&c.commit),
        c.files_changed,
        c.insertions,
        c.deletions
    )
}

/// After `by fork --at`: how the new branch's session continues.
pub fn announce_fork(forked: &Branch) {
    if let Ok(events) = forked.events() {
        announce_forked_at(&forked.info().name, &events);
    }
}

/// [`announce_fork`] from the new branch `name`'s recorded events, as
/// remote mode reads them from the server.
pub fn announce_forked_at(name: &str, events: &[branchyard::RecordedEvent]) {
    if let Some(line) = forked_at_line(name, events) {
        eprintln!("{line}");
    }
}

/// The line [`announce_forked_at`] prints, if the events hold a fork at a
/// checkpoint.
fn forked_at_line(name: &str, events: &[branchyard::RecordedEvent]) -> Option<String> {
    events.iter().find_map(|event| match &event.activity {
        Activity::ForkedAt {
            branch,
            turn,
            session,
            ..
        } => Some(format!(
            "by: forked from {branch} at checkpoint {turn}; {name} {}",
            session.describe()
        )),
        _ => None,
    })
}

pub fn rewind(
    env: &Env,
    target: &Target,
    branch: &str,
    to: u32,
    yes: bool,
    as_json: bool,
) -> Outcome {
    let Target::Remote(remote) = target else {
        return rewind_in(env, &open()?, branch, to, yes, as_json);
    };
    // The server's rewind: the same note, question and output.
    let info = remote.repo.branch(branch)?;
    let note = remote
        .repo
        .undo_plan(branch, to)
        .ok()
        .and_then(|plan| crate::effects_cmd::rewind_note_for(&plan, branch, to));
    let asked = Rewind {
        branch,
        to,
        yes,
        as_json,
    };
    asked.run(
        env,
        note,
        || checkpoints(target, &info),
        || Ok(remote.repo.rewind(branch, to)?),
    )
}

/// `by rewind` in `yard`, as `by task rewind` runs it in a task's own
/// repository.
pub(crate) fn rewind_in(
    env: &Env,
    yard: &branchyard::Yard,
    branch: &str,
    to: u32,
    yes: bool,
    as_json: bool,
) -> Outcome {
    let handle = yard.branch(branch)?;
    // Rewinding past effectful turns leaves the world as it is: say so,
    // with what `by undo` could do (docs/effects.md).
    let note = crate::effects_cmd::rewind_note(yard, branch, to);
    let asked = Rewind {
        branch,
        to,
        yes,
        as_json,
    };
    asked.run(
        env,
        note,
        || Ok(handle.checkpoints()?),
        || Ok(handle.rewind(to)?),
    )
}

/// A `by rewind` as it was asked for, locally or remotely.
struct Rewind<'a> {
    branch: &'a str,
    to: u32,
    yes: bool,
    as_json: bool,
}

impl Rewind<'_> {
    /// Say `note`, ask unless `--yes` (listing the checkpoints `list`
    /// gives), rewind with `act` and print what it did.
    fn run(
        &self,
        env: &Env,
        note: Option<String>,
        list: impl FnOnce() -> Result<Checkpoints, Failure>,
        act: impl FnOnce() -> Result<branchyard::Rewound, Failure>,
    ) -> Outcome {
        let Rewind {
            branch,
            to,
            yes,
            as_json,
        } = *self;
        if let Some(note) = note {
            eprint!("{note}");
        }
        if !yes {
            let list = list()?;
            if !confirm(env, &rewind_question(&list, branch, to)) {
                return Err(crate::errors::Report::usage(format!(
                    "rewinding {branch} resets its worktree, and nothing confirmed it"
                ))
                .next("pass --yes to confirm, or run it on a terminal")
                .into());
            }
        }
        let rewound = act()?;
        if as_json {
            return print(&json::text(
                &serde_json::to_value(&rewound).unwrap_or_default(),
            ));
        }
        let from = rewound
            .from
            .map(|n| format!(" from {n}"))
            .unwrap_or_default();
        print(&format!(
            "rewound {branch}{from} to checkpoint {to} ({})\nthe next turn {}\n",
            short(&rewound.commit),
            rewound.session.describe()
        ))
    }
}

/// What `by rewind` asks before it resets `branch` to `to`.
fn rewind_question(list: &Checkpoints, branch: &str, to: u32) -> String {
    let later: Vec<String> = list
        .checkpoints
        .iter()
        .filter(|e| e.checkpoint.turn > to)
        .map(|e| e.checkpoint.turn.to_string())
        .collect();
    format!(
        "Reset {branch} and its worktree to checkpoint {to}?{}",
        match later.is_empty() {
            true => String::new(),
            false => format!(
                " Checkpoints {} stay, so you can rewind forward again.",
                later.join(", ")
            ),
        }
    )
}

/// `by try`, `by try --off` and `by try --status`.
pub fn try_branch(
    _env: &Env,
    target: &Target,
    branch: Option<&str>,
    off: bool,
    status: bool,
    force: bool,
    as_json: bool,
) -> Outcome {
    local_only(target, "by try")?;
    let yard = open()?;
    if let Some(note) = yard.try_recover()? {
        eprintln!("by: {note}");
    }
    if status {
        let state = yard.try_status()?;
        if as_json {
            return print(&json::text(
                &serde_json::to_value(&state).unwrap_or_default(),
            ));
        }
        return print(&match state {
            None => "nothing is being tried\n".to_owned(),
            Some(state) => try_text("trying", &state),
        });
    }
    if off {
        let state = yard.try_off(force)?;
        if as_json {
            return print(&json::text(&json!({ "restored": state })));
        }
        return print(&match state {
            None => "nothing was being tried\n".to_owned(),
            Some(state) => format!(
                "restored the checkout: {} file(s) {} had changed are as they were\n",
                state.files.len(),
                state.branch
            ),
        });
    }
    let Some(branch) = branch else {
        return Err(
            crate::errors::Report::usage("by try needs a branch, --off or --status")
                .next("by try BRANCH (by ls lists them)")
                .into(),
        );
    };
    let previous = yard.try_status()?;
    let state = yard.try_on(branch)?;
    if as_json {
        return print(&json::text(
            &serde_json::to_value(&state).unwrap_or_default(),
        ));
    }
    let mut text = String::new();
    if let Some(previous) = previous.filter(|p| p.branch != state.branch) {
        text.push_str(&format!("turned off the try of {}\n", previous.branch));
    }
    text.push_str(&try_text("trying", &state));
    text.push_str("turn it off with: by try --off\n");
    print(&text)
}

/// A try in a few lines.
pub fn try_text(verb: &str, state: &TryState) -> String {
    let mut text = format!(
        "{verb} {} ({}) in this checkout: {} file(s)\n",
        state.branch,
        short(&state.commit),
        state.files.len()
    );
    for file in &state.files {
        let what = match (&file.before, &file.after) {
            (None, _) => "added",
            (Some(_), None) if state.phase == "applied" => "deleted",
            _ => "changed",
        };
        text.push_str(&format!("  {what:<8} {}\n", file.path));
    }
    text
}

/// What `by compare` was asked.
pub struct CompareArgs {
    pub branches: Vec<String>,
    pub fan: Option<String>,
    pub check: bool,
    pub diff: Option<Vec<String>>,
    pub pick: Option<String>,
    pub into: Option<String>,
    pub discard_others: bool,
    pub yes: bool,
    pub json: bool,
}

/// The attempts: locally from the SDK, remotely from the server's records,
/// events and diffs (no checks run there).
fn gather(target: &Target, args: &CompareArgs) -> Result<Vec<Attempt>, Failure> {
    match target {
        Target::Local => {
            let yard = open()?;
            let names = match &args.fan {
                Some(fan) => yard.fan_branches(fan)?,
                None => args.branches.clone(),
            };
            Ok(yard.compare(&names, args.check)?)
        }
        Target::Remote(remote) => {
            if args.check {
                return Err(crate::errors::Report::usage(
                    "compare --check runs checks locally; it is not available in remote mode",
                )
                .next("drop --check, or run by compare --check on the server's host")
                .into());
            }
            let infos = remote.repo.branches()?;
            let names: Vec<String> = match &args.fan {
                Some(fan) => {
                    let found: Vec<&BranchInfo> = infos
                        .iter()
                        .filter(|i| i.depth == 0 && i.parent.is_none())
                        .filter(|i| {
                            i.name == format!("{fan}-{}", i.harness)
                                || i.name == format!("{fan}-{}", i.profile)
                        })
                        .collect();
                    let prompt = found.first().map(|i| i.prompt.clone());
                    found
                        .into_iter()
                        .filter(|i| Some(&i.prompt) == prompt.as_ref())
                        .map(|i| i.name.clone())
                        .collect()
                }
                None => args.branches.clone(),
            };
            if names.is_empty() {
                return Err(crate::errors::Report::new(
                    "unknown_branch",
                    format!(
                        "no branches named {}-<harness> from one by fan",
                        args.fan.as_deref().unwrap_or_default()
                    ),
                )
                .next("by ls")
                .into());
            }
            remote_attempts(&remote.repo, &names)
        }
    }
}

/// Attempts on a server, from its records, events and diffs (no checks
/// run there); `by watch`'s `c` uses it too.
pub fn remote_attempts(repo: &Repo, names: &[String]) -> Result<Vec<Attempt>, Failure> {
    let infos = names
        .iter()
        .map(|name| repo.branch(name))
        .collect::<Result<Vec<_>, _>>()?;
    remote_attempts_for_infos(repo, &infos)
}

/// Read remote output against the same branch snapshots the caller renders.
/// A concurrently changed contracted branch remains incomplete.
pub(crate) fn remote_attempts_for_infos(
    repo: &Repo,
    infos: &[BranchInfo],
) -> Result<Vec<Attempt>, Failure> {
    let mut attempts = Vec::new();
    for info in infos {
        let name = &info.name;
        let events = repo.events(name, 0)?.events;
        let files = match &info.candidate {
            Some(_) => branchyard::diff_files(&repo.diff(name)?),
            None => Vec::new(),
        };
        let inspection = repo.inspect(name)?;
        attempts.push(remote_result_attempt(info, &events, files, inspection));
    }
    branchyard::mark_unique(&mut attempts);
    Ok(attempts)
}

/// Assess the log only when its accepted result agrees with the later inspection.
fn remote_result_attempt(
    info: &BranchInfo,
    events: &[branchyard::RecordedEvent],
    files: Vec<String>,
    inspection: branchyard::Inspection,
) -> Attempt {
    let changed = info.turns != inspection.turns
        || info.status != inspection.status
        || info.candidate != inspection.candidate;
    let mut attempt = branchyard::compare_attempt(info, events, files)
        .with_result(inspection.result_contract, events);
    if attempt.result_contract.is_some()
        && (changed
            || attempt
                .result
                .as_ref()
                .is_some_and(|result| Some(result) != inspection.result.as_ref()))
    {
        attempt.result = None;
        attempt.result_event = None;
        attempt.result_error = Some(
            "branch changed while its structured result was inspected; inspect it again".into(),
        );
    }
    if attempt.is_deliverable() {
        attempt.check = match inspection.check {
            Some(_) => branchyard::AttemptCheck::NotRun,
            None => branchyard::AttemptCheck::None,
        };
    }
    attempt
}

pub fn compare(env: &Env, target: &Target, args: &CompareArgs) -> Outcome {
    if let Some(pair) = &args.diff {
        local_only(target, "compare --diff")?;
        let diff = open()?.diff_between(&pair[0], &pair[1])?;
        return print(&diff);
    }
    let attempts = gather(target, args)?;
    if attempts.is_empty() {
        return Err(crate::errors::Report::usage("nothing to compare")
            .next("by compare BRANCH BRANCH..., or --fan NAME")
            .into());
    }
    if let Some(pick) = &args.pick {
        if let Some(attempt) = attempts.iter().find(|attempt| &attempt.branch == pick) {
            check_git_pick(attempt)?;
        }
        if !attempts.iter().any(|a| &a.branch == pick) {
            let names: Vec<&str> = attempts.iter().map(|a| a.branch.as_str()).collect();
            return Err(crate::errors::Report::usage(format!(
                "{pick} is not one of the compared attempts: {}",
                names.join(", ")
            ))
            .next(format!(
                "pick one of them: --pick {}",
                names.first().copied().unwrap_or_default()
            ))
            .into());
        }
    }
    if args.json && args.pick.is_none() {
        return print(&json::text(
            &serde_json::to_value(&attempts).unwrap_or_default(),
        ));
    }
    if !args.json {
        print(&compare_table(&attempts, style(env)))?;
    }
    let Some(pick) = &args.pick else {
        return Ok(());
    };
    let names: Vec<String> = attempts.iter().map(|a| a.branch.clone()).collect();
    let removed = pick_and_discard(env, target, &names, pick, args)?;
    if args.json {
        return print(&json::text(&json!({
            "attempts": attempts,
            "picked": pick,
            "removed": removed,
        })));
    }
    Ok(())
}

/// Keep result selection separate from a Git merge and loser removal.
pub(crate) fn check_git_pick(attempt: &Attempt) -> Result<(), Failure> {
    if attempt.is_deliverable() {
        return Err(crate::errors::Report::new(
            "unsupported",
            "a structured deliverable is selected as a result; --pick merges Git changes",
        )
        .next(format!("by inspect {} --json", attempt.branch))
        .into());
    }
    Ok(())
}

/// Merge `pick` through the validated merge `by merge` uses, then, with
/// `--discard-others`, remove the rest of `names` after asking (or with
/// `--yes`). Returns what was removed. `by compare --pick` and `by judge
/// --pick` both end here.
pub fn pick_and_discard(
    env: &Env,
    target: &Target,
    names: &[String],
    pick: &str,
    args: &CompareArgs,
) -> Result<Vec<String>, Failure> {
    // The existing validated merge: the pick's check runs on the exact
    // merge result, and the target moves only by compare-and-swap.
    match args.json {
        true => commands::merge_quietly(target, pick, args.into.as_deref())?,
        false => commands::merge(target, pick, args.into.as_deref(), false)?,
    }
    let others: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|b| *b != pick)
        .collect();
    let mut removed = Vec::new();
    if args.discard_others && !others.is_empty() {
        let question = format!("Remove the other attempts ({})?", others.join(", "));
        if !args.yes && !confirm(env, &question) {
            return Err(Failure::Message(format!(
                "merged {pick}; kept {} (pass --yes to remove them without asking)",
                others.join(", ")
            )));
        }
        for other in &others {
            match target {
                Target::Local => open()?.remove_with(other, &RemoveOptions::default())?,
                Target::Remote(remote) => remote.repo.remove(other)?,
            }
            removed.push(other.to_string());
            if !args.json {
                print(&format!("removed {other}\n"))?;
            }
        }
    }
    Ok(removed)
}

pub fn duration_text(ms: Option<u64>) -> String {
    match ms {
        None => "-".into(),
        Some(ms) if ms < 1_000 => format!("{ms}ms"),
        Some(ms) => render::age_text(ms / 1000),
    }
}

/// `by compare`'s table: one row per attempt.
pub fn compare_table(attempts: &[Attempt], style: Style) -> String {
    let column = |header, max, right| Column { header, max, right };
    let columns = [
        column("BRANCH", 40, false),
        column("STATUS", 24, false),
        column("TURNS", 5, true),
        column("COST", 9, true),
        column("TOKENS", 8, true),
        column("TIME", 8, true),
        column("CHECK", 9, false),
        column("FILES", 5, true),
        column("+/-", 13, true),
        column("UNIQUE FILES", 40, false),
    ];
    let rows: Vec<Vec<Cell>> = attempts
        .iter()
        .map(|a| {
            let (status, tone) = render::status_text(&a.status);
            let check_tone = match a.check.word() {
                "passed" => Some(Tone::Green),
                "failed" | "timed out" | "error" => Some(Tone::Red),
                _ => None,
            };
            let unique = match a.unique_files.len() {
                0 => "-".to_owned(),
                1..=2 => a.unique_files.join(", "),
                n => format!(
                    "{}, {} +{} more",
                    a.unique_files[0],
                    a.unique_files[1],
                    n - 2
                ),
            };
            vec![
                Cell::plain(&a.branch),
                Cell::toned(status, tone),
                Cell::plain(a.turns.to_string()),
                Cell::plain(render::cost_text(a.cost_usd)),
                Cell::plain(a.tokens.map_or_else(|| "-".into(), render::tokens)),
                Cell::plain(duration_text(a.duration_ms)),
                Cell {
                    text: a.check.word().to_owned(),
                    tone: check_tone,
                },
                Cell::plain(a.files_changed.to_string()),
                Cell::plain(format!("+{} -{}", a.insertions, a.deletions)),
                Cell::plain(unique),
            ]
        })
        .collect();
    let mut text = render::table(&columns, &rows, style);
    let ready: Vec<&str> = attempts
        .iter()
        .filter(|a| a.status == BranchStatus::Ready)
        .map(|a| a.branch.as_str())
        .collect();
    if ready.len() > 1 {
        text.push_str(&format!(
            "\n{}\n  by compare {} --diff {} {}\n  by try {}\n  by compare {} --pick <branch>\n",
            style.paint(Tone::Dim, "next"),
            ready.join(" "),
            ready[0],
            ready[1],
            ready[0],
            ready.join(" ")
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard::{AttemptCheck, RecordedEvent, SessionContinuity};

    #[test]
    fn a_fork_at_a_checkpoint_is_announced_from_its_events() {
        let session = SessionContinuity::Fresh {
            reason: "nothing ran before the base".into(),
        };
        let events = vec![
            RecordedEvent {
                at_ms: 1,
                activity: Activity::Prompt("x".into()),
            },
            RecordedEvent {
                at_ms: 2,
                activity: Activity::ForkedAt {
                    branch: "hello".into(),
                    turn: 0,
                    commit: "abc".into(),
                    session: session.clone(),
                },
            },
        ];
        assert_eq!(
            forked_at_line("hello-2", &events),
            Some(format!(
                "by: forked from hello at checkpoint 0; hello-2 {}",
                session.describe()
            ))
        );
        assert_eq!(forked_at_line("hello-2", &events[..1]), None);
    }

    fn attempt(name: &str, unique: &[&str]) -> Attempt {
        Attempt {
            branch: name.into(),
            harness: "codex".into(),
            status: BranchStatus::Ready,
            turns: 2,
            cost_usd: Some(0.5),
            tokens: Some(12_300),
            duration_ms: Some(95_000),
            check: AttemptCheck::Passed,
            candidate: Some("0123456789abcdef".into()),
            files_changed: 3,
            insertions: 10,
            deletions: 2,
            files: unique.iter().map(|s| s.to_string()).collect(),
            unique_files: unique.iter().map(|s| s.to_string()).collect(),
            result_contract: None,
            result: None,
            result_event: None,
            result_error: None,
        }
    }

    fn remote_snapshot() -> (BranchInfo, Vec<RecordedEvent>, branchyard::Inspection) {
        let info = BranchInfo {
            name: "answer".into(),
            git_branch: "by/answer".into(),
            worktree: "/repo/answer".into(),
            prompt: "Research the answer".into(),
            harness: "fake".into(),
            profile: "fake".into(),
            session: None,
            parent: None,
            children: Vec::new(),
            depth: 0,
            base: "base".into(),
            candidate: None,
            status: BranchStatus::NoChanges,
            turns: 1,
            cost_usd: None,
            created_at: 1,
            stalled: false,
            superseded_by: None,
            model: None,
            provider: "local".into(),
            isolated: false,
            guard: None,
        };
        let events = vec![
            RecordedEvent {
                at_ms: 1,
                activity: Activity::Prompt("research".into()),
            },
            RecordedEvent {
                at_ms: 2,
                activity: Activity::Result {
                    value: json!({"answer":"one"}),
                },
            },
        ];
        let mut inspection: branchyard::Inspection = serde_json::from_value(json!({
            "name":"answer", "status":{"state":"no_changes"}, "harness":"fake",
            "profile":"fake", "parent":null, "children":[], "depth":0, "turns":1,
            "candidate":null, "cost_usd":null, "subtree_cost_usd":0.0,
            "max_usd":null, "remaining_usd":null, "envelope":null, "last_message":"",
        }))
        .unwrap();
        inspection.result_contract = Some(branchyard::ResultContract::deliverable(
            json!({"type":"object", "required":["answer"], "properties":{"answer":{"type":"string"}}}),
        ).unwrap());
        inspection.result = Some(json!({"answer":"one"}));
        (info, events, inspection)
    }

    #[test]
    fn remote_result_snapshots_accept_agreeing_current_turn_evidence() {
        let (info, events, inspection) = remote_snapshot();
        let attempt = remote_result_attempt(&info, &events, Vec::new(), inspection);
        assert_eq!(attempt.result, Some(json!({"answer":"one"})));
        assert_eq!(attempt.result_event, Some(2));
        assert!(attempt.result_error.is_none());
        let view = commands::ResultView::from_attempt(&info, attempt.clone()).unwrap();
        assert!(view.complete());
        assert!(view.outcome(&info).is_ok());
        let mut newer = info;
        newer.turns += 1;
        let view = commands::ResultView::from_attempt(&newer, attempt).unwrap();
        assert!(!view.complete());
        assert!(view.outcome(&newer).is_err());
        let value = view.json(&newer);
        assert!(value["result"].is_null());
        assert!(value["result_event"].is_null());
    }

    #[test]
    fn remote_result_snapshots_clear_evidence_when_the_later_read_changed() {
        let (info, events, original) = remote_snapshot();
        let mut later_turn = original.clone();
        later_turn.turns += 1;
        let mut running = original.clone();
        running.status = BranchStatus::Running;
        let mut candidate = original.clone();
        candidate.candidate = Some(branchyard::CandidateInfo {
            commit: "new".into(),
            files_changed: 1,
            insertions: 1,
            deletions: 0,
        });
        let mut revised = original.clone();
        revised.result = Some(json!({"answer":"revised after the log read"}));
        let mut missing = original;
        missing.result = None;
        for inspection in [later_turn, running, candidate, revised, missing] {
            let attempt = remote_result_attempt(&info, &events, Vec::new(), inspection);
            assert!(attempt.result.is_none());
            assert!(attempt.result_event.is_none());
            assert!(attempt.result_error.as_deref().unwrap().contains("changed"));
            let view = commands::ResultView::from_attempt(&info, attempt).unwrap();
            assert!(!view.complete());
            assert!(view.outcome(&info).is_err());
        }
    }

    #[test]
    fn remote_result_snapshots_preserve_json_null_and_legacy_report_semantics() {
        let (mut info, mut events, mut inspection) = remote_snapshot();
        inspection.result_contract =
            Some(branchyard::ResultContract::deliverable(json!({"type":"null"})).unwrap());
        inspection.result = Some(serde_json::Value::Null);
        events[1].activity = Activity::Result {
            value: serde_json::Value::Null,
        };
        let accepted = remote_result_attempt(&info, &events, Vec::new(), inspection.clone());
        assert_eq!(accepted.result, Some(serde_json::Value::Null));
        assert_eq!(accepted.result_event, Some(2));
        assert!(accepted.result_error.is_none());

        inspection.result_contract =
            Some(branchyard::ResultContract::new(json!({"type":"null"}), true).unwrap());
        info.turns = 2;
        inspection.turns = 2;
        events.push(RecordedEvent {
            at_ms: 3,
            activity: Activity::Prompt("revise".into()),
        });
        // Inspection deliberately retains a Report's legacy latest answer;
        // a log with no result in this turn must not revive it.
        let report = remote_result_attempt(&info, &events, Vec::new(), inspection.clone());
        assert!(report.result.is_none());
        assert!(report.result_event.is_none());
        assert!(!report.result_error.as_deref().unwrap().contains("changed"));

        inspection.result_contract = None;
        inspection.status = BranchStatus::Running;
        let legacy = remote_result_attempt(&info, &events, Vec::new(), inspection);
        assert!(legacy.result_contract.is_none());
        assert!(legacy.result_error.is_none());
    }

    #[test]
    fn the_compare_table_has_a_row_per_attempt() {
        let text = compare_table(
            &[
                attempt("p-codex", &["a.rs", "b.rs", "c.rs"]),
                attempt("p-claude-code", &[]),
            ],
            Style { color: false },
        );
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("BRANCH"), "{text}");
        assert!(lines[0].contains("UNIQUE FILES"), "{text}");
        assert!(
            lines[1].contains("p-codex") && lines[1].contains("12.3k"),
            "{text}"
        );
        assert!(
            lines[1].contains("passed") && lines[1].contains("+10 -2"),
            "{text}"
        );
        assert!(lines[1].contains("a.rs, b.rs +1 more"), "{text}");
        assert!(text.contains("--diff p-codex p-claude-code"), "{text}");
    }

    #[test]
    fn checkpoint_lines_mark_the_current_one() {
        let list = Checkpoints {
            branch: "b".into(),
            base: "ba5e000000".into(),
            current: Some(1),
            checkpoints: vec![CheckpointEntry {
                checkpoint: Checkpoint {
                    turn: 1,
                    commit: "c0ffee0000111".into(),
                    git_ref: "refs/branchyard/b/1/turn-1".into(),
                    after: Some(0),
                    session: None,
                    files_changed: 1,
                    insertions: 2,
                    deletions: 0,
                    sandbox: None,
                },
                prompt: Some("write it".into()),
                available: true,
            }],
        };
        let text = checkpoint_lines(&list, Style { color: false });
        assert!(text.contains("    0  ba5e000000  base"), "{text}");
        assert!(
            text.contains("  * 1  c0ffee0000  1 file(s) +2 -0  write it"),
            "{text}"
        );
        let fresh = SessionContinuity::Fresh { reason: "x".into() };
        assert!(fresh.describe().contains("fresh session"));
    }
}
