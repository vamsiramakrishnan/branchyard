//! What a failed command says, how it says it, and the status it exits with.
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
//! `{"error": {"kind", "message", "next", "detail", "exit"}}` on standard
//! output, whichever layer failed: the engine, a server, the configuration
//! or the command line itself. `kind` is [`branchyard::Error::kind`] for an
//! engine error, the server's `code` for a server's (its `git_error` read
//! as `git`, and so on), and `usage`, `config`, `remote_unreachable` or
//! `io` for the layers above.
//!
//! One way to fail: a command returns a [`Report`] as [`Failure::Report`]
//! (or an engine, server or I/O error, which becomes one here), and
//! [`finish`] prints it, as JSON or as text, and exits with the status its
//! kind maps to in [`KINDS`]. The JSON's `exit` is that status.
//!
//! The engine's [`branchyard::Error`] texts are shared with the server, the
//! MCP tools and the Python module, so they stay plain statements of fact;
//! what the CLI needs beyond them (why a harness cannot start, which
//! profile routes no approvals) it reads from the error's data, never from
//! its text. The next command is the CLI's to add: it knows the command
//! names.

use std::ffi::OsString;
use std::io;
use std::process::ExitCode;

use branchyard::{Error, Unavailable};
use serde_json::{json, Value};

use crate::args::shell_quote;
use crate::commands::{Failure, Outcome};

/// How many lines of a failed check's output a message shows.
const OUTPUT_LINES: usize = 10;

/// The exit statuses `by` ends with, each meaning one thing. Every error
/// kind ends with one of them ([`KINDS`]); docs/cli.md's table is
/// generated from [`EXITS`] and [`KINDS`] by tools/docs_facts.py
/// (`cli.exit_codes`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// Done.
    Success = 0,
    /// The operation ran and failed.
    Failed = 1,
    /// The command line asks for what cannot be.
    Usage = 2,
    /// Something around the command must change first.
    Environment = 3,
    /// Refused before anything ran.
    Refused = 4,
    /// Interrupted with Ctrl-C.
    Interrupted = 130,
}

impl Exit {
    /// The status as the process ends with it.
    pub(crate) fn code(self) -> u8 {
        self as u8
    }
}

/// What each exit status means, in docs/cli.md's words. `by man
/// --markdown` prints it as docs/cli-reference.md's last table, and
/// tools/docs_facts.py reads it for docs/cli.md's.
pub(crate) const EXITS: [(Exit, &str); 6] = [
    (Exit::Success, "the command did what it was asked"),
    (
        Exit::Failed,
        "the operation ran and failed: a branch failed, a check failed, a merge conflicted, the \
         branch was busy; also a failure not yet given a kind (`unclassified`), and any kind not \
         listed here",
    ),
    (
        Exit::Usage,
        "the command line asks for what cannot be: a bad flag or value (clap's own usage errors \
         too), an unknown branch, harness or repository, a name already taken",
    ),
    (
        Exit::Environment,
        "something around the command must change first: not a repository, a harness (or gh, \
         ssh) not installed, a configuration file that does not load, a server that does not \
         answer or refuses the token, a login only you can do",
    ),
    (
        Exit::Refused,
        "refused before anything ran: a policy, a delegation envelope, a budget or quota, or a \
         capability the harness lacks",
    ),
    (
        Exit::Interrupted,
        "interrupted with Ctrl-C: the shell reports 128 + SIGINT. `by serve` and `by worker` \
         catch it and shut down; `by substrate check --smoke` catches SIGINT and SIGTERM to \
         remove its actor, then reports the cancelled smoke with exit 1",
    ),
];

/// Every error kind `by` reports (`--json`'s `kind`), each with the exit
/// status it ends with. A kind not listed exits [`Exit::Failed`].
pub(crate) const KINDS: &[(&str, Exit)] = &[
    // The command line.
    ("usage", Exit::Usage),
    ("invalid_name", Exit::Usage),
    ("invalid_request", Exit::Usage),
    ("invalid_rig", Exit::Usage),
    ("unknown_branch", Exit::Usage),
    ("unknown_harness", Exit::Usage),
    ("unknown_message", Exit::Usage),
    ("unknown_knowledge", Exit::Usage),
    ("unknown_repo", Exit::Usage),
    ("unknown_operation", Exit::Usage),
    ("unknown_trigger", Exit::Usage),
    ("not_found", Exit::Usage),
    ("branch_exists", Exit::Usage),
    // Around the command.
    ("not_a_repository", Exit::Environment),
    ("harness_unavailable", Exit::Environment),
    ("config", Exit::Environment),
    ("remote_unreachable", Exit::Environment),
    ("remote_protocol", Exit::Environment),
    ("unauthorized", Exit::Environment),
    ("shutting_down", Exit::Environment),
    ("manual", Exit::Environment),
    // A program `by` runs for you (gh, ssh) is not installed or not logged in.
    ("tool_unavailable", Exit::Environment),
    // Refused by a policy, an envelope, a budget or a capability.
    ("denied", Exit::Refused),
    ("unsupported", Exit::Refused),
    ("scope_required", Exit::Refused),
    ("repo_not_allowed", Exit::Refused),
    ("delegation_not_allowed", Exit::Refused),
    ("command_not_allowed", Exit::Refused),
    ("provider_not_allowed", Exit::Refused),
    ("secret_not_allowed", Exit::Refused),
    ("unapproved_tools_not_allowed", Exit::Refused),
    ("quota_exceeded", Exit::Refused),
    ("rate_limited", Exit::Refused),
    // The operation ran and failed.
    ("check_failed", Exit::Failed),
    ("check_timed_out", Exit::Failed),
    ("check_not_started", Exit::Failed),
    ("conflict", Exit::Failed),
    ("target_moved", Exit::Failed),
    ("dirty_target", Exit::Failed),
    ("already_merged", Exit::Failed),
    ("no_candidate", Exit::Failed),
    ("invalid_candidate", Exit::Failed),
    ("no_plan", Exit::Failed),
    ("running", Exit::Failed),
    ("not_running", Exit::Failed),
    ("steer_refused", Exit::Failed),
    ("fenced", Exit::Failed),
    ("stale_revision", Exit::Failed),
    ("git", Exit::Failed),
    ("harness", Exit::Failed),
    ("io", Exit::Failed),
    ("state", Exit::Failed),
    ("internal", Exit::Failed),
    // A program `by` ran for you (gh, an editor, anvil) failed.
    ("command_failed", Exit::Failed),
    // A bare message (`Failure::Message`) not yet given a kind of its own;
    // tools/failure_message_ratchet.json counts the places that build one.
    ("unclassified", Exit::Failed),
    // `by init`'s refusals of a plan (docs/setup.md): nothing was written.
    ("incomplete", Exit::Failed),
    ("invalid_answers", Exit::Failed),
    ("invalid_plan", Exit::Failed),
    ("would_overwrite", Exit::Failed),
];

/// The exit status of a failure of `kind` ([`KINDS`]).
pub(crate) fn exit_of(kind: &str) -> Exit {
    KINDS
        .iter()
        .find(|(known, _)| *known == kind)
        .map_or(Exit::Failed, |(_, exit)| *exit)
}

/// One failure, ready to print as text or as JSON. Build it with
/// `Report::new(kind, what).line(..).next(..)` and return it (`.into()`
/// makes it a [`Failure::Report`]); [`finish`] prints it.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    /// `unknown_branch`, `usage`, `config`, ...
    pub(crate) kind: String,
    /// The first line after `by: `: what failed, naming the object.
    pub(crate) message: String,
    /// Labelled lines under it (`why`, `check`, `output`); an empty label
    /// is an item of a list, indented with none.
    pub(crate) lines: Vec<(&'static str, String)>,
    /// What to run next, each a command or a short sentence with one.
    pub(crate) next: Vec<String>,
    /// What the error carries beyond its message, as
    /// [`branchyard::Error::detail`] gives it.
    pub(crate) detail: Option<Value>,
    /// More members of the JSON `error` object (`steer`, `field`).
    pub(crate) fields: Vec<(&'static str, Value)>,
    /// JSON or text, when the command decided it (the background command
    /// of `--detach` writes text); `None` asks the command line.
    pub(crate) json: Option<bool>,
}

impl Report {
    pub(crate) fn new(kind: impl Into<String>, message: impl Into<String>) -> Report {
        Report {
            kind: kind.into(),
            message: message.into(),
            lines: Vec::new(),
            next: Vec::new(),
            detail: None,
            fields: Vec::new(),
            json: None,
        }
    }

    /// A command line that asks for what cannot be (`usage`, exit 2).
    pub(crate) fn usage(message: impl Into<String>) -> Report {
        Report::new("usage", message)
    }

    pub(crate) fn line(mut self, label: &'static str, text: impl Into<String>) -> Report {
        self.lines.push((label, text.into()));
        self
    }

    pub(crate) fn next(mut self, step: impl Into<String>) -> Report {
        self.next.push(step.into());
        self
    }

    /// Another member of the JSON `error` object.
    pub(crate) fn field(mut self, name: &'static str, value: Value) -> Report {
        self.fields.push((name, value));
        self
    }

    /// Printed as JSON (`true`) or as text, whatever the command line says.
    pub(crate) fn with_json(mut self, json: bool) -> Report {
        self.json = Some(json);
        self
    }

    /// The exit status its kind ends with.
    pub(crate) fn exit(&self) -> Exit {
        exit_of(&self.kind)
    }

    /// The text for standard error.
    pub(crate) fn text(&self) -> String {
        format!("by: {}\n", self.said())
    }

    /// What the text says after `by: `, without the final newline.
    pub(crate) fn said(&self) -> String {
        let mut out = self.message.clone();
        for (label, text) in &self.lines {
            match (label.is_empty(), text.contains('\n')) {
                // An unlabelled line: an item of a list (`by init`'s paths).
                (true, _) => out.push_str(&format!("\n  {text}")),
                (false, false) => out.push_str(&format!("\n  {label}: {text}")),
                (false, true) => {
                    out.push_str(&format!("\n  {label}:"));
                    for line in text.lines() {
                        out.push_str(&format!("\n    {line}"));
                    }
                }
            }
        }
        if !self.next.is_empty() {
            out.push_str(&format!("\n  next: {}", self.next.join(", or ")));
        }
        out
    }

    /// The `--json` envelope for standard output. Its `message` is the
    /// first line and the labelled lines under it, so nothing the text
    /// says is missing from it; `exit` is the status the process ends with.
    pub(crate) fn json(&self) -> Value {
        let mut message = self.message.clone();
        for (label, text) in &self.lines {
            match label.is_empty() {
                true => message.push_str(&format!("\n{text}")),
                false => message.push_str(&format!("\n{label}: {text}")),
            }
        }
        let mut error = json!({"kind": self.kind, "message": message});
        if !self.next.is_empty() {
            error["next"] = json!(self.next);
        }
        if let Some(detail) = &self.detail {
            error["detail"] = detail.clone();
        }
        for (name, value) in &self.fields {
            error[*name] = value.clone();
        }
        error["exit"] = json!(self.exit().code());
        json!({ "error": error })
    }
}

impl From<Report> for Failure {
    fn from(report: Report) -> Failure {
        Failure::Report(Box::new(report))
    }
}

/// Print `report`: its JSON on standard output with `json` (unless the
/// report decided), else its text on standard error. A closed pipe is the
/// reader's choice. [`finish`] calls it, and `main` for a command line clap
/// refused; a command returns its report instead of printing it.
pub(crate) fn print(report: &Report, json: bool) {
    if report.json.unwrap_or(json) {
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

/// End a command: print its failure, once, as JSON with `json` (whether
/// `--json` is on the command line) or as text, and return the exit
/// status its kind maps to. The one place a failure is printed.
pub(crate) fn finish(outcome: Outcome, json: bool) -> ExitCode {
    let exit = match outcome {
        Ok(()) => Exit::Success,
        // A closed pipe, as in `by ls | head`, is the reader's choice.
        Err(Failure::Io(error)) if error.kind() == io::ErrorKind::BrokenPipe => Exit::Success,
        // Said already, and not as a report: the operation failed.
        Err(Failure::Reported) => Exit::Failed,
        Err(failure) => match self::failure(&failure, None) {
            Some(report) => {
                print(&report, json);
                report.exit()
            }
            None => Exit::Failed,
        },
    };
    ExitCode::from(exit.code())
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
        Error::HarnessUnavailable { harness, cause, .. } => unavailable(
            Report::new(error.kind(), error.to_string()),
            harness,
            *cause,
        ),
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
        Error::UnroutedApprovals { profile } => unrouted(profile),
        other => Report::new(other.kind(), other.to_string()),
    };
    report.detail = error.detail();
    report
}

/// A harness that cannot start: install it when it is missing, else see
/// what is installed.
fn unavailable(report: Report, harness: &str, cause: Unavailable) -> Report {
    match cause {
        Unavailable::NotOnPath if is_name(harness) => report
            .next(format!("by harnesses install {harness}"))
            .next("pick another with by harnesses"),
        _ => report.next("by harnesses (shows what is installed)"),
    }
}

/// A profile that routes no tool approvals, run without allowing that.
fn unrouted(profile: &str) -> Report {
    Report::new(
        "unsupported",
        format!(
            "unsupported: {profile} does not route tool permission requests to Branchyard, so \
             its tools would run without your policy"
        ),
    )
    .next(
        "add --allow-unapproved-tools (its tools then run under the harness's own permissions, \
         not your policy)",
    )
    .next("pick another profile: by harnesses --profiles")
}

/// The report for an error known by its `kind`, message and detail alone,
/// as a server or a parent's engine sends it: the hints that need only the
/// branch the message starts with, the one the command acts on, or the
/// detail's data.
fn by_kind(kind: &str, message: &str, detail: Option<&Value>, branch: Option<&str>) -> Report {
    let first = message.split_whitespace().next().unwrap_or_default();
    let said = |key: &str| detail.and_then(|d| d.get(key)).and_then(Value::as_str);
    let report = Report::new(kind, message);
    match kind {
        "unknown_branch" => report.next("by ls"),
        "unknown_harness" => {
            report.next("by harnesses --profiles (lists the harnesses and profiles)")
        }
        "harness_unavailable" => {
            match (
                said("harness"),
                said("cause").and_then(Unavailable::from_name),
            ) {
                (Some(harness), Some(cause)) => unavailable(report, harness, cause),
                _ => report.next("by harnesses (shows what is installed)"),
            }
        }
        "unsupported" if said("unsupported") == Some("unrouted_approvals") => {
            unrouted(said("profile").unwrap_or("this profile"))
        }
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
/// said all it has to. A step only the person can do (a login that needs a
/// terminal) is a report of kind `manual`, exit 3, like any other.
pub(crate) fn failure(failure: &Failure, branch: Option<&str>) -> Option<Report> {
    Some(match failure {
        Failure::Report(report) => (**report).clone(),
        Failure::Sdk(error) => sdk(error, branch),
        Failure::Remote(error) => remote(error, branch),
        Failure::Io(error) => Report::new("io", error.to_string()),
        Failure::Message(message) => Report::new("unclassified", message.clone()),
        Failure::Reported => return None,
    })
}

/// The kind a local `by` gives what a server calls `code`: the server's
/// `git_error`, `harness_error`, `io_error`, `state_error` and
/// `config_error` are `git`, `harness`, `io`, `state` and `config`, and a
/// `remote_error` is the kind its detail names.
pub(crate) fn local_kind(code: &str, detail: Option<&Value>) -> String {
    match code {
        "git_error" => "git",
        "harness_error" => "harness",
        "io_error" => "io",
        "state_error" => "state",
        "config_error" => "config",
        "remote_error" => detail.and_then(|d| d["kind"].as_str()).unwrap_or("remote"),
        code => code,
    }
    .to_owned()
}

/// The report for an error from a Branchyard server, under the kind a
/// local `by` would give it ([`local_kind`]).
pub(crate) fn remote(error: &branchyard_client::Error, branch: Option<&str>) -> Report {
    use branchyard_client::Error as Client;
    match error {
        Client::Api { error: body, .. } => {
            let kind = local_kind(&body.code, body.detail.as_ref());
            let mut report = by_kind(&kind, &body.message, body.detail.as_ref(), branch);
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

/// An I/O error in words, without the operating system's number:
/// `no such file or directory`, not `No such file or directory (os error
/// 2)` (CONTRIBUTING.md, "CLI style").
pub(crate) fn plain_io(error: &io::Error) -> String {
    let text = error.to_string();
    let text = match text.rfind(" (os error ") {
        Some(at) if text.ends_with(')') => &text[..at],
        _ => &text,
    };
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The report for a command line clap refused.
pub(crate) fn usage(error: &clap::Error) -> Report {
    let text = error.to_string();
    let message = text.trim_end();
    Report::usage(message.strip_prefix("error: ").unwrap_or(message))
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
        assert_eq!(json["error"]["exit"], 1);
        assert!(json["error"].get("detail").is_none());
        // The envelope stays one member, so a reader that checks for
        // exactly `{"error": ...}` still finds it.
        assert_eq!(json.as_object().map(serde_json::Map::len), Some(1));
    }

    #[test]
    fn a_report_carries_more_members_and_a_decided_format() {
        let report = Report::new("invalid_rig", "rig.toml: no seats")
            .field("line", json!(3))
            .with_json(false);
        assert_eq!(report.json()["error"]["line"], 3);
        assert_eq!(report.json()["error"]["exit"], 2);
        assert_eq!(report.json, Some(false));
        let failure: Failure = report.clone().into();
        assert_eq!(super::failure(&failure, None), Some(report));
    }

    #[test]
    fn an_io_error_is_said_without_its_number() {
        let missing = io::Error::from_raw_os_error(2);
        assert_eq!(plain_io(&missing), "no such file or directory");
        assert_eq!(plain_io(&io::Error::other("Broken")), "broken");
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
                cause: Unavailable::NotOnPath,
            },
            None,
        );
        assert_eq!(
            report.text(),
            "by: claude-code is not installed: claude was not found on PATH\n  next: by \
             harnesses install claude-code, or pick another with by harnesses\n"
        );
        assert_eq!(report.kind, "harness_unavailable");
        assert_eq!(report.exit(), Exit::Environment);
        assert_eq!(report.json()["error"]["detail"]["cause"], "not_on_path");
        // Any other cause: what is installed, not an install.
        let broken = sdk(
            &Error::HarnessUnavailable {
                harness: "claude-code".into(),
                reason: "/opt/claude is not an executable file".into(),
                cause: Unavailable::NotExecutable,
            },
            None,
        );
        assert_eq!(broken.next, ["by harnesses (shows what is installed)"]);
    }

    /// The hints come from the error's data: a server's (or a parent
    /// engine's) error carries it in its detail, whatever its text says.
    #[test]
    fn a_remote_refusal_is_read_from_its_detail_not_its_text() {
        let remote = |code: &str, message: &str, detail: Value| Error::Remote {
            kind: code.into(),
            message: message.into(),
            detail: Some(Box::new(detail)),
        };
        let missing = sdk(
            &remote(
                "harness_unavailable",
                "whatever the server says",
                json!({"harness": "codex", "cause": "not_on_path"}),
            ),
            None,
        );
        assert_eq!(
            missing.next,
            [
                "by harnesses install codex",
                "pick another with by harnesses"
            ]
        );
        let unrouted = sdk(
            &remote(
                "unsupported",
                "unsupported: anything",
                json!({"unsupported": "unrouted_approvals", "profile": "pi-rpc"}),
            ),
            None,
        );
        assert_eq!(
            unrouted.message,
            "unsupported: pi-rpc does not route tool permission requests to Branchyard, so its \
             tools would run without your policy"
        );
        assert_eq!(unrouted.next.len(), 2);
        assert_eq!(unrouted.exit(), Exit::Refused);
        // The text alone carries no hint any more.
        let bare = by_kind(
            "harness_unavailable",
            "codex is not installed: codex was not found on PATH",
            None,
            None,
        );
        assert_eq!(bare.next, ["by harnesses (shows what is installed)"]);
    }

    #[test]
    fn unrouted_approvals_name_the_flag_and_another_profile() {
        let report = sdk(
            &Error::UnroutedApprovals {
                profile: "pi-rpc".into(),
            },
            None,
        );
        assert_eq!(report.kind, "unsupported");
        assert_eq!(
            report.text(),
            "by: unsupported: pi-rpc does not route tool permission requests to Branchyard, so \
             its tools would run without your policy\n  next: add --allow-unapproved-tools (its \
             tools then run under the harness's own permissions, not your policy), or pick \
             another profile: by harnesses --profiles\n"
        );
        assert_eq!(
            report.detail,
            Some(json!({"unsupported": "unrouted_approvals", "profile": "pi-rpc"}))
        );
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
            cause: Unavailable::NotOnPath,
        };
        let other_reason = Error::HarnessUnavailable {
            harness: "g".into(),
            reason: "the command is empty".into(),
            cause: Unavailable::NoCommand,
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
            (
                Error::UnroutedApprovals {
                    profile: "amp-stream-json".into(),
                },
                "unsupported: amp-stream-json does not route tool permission requests to \
                 Branchyard, so its tools would run without your policy; allow that explicitly \
                 (--allow-unapproved-tools, TaskOptions::unapproved_tools) or choose another \
                 profile",
                "unsupported",
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
        // A bare message has no kind of its own yet, and exits 1 as before.
        let message = failure(&Failure::Message("gh failed".into()), None).unwrap();
        assert_eq!(
            (message.kind.as_str(), message.exit()),
            ("unclassified", Exit::Failed)
        );
        let transport = branchyard_client::Error::Transport {
            endpoint: "http://h:1".into(),
            message: "refused".into(),
        };
        let unreachable = remote(&transport, None);
        assert_eq!(unreachable.kind, "remote_unreachable");
        assert_eq!(unreachable.message, "cannot reach http://h:1: refused");
        assert_eq!(unreachable.exit(), Exit::Environment);
        assert_eq!(config("bad toml").kind, "config");
        assert!(failure(&Failure::Reported, None).is_none());
        // A step only the person can do is a report like any other: its
        // `--json` envelope carries the members the command adds, and 3.
        let manual: Failure = Report::new("manual", "no terminal here")
            .next("run `codex login` in a terminal")
            .field("status", json!("manual"))
            .into();
        let manual = failure(&manual, None).unwrap();
        assert_eq!((manual.exit(), manual.json), (Exit::Environment, None));
        assert_eq!(manual.json()["error"]["status"], "manual");
        assert_eq!(manual.json()["error"]["exit"], 3);
    }

    /// Every engine error, one of each variant. The `match` is exhaustive:
    /// a new variant does not compile until it is listed here, and so
    /// until its kind is in [`KINDS`] (the test below).
    fn every_engine_error() -> Vec<Error> {
        let all = vec![
            Error::NotARepository("/x".into()),
            Error::UnknownBranch("b".into()),
            Error::BranchExists("b".into()),
            Error::UnknownMessage(1),
            Error::UnknownKnowledge(1),
            Error::NoPlan("none".into()),
            Error::InvalidName {
                name: "B".into(),
                reason: "upper case".into(),
            },
            Error::UnknownHarness("h".into()),
            Error::HarnessUnavailable {
                harness: "h".into(),
                reason: "h was not found on PATH".into(),
                cause: Unavailable::NotOnPath,
            },
            Error::Unsupported("no".into()),
            Error::UnroutedApprovals {
                profile: "pi-rpc".into(),
            },
            Error::Config("bad".into()),
            Error::NoCandidate("b".into()),
            Error::TargetMoved {
                expected: "a".into(),
                actual: None,
            },
            Error::Conflict {
                files: vec!["f".into()],
            },
            Error::ConflictBetween {
                branch: "b".into(),
                target: "main".into(),
                merged: Vec::new(),
                files: vec!["f".into()],
            },
            Error::CheckFailed {
                output_tail: String::new(),
                ran: None,
                checks: Vec::new(),
                shared: None,
                own_work: Vec::new(),
            },
            Error::CheckTimedOut {
                timeout: std::time::Duration::from_secs(1),
                output_tail: String::new(),
                checks: Vec::new(),
            },
            Error::CheckNotStarted {
                reason: "no sh".into(),
                checks: Vec::new(),
            },
            Error::DirtyTarget("/x".into()),
            Error::AlreadyMerged {
                target: "main".into(),
            },
            Error::InvalidCandidate("c".into()),
            Error::Denied("no".into()),
            Error::Usage("no".into()),
            Error::Running("b".into()),
            Error::NotRunning("b".into()),
            Error::Fenced("lost".into()),
            Error::StaleRevision {
                branch: "b".into(),
                expected: 1,
                actual: 2,
            },
            Error::Remote {
                kind: "denied".into(),
                message: "no".into(),
                detail: None,
            },
            Error::Git("g".into()),
            Error::Harness("h".into()),
            Error::Io(io::Error::other("i")),
            Error::State("s".into()),
        ];
        for error in &all {
            match error {
                Error::NotARepository(_)
                | Error::UnknownBranch(_)
                | Error::BranchExists(_)
                | Error::UnknownMessage(_)
                | Error::UnknownKnowledge(_)
                | Error::NoPlan(_)
                | Error::InvalidName { .. }
                | Error::UnknownHarness(_)
                | Error::HarnessUnavailable { .. }
                | Error::Unsupported(_)
                | Error::UnroutedApprovals { .. }
                | Error::Config(_)
                | Error::NoCandidate(_)
                | Error::TargetMoved { .. }
                | Error::Conflict { .. }
                | Error::ConflictBetween { .. }
                | Error::CheckFailed { .. }
                | Error::CheckTimedOut { .. }
                | Error::CheckNotStarted { .. }
                | Error::DirtyTarget(_)
                | Error::AlreadyMerged { .. }
                | Error::InvalidCandidate(_)
                | Error::Denied(_)
                | Error::Usage(_)
                | Error::Running(_)
                | Error::NotRunning(_)
                | Error::Fenced(_)
                | Error::StaleRevision { .. }
                | Error::Remote { .. }
                | Error::Git(_)
                | Error::Harness(_)
                | Error::Io(_)
                | Error::State(_) => {}
            }
        }
        all
    }

    /// Every kind has one exit status: each engine error's kind is in the
    /// table, a server's code for it reads back as the same kind with the
    /// same status, and the kinds the CLI adds are there too.
    #[test]
    fn every_kind_maps_to_one_exit_status() {
        let listed = |kind: &str| KINDS.iter().filter(|(k, _)| *k == kind).count();
        for (kind, _) in KINDS {
            assert_eq!(listed(kind), 1, "{kind} is listed once");
        }
        for error in every_engine_error() {
            let kind = error.kind().to_owned();
            assert_eq!(listed(&kind), 1, "{kind} ({error:?}) has an exit status");
            let report = sdk(&error, None);
            assert_eq!(report.exit(), exit_of(&kind), "{kind}");
            assert_eq!(report.json()["error"]["exit"], exit_of(&kind).code());
            // The same error from a server: the same kind, the same status.
            let served = branchyard_server::error::sdk(&error);
            let read = local_kind(&served.body.code, served.body.detail.as_ref());
            assert_eq!(
                read, kind,
                "the server's {} reads as {kind}",
                served.body.code
            );
        }
        for kind in [
            "usage",
            "config",
            "io",
            "remote_unreachable",
            "remote_protocol",
            "manual",
            "steer_refused",
            "invalid_rig",
            "unknown_repo",
            "unauthorized",
        ] {
            assert_eq!(listed(kind), 1, "{kind} has an exit status");
        }
        // Each status but success and Ctrl-C has kinds, and is described.
        for (exit, meaning) in EXITS {
            assert!(!meaning.is_empty());
            let kinds = KINDS.iter().filter(|(_, e)| *e == exit).count();
            match exit {
                Exit::Success | Exit::Interrupted => assert_eq!(kinds, 0, "{exit:?}"),
                _ => assert!(kinds > 0, "{exit:?} has kinds"),
            }
        }
        assert_eq!(exit_of("never-heard-of-it"), Exit::Failed);
        assert_eq!(
            [
                exit_of("usage"),
                exit_of("not_a_repository"),
                exit_of("denied")
            ]
            .map(Exit::code),
            [2, 3, 4]
        );
    }

    /// A command returns its report and `finish` prints it, with its kind's
    /// status; what a command explained itself ([`Failure::Reported`]) is a
    /// failed operation (the end-to-end paths, a pre-flight refusal exiting
    /// 3 among them, are in tests/errors.rs).
    #[test]
    fn finish_ends_with_the_status_of_the_kind() {
        assert_eq!(finish(Err(Failure::Reported), false), ExitCode::from(1));
        let code = finish(
            Err(Report::new("harness_unavailable", "x is not installed")
                .with_json(false)
                .into()),
            false,
        );
        assert_eq!(code, ExitCode::from(3));
        assert_eq!(finish(Ok(()), false), ExitCode::SUCCESS);
        let pipe = io::Error::from(io::ErrorKind::BrokenPipe);
        assert_eq!(finish(Err(Failure::Io(pipe)), false), ExitCode::SUCCESS);
    }
}
