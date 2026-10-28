//! `by doctor [--json] [--path PATH]`: one report on whether this machine
//! and repository are ready to use Branchyard.
//!
//! Each check (in [`checks`]) is a function from what was found
//! ([`Context`]) to rows: a name, a status (`ok`, `todo`, `warn` or `fail`),
//! what was found in one line, and, for a row that is not `ok`, what to do in
//! one line. The checks are listed once, in [`CHECKS`], in report order; to add
//! one, write its function and list it there.
//!
//! The run is offline except for a configured `[remote]`, asked once with a
//! deadline. It reads state and changes none that the user keeps: it opens a
//! `.branchyard/` that exists, as every `by` command does, creates none, and
//! leaves an older store at its schema. Credentials are reported by
//! variable name, never by value. Exit status: 0 when nothing failed
//! (a to-do and a warning do not fail), 1 when something did. A line above
//! the table says which mode this repository runs in. See docs/doctor.md.

mod checks;
mod context;
mod hooks;
mod serve;

use serde::Serialize;
use serde_json::{json, Value};

use crate::args::Globals;
use crate::commands::{print, Env, Failure, Outcome};
use crate::render::{table, Cell, Column, Style, Tone};
use context::Context;

/// How a check came out. Ordered by severity, so the worst of a set is its
/// maximum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Status {
    /// Nothing to do.
    Ok,
    /// A step not taken yet, with nothing wrong: Branchyard has not been set
    /// up in this repository. It is where everyone starts, so it is not a
    /// warning.
    Todo,
    /// Something to look at, or a check that could not run; nothing that
    /// stops Branchyard working.
    Warn,
    /// Something that stops a command working.
    Fail,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Todo => "todo",
            Status::Warn => "warn",
            Status::Fail => "fail",
        }
    }

    fn tone(self) -> Tone {
        match self {
            Status::Ok => Tone::Green,
            Status::Todo => Tone::Cyan,
            Status::Warn => Tone::Yellow,
            Status::Fail => Tone::Red,
        }
    }
}

/// One row of the report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Check {
    /// A stable dotted name, such as `repository.git`; `harnesses.ID` has one
    /// row for each installed harness.
    pub name: String,
    /// How it came out.
    pub status: Status,
    /// What was found, in one line.
    pub detail: String,
    /// What to do about it, in one line; `None` for an `ok` row.
    pub fix: Option<String>,
}

/// `text` as one line: runs of whitespace, newlines included, become one
/// space.
fn one_line(text: impl Into<String>) -> String {
    text.into().split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Check {
    /// A row that needs nothing.
    pub(crate) fn ok(name: impl Into<String>, detail: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            status: Status::Ok,
            detail: one_line(detail),
            fix: None,
        }
    }

    /// A step not taken yet, with the command that takes it.
    pub(crate) fn todo(
        name: impl Into<String>,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Check {
        Check {
            name: name.into(),
            status: Status::Todo,
            detail: one_line(detail),
            fix: Some(one_line(fix)),
        }
    }

    /// A row to look at, with what to do.
    pub(crate) fn warn(
        name: impl Into<String>,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Check {
        Check {
            name: name.into(),
            status: Status::Warn,
            detail: one_line(detail),
            fix: Some(one_line(fix)),
        }
    }

    /// A row that stops something working, with what to do.
    pub(crate) fn fail(
        name: impl Into<String>,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Check {
        Check {
            name: name.into(),
            status: Status::Fail,
            detail: one_line(detail),
            fix: Some(one_line(fix)),
        }
    }
}

/// How the detail of a row that could not run begins.
pub(crate) const NOT_CHECKED: &str = "not checked: ";

/// A check: what it found, as rows.
type CheckFn = fn(&Context) -> Vec<Check>;

/// The checks, in the order the report lists them: the repository, the
/// configuration, the harnesses and whether their hooks were heard, the
/// provider, this host, a configured server and the model gateway.
const CHECKS: &[CheckFn] = &[
    checks::repository_git,
    checks::repository_state,
    checks::repository_schema,
    checks::repository_services,
    checks::config_user,
    checks::config_project,
    checks::config_merged,
    checks::config_workspace_trust,
    checks::harnesses_installed,
    checks::harnesses_default,
    checks::harnesses_credentials,
    hooks::heard,
    checks::provider_default,
    checks::provider_sandboxes,
    checks::host_namespaces,
    checks::host_landlock,
    checks::host_loopback,
    checks::host_kvm,
    checks::server_reachable,
    serve::config,
    serve::database,
    checks::models_keys,
    checks::models_prices,
];

/// The version of the JSON report: 1 had `ok`, `warn` and `fail`, one
/// `harnesses.installed` row, and no `mode`; 2 adds the `todo` status,
/// `harnesses.summary` with one `harnesses.ID` row for each installed
/// harness in its place, and `mode`; 3 adds `hooks`, what was heard of
/// each harness's hooks, beside its `hooks.ID` row.
const FORMAT: u32 = 3;

/// Every check's rows, the mode the repository runs in, and what was heard
/// of the harnesses' hooks.
struct Report {
    /// The `Mode:` line's text, without the label.
    mode: Option<String>,
    checks: Vec<Check>,
    /// The `hooks` array of `--json`.
    hooks: Value,
}

impl Report {
    fn run(context: &Context) -> Report {
        Report {
            mode: Some(checks::mode(context)),
            checks: CHECKS.iter().flat_map(|check| check(context)).collect(),
            hooks: hooks::json(context),
        }
    }

    /// The worst status of any row; `ok` for none.
    fn status(&self) -> Status {
        self.checks
            .iter()
            .map(|c| c.status)
            .max()
            .unwrap_or(Status::Ok)
    }

    fn count(&self, status: Status) -> usize {
        self.checks.iter().filter(|c| c.status == status).count()
    }

    /// `{"format", "mode", "checks": [{"name", "status", "detail", "fix"}],
    /// "hooks": [{"harness", "label", "install", "heard"}], "status"}`.
    fn json(&self) -> Value {
        json!({
            "format": FORMAT,
            "mode": self.mode,
            "checks": self.checks,
            "hooks": self.hooks,
            "status": self.status(),
        })
    }

    /// A table of the rows, what to do about each that is not `ok`, and a
    /// summary line.
    fn text(&self, style: Style) -> String {
        let columns = [
            Column {
                header: "STATUS",
                max: 6,
                right: false,
            },
            Column {
                header: "CHECK",
                max: 40,
                right: false,
            },
            // A refusal is shown whole, however long its path.
            Column {
                header: "DETAIL",
                max: usize::MAX,
                right: false,
            },
        ];
        let rows: Vec<Vec<Cell>> = self
            .checks
            .iter()
            .map(|c| {
                vec![
                    Cell::toned(c.status.as_str(), c.status.tone()),
                    Cell::plain(&c.name),
                    Cell::plain(&c.detail),
                ]
            })
            .collect();
        let mut text = String::new();
        if let Some(mode) = &self.mode {
            text.push_str(&format!("Mode: {mode}\n\n"));
        }
        text.push_str(&table(&columns, &rows, style));
        // A row that could not run only points at the row that failed first.
        let todo: Vec<&Check> = self
            .checks
            .iter()
            .filter(|c| c.fix.is_some() && !c.detail.starts_with(NOT_CHECKED))
            .collect();
        if !todo.is_empty() {
            text.push_str("\nTo do:\n");
            for check in todo {
                if let Some(fix) = &check.fix {
                    text.push_str(&format!("  {}: {fix}\n", check.name));
                }
            }
        }
        text.push('\n');
        text.push_str(&self.summary(style));
        text.push('\n');
        text
    }

    fn summary(&self, style: Style) -> String {
        let status = self.status();
        let (warn, fail, todo) = (
            self.count(Status::Warn),
            self.count(Status::Fail),
            self.count(Status::Todo),
        );
        let plural = |n: usize, one: &str, many: &str| match n {
            1 => format!("1 {one}"),
            n => format!("{n} {many}"),
        };
        let to_do = match todo {
            0 => String::new(),
            n => format!("{n} to do, "),
        };
        let detail = match status {
            Status::Ok => format!("all {} checks passed", self.checks.len()),
            Status::Todo => format!("{to_do}{} ok", self.count(Status::Ok)),
            Status::Warn => format!(
                "{}, {to_do}{} ok",
                plural(warn, "warning", "warnings"),
                self.count(Status::Ok)
            ),
            Status::Fail => format!(
                "{} failed, {}, {to_do}{} ok",
                fail,
                plural(warn, "warning", "warnings"),
                self.count(Status::Ok)
            ),
        };
        format!("{}: {detail}", style.paint(status.tone(), status.as_str()))
    }
}

/// `by doctor`: run every check and print the report. `path` is the
/// repository to look at, else the one `by` would act on from here;
/// `globals` carry `--remote` and the flags and variables beside it.
pub fn main(env: &Env, globals: &Globals, path: Option<&str>, json: bool) -> Outcome {
    let context = Context::new(path, globals)?;
    let progress = (env.stderr_tty && !json).then(cliclack::spinner);
    if let Some(progress) = &progress {
        progress.start("Checking repository, host and configured provider");
    }
    let report = Report::run(&context);
    if let Some(progress) = &progress {
        match report.status() {
            Status::Fail => progress.error("Checks found failures; follow the remedies below"),
            _ => progress.stop("Checks complete"),
        }
    }
    match json {
        true => print(&crate::json::text(&report.json()))?,
        false => print(&report.text(env.style()))?,
    }
    match report.status() {
        Status::Fail => Err(Failure::Reported),
        Status::Ok | Status::Todo | Status::Warn => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> Report {
        Report {
            mode: None,
            hooks: Value::Array(Vec::new()),
            checks: vec![
                Check::ok("repository.git", "/src/app (on branch main)"),
                Check::warn(
                    "config.workspace_trust",
                    "[workspace] has scripts you have not trusted",
                    "run `by workspace trust`",
                ),
                Check::fail(
                    "repository.state",
                    ".branchyard does not exist",
                    "run `by init`",
                ),
            ],
        }
    }

    #[test]
    fn statuses_order_by_severity_and_a_report_is_as_bad_as_its_worst_row() {
        assert!(
            Status::Ok < Status::Todo && Status::Todo < Status::Warn && Status::Warn < Status::Fail
        );
        assert_eq!(report().status(), Status::Fail);
        let mut checks = report().checks;
        checks.pop();
        let hooks = Value::Array(Vec::new());
        assert_eq!(
            Report {
                mode: None,
                checks,
                hooks: hooks.clone()
            }
            .status(),
            Status::Warn
        );
        assert_eq!(
            Report {
                mode: None,
                checks: Vec::new(),
                hooks
            }
            .status(),
            Status::Ok
        );
    }

    #[test]
    fn the_json_has_the_documented_shape() {
        let value = report().json();
        assert_eq!(value["status"], "fail");
        assert_eq!(value["format"], 3);
        assert_eq!(value["mode"], Value::Null);
        assert_eq!(value["hooks"], json!([]));
        let rows = value["checks"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            json!({
                "name": "repository.git",
                "status": "ok",
                "detail": "/src/app (on branch main)",
                "fix": null,
            })
        );
        assert_eq!(rows[1]["status"], "warn");
        assert_eq!(rows[2]["fix"], "run `by init`");
    }

    #[test]
    fn the_table_lists_every_row_then_what_to_do_then_a_summary() {
        let text = report().text(Style::PLAIN);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0].split_whitespace().collect::<Vec<_>>(),
            ["STATUS", "CHECK", "DETAIL"]
        );
        assert!(lines[1].starts_with("ok      repository.git "), "{text}");
        assert!(
            lines[2].starts_with("warn    config.workspace_trust "),
            "{text}"
        );
        assert!(lines[3].starts_with("fail    repository.state "), "{text}");
        assert!(text.contains("\nTo do:\n  config.workspace_trust: run `by workspace trust`\n"));
        assert!(text.contains("  repository.state: run `by init`\n"));
        assert!(
            !text.contains("repository.git:"),
            "an ok row has nothing to do"
        );
        assert_eq!(
            lines.last().copied(),
            Some("fail: 1 failed, 1 warning, 1 ok")
        );
    }

    #[test]
    fn the_mode_is_one_line_above_the_table_and_a_key_in_the_json() {
        let report = Report {
            mode: Some("local, no isolation · SQLite (.branchyard/state.db) · no server".into()),
            hooks: Value::Array(Vec::new()),
            checks: vec![Check::ok("a.b", "x")],
        };
        let text = report.text(Style::PLAIN);
        assert!(
            text.starts_with(
                "Mode: local, no isolation · SQLite (.branchyard/state.db) · no server\n\nSTATUS"
            ),
            "{text}"
        );
        assert_eq!(
            report.json()["mode"],
            "local, no isolation · SQLite (.branchyard/state.db) · no server"
        );
    }

    #[test]
    fn the_summary_counts_each_status() {
        let ok = Report {
            mode: None,
            hooks: Value::Array(Vec::new()),
            checks: vec![Check::ok("a.b", "x"), Check::ok("a.c", "y")],
        };
        assert!(ok.text(Style::PLAIN).ends_with("ok: all 2 checks passed\n"));
        let warned = Report {
            mode: None,
            hooks: Value::Array(Vec::new()),
            checks: vec![
                Check::ok("a.b", "x"),
                Check::warn("a.c", "y", "z"),
                Check::warn("a.d", "y", "z"),
            ],
        };
        assert!(warned
            .text(Style::PLAIN)
            .ends_with("warn: 2 warnings, 1 ok\n"));
        // A to-do is not a warning, and nothing broken exits 0.
        let new = Report {
            mode: None,
            hooks: Value::Array(Vec::new()),
            checks: vec![
                Check::ok("a.b", "x"),
                Check::todo("a.c", "not set up yet", "run `by setup`"),
            ],
        };
        assert_eq!(new.status(), Status::Todo);
        let text = new.text(Style::PLAIN);
        assert!(text.contains("todo    a.c "), "{text}");
        assert!(text.contains("To do:\n  a.c: run `by setup`\n"), "{text}");
        assert!(text.ends_with("todo: 1 to do, 1 ok\n"), "{text}");
        let mixed = Report {
            mode: None,
            hooks: Value::Array(Vec::new()),
            checks: vec![
                Check::todo("a.c", "x", "y"),
                Check::warn("a.d", "x", "y"),
                Check::ok("a.e", "x"),
            ],
        };
        assert!(mixed
            .text(Style::PLAIN)
            .ends_with("warn: 1 warning, 1 to do, 1 ok\n"));
    }

    #[test]
    fn a_detail_is_never_cut_and_is_one_line() {
        let long = "x".repeat(400);
        let check = Check::fail(
            "config.project",
            format!("/a/branchyard.toml: line 3:\n  unknown field `harnes`, {long}"),
            "fix the\n line",
        );
        assert!(!check.detail.contains('\n') && !check.fix.as_deref().unwrap().contains('\n'));
        assert_eq!(check.fix.as_deref(), Some("fix the line"));
        let text = Report {
            mode: None,
            hooks: Value::Array(Vec::new()),
            checks: vec![check],
        }
        .text(Style::PLAIN);
        assert!(text.contains(&long), "the table cut a refusal");
        assert!(text.contains("line 3: unknown field `harnes`"));
    }

    #[test]
    fn color_paints_the_status_and_the_summary_only() {
        let text = report().text(Style { color: true });
        assert!(text.contains("\x1b[32mok\x1b[0m"), "{text:?}");
        assert!(text.contains("\x1b[33mwarn\x1b[0m"), "{text:?}");
        assert!(text.contains("\x1b[31mfail\x1b[0m: 1 failed"), "{text:?}");
    }
}
