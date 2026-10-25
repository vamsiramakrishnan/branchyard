//! What a failed command says, and how it says it.
//!
//! One shape for every failure (CONTRIBUTING.md, "CLI style"):
//!
//! ```text
//! by: <what failed, naming the object>
//!   why: <only when it is not obvious>
//!   next: <the exact command to run>
//! ```
//!
//! With `--json` anywhere on the command line the same failure is
//! `{"error": {"kind", "message", "next", "detail"}}` on standard output,
//! whichever layer failed: the engine, a server, the configuration or the
//! command line itself. `kind` is [`branchyard::Error::kind`] for an
//! engine error, the server's `code` for a server's, and `usage`,
//! `config`, `remote_unreachable` or `io` for the layers above.
//!
//! The engine's [`branchyard::Error`] texts are shared with the server, the
//! MCP tools and the Python module, so they stay plain statements of fact.
//! The next command is the CLI's to add: it knows the command names.

use std::ffi::OsString;

use branchyard::Error;
use serde_json::{json, Value};

use crate::args::shell_quote;
use crate::commands::Failure;

/// How many lines of a failed check's output a message shows.
const OUTPUT_LINES: usize = 10;

/// One failure, ready to print as text or as JSON.
#[derive(Debug, PartialEq)]
pub(crate) struct Report {
    /// `unknown_branch`, `usage`, `config`, ...
    pub(crate) kind: String,
    /// The first line after `by: `: what failed, naming the object.
    pub(crate) message: String,
    /// Labelled lines under it (`why`, `check`, `output`).
    pub(crate) lines: Vec<(&'static str, String)>,
    /// What to run next, each a command or a short sentence with one.
    pub(crate) next: Vec<String>,
    /// What the error carries beyond its message, as
    /// [`branchyard::Error::detail`] gives it.
    pub(crate) detail: Option<Value>,
}

impl Report {
    pub(crate) fn new(kind: impl Into<String>, message: impl Into<String>) -> Report {
        Report {
            kind: kind.into(),
            message: message.into(),
            lines: Vec::new(),
            next: Vec::new(),
            detail: None,
        }
    }

    pub(crate) fn line(mut self, label: &'static str, text: impl Into<String>) -> Report {
        self.lines.push((label, text.into()));
        self
    }

    pub(crate) fn next(mut self, step: impl Into<String>) -> Report {
        self.next.push(step.into());
        self
    }

    /// The text for standard error.
    pub(crate) fn text(&self) -> String {
        let mut out = format!("by: {}\n", self.message);
        for (label, text) in &self.lines {
            match text.contains('\n') {
                false => out.push_str(&format!("  {label}: {text}\n")),
                true => {
                    out.push_str(&format!("  {label}:\n"));
                    for line in text.lines() {
                        out.push_str(&format!("    {line}\n"));
                    }
                }
            }
        }
        if !self.next.is_empty() {
            out.push_str(&format!("  next: {}\n", self.next.join(", or ")));
        }
        out
    }

    /// The `--json` envelope for standard output. Its `message` is the
    /// first line and the labelled lines under it, so nothing the text
    /// says is missing from it.
    pub(crate) fn json(&self) -> Value {
        let mut message = self.message.clone();
        for (label, text) in &self.lines {
            message.push_str(&format!("\n{label}: {text}"));
        }
        let mut error = json!({"kind": self.kind, "message": message});
        if !self.next.is_empty() {
            error["next"] = json!(self.next);
        }
        if let Some(detail) = &self.detail {
            error["detail"] = detail.clone();
        }
        json!({ "error": error })
    }
}

/// Print `report`: its JSON on standard output with `json`, else its text on
/// standard error. A closed pipe is the reader's choice.
pub(crate) fn print(report: &Report, json: bool) {
    if json {
        branchyard_support::best_effort(
            "print the error as JSON",
            crate::commands::print(&format!(
                "{}\n",
                serde_json::to_string_pretty(&report.json()).unwrap_or_default()
            )),
        );
    } else {
        eprint!("{}", report.text());
    }
}

/// Whether `--json` is among `argv`'s arguments (the program name first),
/// before any `--`: then a failure is reported as JSON, even one found
/// before the command was parsed.
pub(crate) fn wants_json(argv: &[OsString]) -> bool {
    argv.iter()
        .skip(1)
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--json")
}

/// The report for an engine error. `branch` is the branch the command acts
/// on, when it knows it, for the hints that name one.
pub(crate) fn sdk(error: &Error, branch: Option<&str>) -> Report {
    let mut report = match error {
        Error::NotARepository(_) => Report::new(error.kind(), error.to_string())
            .next("cd into your project, or run `git init` here"),
        Error::UnknownBranch(name) => {
            let mut message = error.to_string();
            let near = suggest(name, &known_branches());
            match near.as_slice() {
                [] => {}
                [one] => message.push_str(&format!("; did you mean {one}?")),
                several => message.push_str(&format!("; did you mean {}?", several.join(" or "))),
            }
            Report::new(error.kind(), message).next("by ls")
        }
        Error::Running(name) if is_name(name) => Report::new(error.kind(), error.to_string())
            .next(format!("by wait {name} (blocks until it settles)"))
            .next(format!("by cancel {name} (stops it)")),
        Error::NotRunning(name) if is_name(name) => Report::new(error.kind(), error.to_string())
            .next(format!("by send {name} \"<prompt>\" (starts a new turn)")),
        Error::HarnessUnavailable { harness, reason } => {
            let report = Report::new(error.kind(), error.to_string());
            match reason.ends_with("was not found on PATH") {
                true => report
                    .next(format!("by harnesses install {harness}"))
                    .next("pick another with by harnesses"),
                false => report.next("by harnesses (shows what is installed)"),
            }
        }
        Error::UnknownHarness(_) => Report::new(error.kind(), error.to_string())
            .next("by harnesses --profiles (lists the harnesses and profiles)"),
        Error::CheckFailed {
            output_tail,
            ran,
            checks,
            shared,
            own_work,
        } if checks.is_empty() && shared.is_none() && own_work.is_empty() => {
            let (status, command) = ran
                .as_deref()
                .map_or(("", &[][..]), |ran| (ran.status.as_str(), &ran.command[..]));
            check_failed(error.kind(), branch, output_tail, status, command)
        }
        Error::CheckTimedOut { .. } | Error::CheckNotStarted { .. } if branch.is_some() => {
            let name = branch.unwrap_or_default();
            Report::new(error.kind(), error.to_string())
                .next(format!("by check {name} (runs the check without merging)"))
        }
        Error::Remote {
            kind,
            message,
            detail,
        } => by_kind(kind, message, detail.as_deref(), branch),
        // The engine names the SDK option too; here only the flag matters.
        Error::Unsupported(what) if what.contains("does not route tool permission requests") => {
            let what = what.split("; allow that explicitly").next().unwrap_or(what);
            Report::new(error.kind(), format!("unsupported: {what}"))
                .next(
                    "add --allow-unapproved-tools (its tools then run under the harness's own \
                     permissions, not your policy)",
                )
                .next("pick another profile: by harnesses --profiles")
        }
        other => Report::new(other.kind(), other.to_string()),
    };
    report.detail = error.detail();
    report
}

/// The report for an error known by its `kind` and message alone, as a
/// server sends it: the hints that need only the branch the message starts
/// with, or the one the command acts on.
fn by_kind(kind: &str, message: &str, detail: Option<&Value>, branch: Option<&str>) -> Report {
    let first = message.split_whitespace().next().unwrap_or_default();
    let report = Report::new(kind, message);
    match kind {
        "unknown_branch" => report.next("by ls"),
        "unknown_harness" => {
            report.next("by harnesses --profiles (lists the harnesses and profiles)")
        }
        "harness_unavailable" if message.ends_with("was not found on PATH") && is_name(first) => {
            report
                .next(format!("by harnesses install {first}"))
                .next("pick another with by harnesses")
        }
        "harness_unavailable" => report.next("by harnesses (shows what is installed)"),
        "running" if is_name(first) => report
            .next(format!("by wait {first} (blocks until it settles)"))
            .next(format!("by cancel {first} (stops it)")),
        "not_running" if is_name(first) => {
            report.next(format!("by send {first} \"<prompt>\" (starts a new turn)"))
        }
        // A failed check a server detailed: reported as a local one is.
        "check_failed"
            if detail.is_some_and(|d| {
                d.get("output_tail").is_some()
                    && d.get("checks").is_none()
                    && d.get("own_work").is_none()
                    && d.get("shared").is_none()
            }) =>
        {
            let d = detail.cloned().unwrap_or_default();
            let text = |key: &str| {
                d.get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let command: Vec<String> = d
                .get("command")
                .and_then(Value::as_array)
                .map(|words| {
                    words
                        .iter()
                        .filter_map(|w| w.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            check_failed(
                kind,
                branch,
                &text("output_tail"),
                &text("status"),
                &command,
            )
        }
        "check_failed" | "check_timed_out" | "check_not_started" => match branch {
            Some(name) => report.next(format!("by check {name} (runs the check without merging)")),
            None => report,
        },
        _ => report,
    }
}

/// A failed `by merge` check: the check, how it ended, the end of its
/// output, and that nothing was merged.
fn check_failed(
    kind: &str,
    branch: Option<&str>,
    output_tail: &str,
    status: &str,
    command: &[String],
) -> Report {
    let of = branch.map_or_else(|| "the check".to_owned(), |b| format!("the check for {b}"));
    let how = match status.is_empty() {
        true => String::new(),
        false => format!(" ({status})"),
    };
    let mut report = Report::new(kind, format!("{of} failed{how}; nothing was merged."));
    if !command.is_empty() {
        let words: Vec<String> = command.iter().map(|w| shell_quote(w)).collect();
        report = report.line("check", words.join(" "));
    }
    if let Some(shown) = last_lines(output_tail) {
        report = report.line("output", shown);
    }
    if let Some(name) = branch {
        report = report.next(format!("by check {name} (runs the check without merging)"));
    }
    report
}

/// The last [`OUTPUT_LINES`] lines of `output`, a leading `…` when cut;
/// `None` when it holds nothing.
fn last_lines(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.trim_end().lines().collect();
    if lines.is_empty() {
        return None;
    }
    let from = lines.len().saturating_sub(OUTPUT_LINES);
    let shown = lines[from..].join("\n");
    Some(match from {
        0 => shown,
        _ => format!("…\n{shown}"),
    })
}

/// The report for any failure; `None` for [`Failure::Reported`], which has
/// said all it has to.
pub(crate) fn failure(failure: &Failure, branch: Option<&str>) -> Option<Report> {
    Some(match failure {
        Failure::Sdk(error) => sdk(error, branch),
        Failure::Remote(error) => remote(error, branch),
        Failure::Io(error) => Report::new("io", error.to_string()),
        Failure::Message(message) => Report::new("usage", message.clone()),
        Failure::Reported => return None,
        // Something only the person can do remains (exit 2); an empty text
        // means stdout already said what.
        Failure::Manual(message) if message.is_empty() => return None,
        Failure::Manual(message) => Report::new("manual", message.clone()),
    })
}

/// The report for an error from a Branchyard server: its `code` is the
/// kind, as `by --json` has always printed a server's.
pub(crate) fn remote(error: &branchyard_client::Error, branch: Option<&str>) -> Report {
    use branchyard_client::Error as Client;
    match error {
        Client::Api { error: body, .. } => {
            let mut report = by_kind(&body.code, &body.message, body.detail.as_ref(), branch);
            report.detail = body.detail.clone();
            report
        }
        Client::Transport { .. } => Report::new("remote_unreachable", error.to_string())
            .next(crate::remote::UNREACHABLE_FIX),
        Client::Protocol(_) => Report::new("remote_protocol", error.to_string()),
        Client::Config(_) => Report::new("config", error.to_string()),
    }
}

/// The report for a configuration file that could not be applied.
pub(crate) fn config(message: &str) -> Report {
    Report::new("config", message)
}

/// The report for a command line clap refused.
pub(crate) fn usage(error: &clap::Error) -> Report {
    let text = error.to_string();
    let message = text.trim_end();
    Report::new("usage", message.strip_prefix("error: ").unwrap_or(message))
}

/// Whether `text` could be a branch name, so a hint may name it.
fn is_name(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
}

/// The branches of the repository `by` is in, if it can be opened.
fn known_branches() -> Vec<String> {
    crate::commands::open()
        .ok()
        .and_then(|yard| yard.branches().ok())
        .map(|infos| infos.into_iter().map(|info| info.name).collect())
        .unwrap_or_default()
}

/// Up to three of `names` close to `typed`, nearest first: a name that
/// starts with it, or one within a third of its length in edits.
pub(crate) fn suggest(typed: &str, names: &[String]) -> Vec<String> {
    let allowed = (typed.chars().count() / 3).max(1);
    let mut near: Vec<(usize, &String)> = names
        .iter()
        .filter(|name| name.as_str() != typed)
        .filter_map(|name| {
            let distance = edit_distance(typed, name);
            let prefix = typed.chars().count() >= 2 && name.starts_with(typed);
            (distance <= allowed || prefix).then_some((distance, name))
        })
        .collect();
    near.sort();
    near.into_iter()
        .take(3)
        .map(|(_, name)| name.clone())
        .collect()
}

/// The Levenshtein distance between `a` and `b`, a swap of neighbours
/// counting as one edit.
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut rows = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in rows.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in rows[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (rows[i - 1][j] + 1)
                .min(rows[i][j - 1] + 1)
                .min(rows[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(rows[i - 2][j - 2] + 1);
            }
            rows[i][j] = best;
        }
    }
    rows[a.len()][b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn a_misspelt_branch_gets_the_nearest_names() {
        let known = names(&["broken", "hello", "hello-fork", "other"]);
        assert_eq!(suggest("brokn", &known), ["broken"]);
        assert_eq!(suggest("borken", &known), ["broken"]);
        assert_eq!(suggest("hel", &known), ["hello", "hello-fork"]);
        assert_eq!(suggest("zzzzzz", &known), Vec::<String>::new());
        // Never the name itself, and nothing for an empty repository.
        assert!(suggest("hello", &names(&["hello"])).is_empty());
        assert!(suggest("a", &[]).is_empty());
    }

    #[test]
    fn json_is_wanted_before_a_double_dash_only() {
        let argv =
            |line: &str| -> Vec<OsString> { line.split_whitespace().map(OsString::from).collect() };
        assert!(wants_json(&argv("by show x --json")));
        assert!(wants_json(&argv("by --json ls")));
        assert!(!wants_json(&argv("by run -- --json")));
        assert!(!wants_json(&argv("by run --json-ish")));
        assert!(!wants_json(&argv("by ls")));
    }

    #[test]
    fn the_text_is_what_why_and_next() {
        let report = Report::new("running", "stuck is still running its turn")
            .line("why", "a turn holds the branch")
            .next("by wait stuck")
            .next("by cancel stuck");
        assert_eq!(
            report.text(),
            "by: stuck is still running its turn\n  why: a turn holds the branch\n  \
             next: by wait stuck, or by cancel stuck\n"
        );
        let json = report.json();
        assert_eq!(json["error"]["kind"], "running");
        assert_eq!(
            json["error"]["message"],
            "stuck is still running its turn\nwhy: a turn holds the branch"
        );
        assert_eq!(
            json["error"]["next"],
            json!(["by wait stuck", "by cancel stuck"])
        );
        assert!(json["error"].get("detail").is_none());
    }

    #[test]
    fn running_and_not_running_say_the_command_to_run() {
        let running = sdk(&Error::Running("stuck".into()), None);
        assert_eq!(running.message, "stuck is still running its turn");
        assert_eq!(
            running.text(),
            "by: stuck is still running its turn\n  next: by wait stuck (blocks until it \
             settles), or by cancel stuck (stops it)\n"
        );
        let idle = sdk(&Error::NotRunning("hello".into()), None);
        assert_eq!(
            idle.text(),
            "by: hello has no running turn to steer\n  next: by send hello \"<prompt>\" \
             (starts a new turn)\n"
        );
        // A scratch area's lock is not a branch: no command names it.
        let held = sdk(&Error::Running("scratch area x (held by y)".into()), None);
        assert!(held.next.is_empty());
    }

    #[test]
    fn a_missing_harness_says_how_to_install_or_pick() {
        let report = sdk(
            &Error::HarnessUnavailable {
                harness: "claude-code".into(),
                reason: "claude was not found on PATH".into(),
            },
            None,
        );
        assert_eq!(
            report.text(),
            "by: claude-code is not installed: claude was not found on PATH\n  next: by \
             harnesses install claude-code, or pick another with by harnesses\n"
        );
        assert_eq!(report.kind, "harness_unavailable");
    }

    #[test]
    fn a_failed_check_names_the_check_the_exit_and_that_nothing_merged() {
        let error = Error::CheckFailed {
            output_tail: "line 1\nnope\n".into(),
            ran: Some(Box::new(branchyard::FailedCheck {
                command: vec!["sh".into(), "-c".into(), "echo nope >&2; exit 3".into()],
                status: "exit 3".into(),
            })),
            checks: Vec::new(),
            shared: None,
            own_work: Vec::new(),
        };
        let report = sdk(&error, Some("broken"));
        assert_eq!(
            report.text(),
            "by: the check for broken failed (exit 3); nothing was merged.\n  check: sh -c \
             'echo nope >&2; exit 3'\n  output:\n    line 1\n    nope\n  next: by check broken \
             (runs the check without merging)\n"
        );
        assert_eq!(report.kind, "check_failed");
    }

    #[test]
    fn a_long_check_output_shows_its_end() {
        let output: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        let shown = last_lines(&output).unwrap();
        assert!(shown.starts_with("…\nline 21\n"), "{shown}");
        assert!(shown.ends_with("line 30"), "{shown}");
        assert_eq!(last_lines("\n\n"), None);
    }

    /// The engine's texts carry no variant prefix (`denied:`, `state:`,
    /// `git:`) and no relative path; the kinds `--json` reports stay.
    #[test]
    fn the_engine_texts_are_plain_and_their_kinds_are_stable() {
        let missing_tool = Error::HarnessUnavailable {
            harness: "g".into(),
            reason: "g was not found on PATH".into(),
        };
        let other_reason = Error::HarnessUnavailable {
            harness: "g".into(),
            reason: "the command is empty".into(),
        };
        let failed = Error::CheckFailed {
            output_tail: "nope\n".into(),
            ran: Some(Box::new(branchyard::FailedCheck {
                command: Vec::new(),
                status: "exit 3".into(),
            })),
            checks: Vec::new(),
            shared: None,
            own_work: Vec::new(),
        };
        for (error, text, kind) in [
            (Error::Denied("no way".into()), "no way", "denied"),
            (
                Error::Usage("give a branch".into()),
                "give a branch",
                "usage",
            ),
            (
                Error::State("create /x: busy".into()),
                "create /x: busy",
                "state",
            ),
            (
                Error::Git("git status failed".into()),
                "git status failed",
                "git",
            ),
            (
                Error::Running("a".into()),
                "a is still running its turn",
                "running",
            ),
            (
                Error::NotRunning("a".into()),
                "a has no running turn to steer",
                "not_running",
            ),
            (
                missing_tool,
                "g is not installed: g was not found on PATH",
                "harness_unavailable",
            ),
            (
                other_reason,
                "g is unavailable: the command is empty",
                "harness_unavailable",
            ),
            (failed, "check failed (exit 3):\nnope\n", "check_failed"),
        ] {
            assert_eq!(error.to_string(), text);
            assert_eq!(error.kind(), kind);
        }
        let relative = Error::NotARepository("some/dir".into()).to_string();
        assert!(
            relative.starts_with('/')
                && relative.ends_with("/some/dir is not inside a git repository"),
            "{relative}"
        );
    }

    #[test]
    fn layers_above_the_engine_have_their_own_kinds() {
        let message = failure(&Failure::Message("--priority orders a queue".into()), None);
        assert_eq!(message.unwrap().kind, "usage");
        let transport = branchyard_client::Error::Transport {
            endpoint: "http://h:1".into(),
            message: "refused".into(),
        };
        let unreachable = remote(&transport, None);
        assert_eq!(unreachable.kind, "remote_unreachable");
        assert_eq!(unreachable.message, "cannot reach http://h:1: refused");
        assert_eq!(config("bad toml").kind, "config");
        assert!(failure(&Failure::Reported, None).is_none());
    }
}
