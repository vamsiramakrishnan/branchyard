//! `by hook` and `by statusline`: what a Claude Code session that a person
//! started (not one Branchyard runs, whose status comes from its own
//! stream) learns about the repository's branches, through the plugin's
//! hooks and the `statusLine` setting. See `docs/harness-integration.md`.
//!
//! Both run on every prompt or redraw of someone else's session, so they
//! read local state only ([`branchyard::glance`], a read-only look at
//! `.branchyard/state.db`), give up after [`DEADLINE`], and never fail: any
//! problem prints nothing on stdout, at most a note on stderr, and exits 0.

use std::collections::BTreeMap;
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use branchyard::glance::{self, Glance};
use branchyard::{BranchInfo, BranchStatus, Message};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How long either command may take before it gives up and prints nothing.
const DEADLINE: Duration = Duration::from_secs(2);
/// At most this many lines of context per hook call.
const MAX_LINES: usize = 12;
/// At most this many characters of one line.
const MAX_LINE: usize = 240;

/// `by hook <event>`: always exits 0.
pub fn hook(event: &str) -> ExitCode {
    let event = event.to_owned();
    finish(
        "hook",
        within(DEADLINE, move || {
            let input = stdin_text();
            let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
            hook_output(&event, &input, &env, &cwd())
        }),
    )
}

/// `by statusline`: always exits 0.
pub fn statusline() -> ExitCode {
    finish(
        "statusline",
        within(DEADLINE, move || {
            let input = stdin_text();
            let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
            status_line(&input, &env, &cwd())
        }),
    )
}

/// Print `outcome`'s text, if any; a failure is a note on stderr. Exit 0
/// either way, so a session never sees a failed hook.
fn finish(what: &str, outcome: Result<Option<String>, String>) -> ExitCode {
    match outcome {
        Ok(Some(text)) => {
            let mut stdout = std::io::stdout().lock();
            branchyard_support::best_effort(
                "print the output",
                stdout
                    .write_all(text.as_bytes())
                    .and_then(|()| stdout.flush()),
            );
        }
        Ok(None) => {}
        Err(error) => eprintln!("by {what}: {error}"),
    }
    ExitCode::SUCCESS
}

/// Run `f` on a thread of its own, giving up after `deadline`: the process
/// exits then, and the thread with it.
fn within<F>(deadline: Duration, f: F) -> Result<Option<String>, String>
where
    F: FnOnce() -> Result<Option<String>, String> + Send + 'static,
{
    let (send, receive) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("by-hook".into())
        .spawn(move || {
            // The receiver is gone only after the deadline passed.
            branchyard_support::best_effort("send the result", send.send(f()));
        });
    if let Err(error) = spawned {
        return Err(format!("could not start: {error}"));
    }
    match receive.recv_timeout(deadline) {
        Ok(outcome) => outcome,
        Err(_) => Err(format!(
            "gave up after {}s; printed nothing",
            deadline.as_secs()
        )),
    }
}

/// Standard input, unless it is a terminal (a person running the command
/// by hand), which would block until the deadline.
fn stdin_text() -> String {
    let mut text = String::new();
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        // Unreadable input is no input: the defaults still apply.
        branchyard_support::best_effort(
            "read standard input",
            stdin.lock().read_to_string(&mut text),
        );
    }
    text
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_default()
}

/// The JSON Claude Code passes, or `null` when it is not JSON.
fn parse(input: &str) -> Value {
    serde_json::from_str(input).unwrap_or(Value::Null)
}

/// The yard for `dir`: the one `BRANCHYARD_ROOT` names when it has one,
/// else the nearest above `dir`.
fn root_for(dir: &Path, env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    env("BRANCHYARD_ROOT")
        .and_then(|root| glance::find_root(Path::new(&root)))
        .or_else(|| glance::find_root(dir))
}

/// A hook's stdout for `event`, given its JSON `input`: `None` when there
/// is nothing new, or no yard.
pub(crate) fn hook_output(
    event: &str,
    input: &str,
    env: &dyn Fn(&str) -> Option<String>,
    cwd: &Path,
) -> Result<Option<String>, String> {
    let name = match event {
        "session-start" => "SessionStart",
        "user-prompt-submit" => "UserPromptSubmit",
        other => {
            return Err(format!(
                "unknown event {other:?}; expected session-start or user-prompt-submit"
            ))
        }
    };
    // Without the hook's JSON there is no session to remember for.
    let input = parse(input);
    if !input.is_object() {
        return Err("standard input is not a hook's JSON object; printed nothing".into());
    }
    let dir = input["cwd"]
        .as_str()
        .map_or_else(|| cwd.to_path_buf(), PathBuf::from);
    let Some(root) = root_for(&dir, env) else {
        return Ok(None);
    };
    let branch = env("BRANCHYARD_BRANCH");
    let Some(glance) = glance::read(&root, branch.as_deref()).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let session = session_file_name(input["session_id"].as_str().unwrap_or_default());
    let path = glance::state_dir(&root)
        .join("hooks")
        .join(format!("{session}.json"));
    let cursor = read_cursor(&path);
    let (lines, next) = match &branch {
        Some(branch) => inbox_lines(&glance, branch, cursor.as_ref()),
        None => change_lines(&glance, cursor.as_ref()),
    };
    if cursor.as_ref() != Some(&next) {
        if let Err(error) = write_cursor(&path, &next) {
            eprintln!("by hook: could not remember what was shown: {error}");
        }
    }
    if lines.is_empty() {
        return Ok(None);
    }
    let output = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": name,
            "additionalContext": capped(lines).join("\n"),
        }
    });
    Ok(Some(format!("{output}\n")))
}

/// What a session has been told: the last message id shown, and each
/// branch's last reported state.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Cursor {
    #[serde(default)]
    message: u64,
    #[serde(default)]
    branches: BTreeMap<String, String>,
}

fn read_cursor(path: &Path) -> Option<Cursor> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Write the cursor through a temporary file and a rename, so a reader
/// never sees half of one.
fn write_cursor(path: &Path, cursor: &Cursor) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string(cursor).map_err(std::io::Error::other)?;
    let temporary = path.with_extension(format!("json.{}", std::process::id()));
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        branchyard_support::cleanup_file(&temporary);
    })
}

/// A session id as a file name: letters, digits, `-` and `_` only.
fn session_file_name(id: &str) -> String {
    let name: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(100)
        .collect();
    match name.is_empty() {
        true => "default".to_owned(),
        false => name,
    }
}

/// `branch`'s unread messages not shown to this session yet.
fn inbox_lines(glance: &Glance, branch: &str, cursor: Option<&Cursor>) -> (Vec<String>, Cursor) {
    let seen = cursor.map_or(0, |c| c.message);
    let new: Vec<&Message> = glance.unread.iter().filter(|m| m.id > seen).collect();
    let next = Cursor {
        message: glance
            .unread
            .iter()
            .map(|m| m.id)
            .max()
            .unwrap_or(0)
            .max(seen),
        branches: BTreeMap::new(),
    };
    if new.is_empty() {
        return (Vec::new(), next);
    }
    let mut lines = vec![format!(
        "Branchyard: {} unread message{} for {branch} \
         (by inbox --as {branch} --unread; by answer --as {branch} ID TEXT):",
        new.len(),
        plural(new.len())
    )];
    lines.extend(new.iter().map(|m| {
        let reply = m
            .in_reply_to
            .map(|id| format!(" re #{id}"))
            .unwrap_or_default();
        line(&format!(
            "- #{} {} from {}{reply}: {}",
            m.id, m.kind, m.from, m.text
        ))
    }));
    (lines, next)
}

/// Branches whose state a person acts on and that changed since this
/// session's last hook call. On a session's first call, only what needs an
/// answer now (asking, blocked, a plan to approve) and a count of what is
/// running or ready: older endings are not news.
fn change_lines(glance: &Glance, cursor: Option<&Cursor>) -> (Vec<String>, Cursor) {
    let mut next = Cursor::default();
    let mut changed = Vec::new();
    for info in &glance.branches {
        let asking = glance.asking.iter().rev().find(|m| m.from == info.name);
        let Some(label) = notable(info, asking) else {
            continue;
        };
        let key = format!("{}|{}", info.turns, label.text);
        let new = match cursor {
            Some(cursor) => cursor.branches.get(&info.name) != Some(&key),
            None => label.urgent,
        };
        if new {
            changed.push(line(&format!("- {}: {}", info.name, label.text)));
        }
        next.branches.insert(info.name.clone(), key);
    }
    let mut lines = Vec::new();
    if !changed.is_empty() {
        lines.push(
            "Branchyard: branches changed (by ls; by show NAME; by inbox --as PARENT):".to_owned(),
        );
        lines.extend(changed);
    }
    if cursor.is_none() {
        let counts = glance.counts();
        let mut parts = Vec::new();
        if counts.running > 0 {
            parts.push(format!("{} running", counts.running));
        }
        if counts.ready > 0 {
            parts.push(format!("{} ready to integrate", counts.ready));
        }
        if !parts.is_empty() {
            lines.push(format!("Branchyard: {} (by ls).", parts.join(", ")));
        }
    }
    (lines, next)
}

/// A branch state worth telling a session about.
struct Label {
    text: String,
    /// Needs an answer or a decision, not only news.
    urgent: bool,
}

fn notable(info: &BranchInfo, asking: Option<&Message>) -> Option<Label> {
    let label = |text: String, urgent: bool| Some(Label { text, urgent });
    if let Some(message) = asking {
        return label(
            format!(
                "asking (#{} {}): {}",
                message.id, message.kind, message.text
            ),
            true,
        );
    }
    match &info.status {
        BranchStatus::Running
        | BranchStatus::Waiting
        | BranchStatus::WaitingOnChildren
        | BranchStatus::Merged { .. }
        | BranchStatus::Discarded { .. } => None,
        BranchStatus::Ready => {
            let files = info.candidate.as_ref().map_or(0, |c| c.files_changed);
            label(
                format!(
                    "finished, ready to integrate ({files} file{} changed)",
                    plural(files as usize)
                ),
                false,
            )
        }
        BranchStatus::NoChanges => label("finished with no changes".into(), false),
        BranchStatus::Failed { reason } => label(format!("failed: {reason}"), false),
        BranchStatus::BudgetExceeded { limit } => {
            label(format!("stopped at its {limit} limit"), false)
        }
        BranchStatus::Interrupted => label("interrupted".into(), false),
        BranchStatus::Blocked { reason } => label(format!("blocked: {reason}"), true),
        BranchStatus::AwaitingPlanApproval => {
            label("waiting for its plan to be approved".into(), true)
        }
    }
}

/// `lines`, at most [`MAX_LINES`] of them, the last saying how many more.
fn capped(mut lines: Vec<String>) -> Vec<String> {
    if lines.len() > MAX_LINES {
        let more = lines.len() - (MAX_LINES - 1);
        lines.truncate(MAX_LINES - 1);
        lines.push(format!("- ... and {more} more (by ls)"));
    }
    lines
}

/// One line of at most [`MAX_LINE`] characters, newlines folded.
fn line(text: &str) -> String {
    let flat = text.replace(['\n', '\r'], " ");
    match flat.chars().count() > MAX_LINE {
        true => format!("{}...", flat.chars().take(MAX_LINE - 3).collect::<String>()),
        false => flat,
    }
}

fn plural(n: usize) -> &'static str {
    match n {
        1 => "",
        _ => "s",
    }
}

/// The status line for Claude Code's JSON `input`: `None` outside a yard.
pub(crate) fn status_line(
    input: &str,
    env: &dyn Fn(&str) -> Option<String>,
    cwd: &Path,
) -> Result<Option<String>, String> {
    let input = parse(input);
    let dir = input["workspace"]["current_dir"]
        .as_str()
        .or_else(|| input["cwd"].as_str())
        .map_or_else(|| cwd.to_path_buf(), PathBuf::from);
    let Some(root) = root_for(&dir, env) else {
        return Ok(None);
    };
    let Some(glance) = glance::read(&root, None).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let mut parts = Vec::new();
    if let Some(branch) =
        env("BRANCHYARD_BRANCH").or_else(|| glance.branch_at(&dir).map(|info| info.name.clone()))
    {
        parts.push(branch);
    }
    let counts = glance.counts();
    for (n, word) in [
        (counts.running, "running"),
        (counts.waiting, "waiting"),
        (counts.asking, "asking"),
        (counts.blocked, "blocked"),
        (counts.ready, "ready"),
        (counts.failed, "failed"),
    ] {
        if n > 0 {
            parts.push(format!("{n} {word}"));
        }
    }
    if parts.is_empty() {
        parts.push(match glance.branches.is_empty() {
            true => "no branches".to_owned(),
            false => "idle".to_owned(),
        });
    }
    Ok(Some(format!("by: {}\n", parts.join(" · "))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_become_safe_file_names() {
        assert_eq!(session_file_name("abc-123_x"), "abc-123_x");
        assert_eq!(session_file_name("../../etc/passwd"), "etcpasswd");
        assert_eq!(session_file_name(""), "default");
        assert_eq!(session_file_name("///"), "default");
        assert_eq!(session_file_name(&"a".repeat(300)).len(), 100);
    }

    #[test]
    fn lines_are_capped_and_folded() {
        let many: Vec<String> = (0..20).map(|n| n.to_string()).collect();
        let shown = capped(many);
        assert_eq!(shown.len(), MAX_LINES);
        assert_eq!(shown[MAX_LINES - 1], "- ... and 9 more (by ls)");
        assert_eq!(line("a\nb"), "a b");
        assert_eq!(line(&"x".repeat(500)).chars().count(), MAX_LINE);
    }

    #[test]
    fn an_unknown_event_or_no_yard_prints_nothing() {
        let dir = branchyard_testkit::Scratch::new("hook-unit");
        let none = |_: &str| None;
        assert!(hook_output("stop", "{}", &none, dir.path()).is_err());
        assert!(hook_output("session-start", "not json", &none, dir.path()).is_err());
        assert_eq!(
            hook_output("session-start", "{}", &none, dir.path()),
            Ok(None)
        );
        assert_eq!(status_line("", &none, dir.path()), Ok(None));
    }
}
