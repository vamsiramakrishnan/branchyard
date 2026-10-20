//! `--detach` on `by run`, `by send` and `by fork` in local mode: the same
//! command line, started again in the background the way `by watch` starts
//! a turn ([`spawn`]: its own process group, no standard input, its output
//! in a log file), so the turn outlives the terminal and this process.
//!
//! The background command learns it is one from [`REPORT_ENV`], which
//! names a file it reports to ([`Reporter`]): the branch its first event is
//! about, then that the branch's prompt was recorded, which the engine does
//! once the harness is up and the turn holds its lease, just before the
//! prompt is submitted. The launching command waits for that report, or for
//! the background command to exit, and then prints the branch and how to
//! follow it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use branchyard::{Activity, BranchEvent};
use branchyard_support::LockExt as _;
use serde::Serialize;

use crate::args::{Command as Parsed, Permissions};
use crate::commands::{self, Failure, Outcome, Target};

/// In the background command's environment: the file it reports to.
pub const REPORT_ENV: &str = "BRANCHYARD_DETACH_REPORT";

/// How long the launching command waits for the turn to start before it
/// returns anyway, saying the turn has not started yet.
const STARTS: Duration = Duration::from_secs(120);

/// How often the launching command looks at the report.
const POLL: Duration = Duration::from_millis(25);

/// Lines of the log shown when the background command fails.
const TAIL: usize = 40;

/// Start `command` in the background: in its own process group, so the
/// terminal's signals and the launcher exiting leave it running, with
/// standard input from `/dev/null` and its output appended to `log`
/// (created, with its directory, if missing). `by watch`, `by review
/// --detach` and `--detach` all start their commands this way.
pub fn spawn(command: &mut Command, log: &Path) -> io::Result<Child> {
    if let Some(dir) = log.parent() {
        fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new().create(true).append(true).open(log)?;
    command
        .stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
    command.spawn()
}

/// A `--detach` the command line asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    /// `run`, `send` or `fork`.
    pub command: &'static str,
    pub permissions: Permissions,
    /// Print the result as JSON.
    pub json: bool,
    /// `by send --prompt-file`.
    pub prompt_file: Option<String>,
}

/// The `--detach` in `command`, if it has one.
pub fn requested(command: &Parsed) -> Option<Launch> {
    let launch = |command, task: &crate::args::TaskArgs, json, prompt_file: &Option<String>| {
        Some(Launch {
            command,
            permissions: task.permissions,
            json,
            prompt_file: prompt_file.clone(),
        })
    };
    match command {
        Parsed::Run { detached, task, .. } if detached.detach => {
            launch("run", task, detached.json, &None)
        }
        Parsed::Fork { detached, task, .. } if detached.detach => {
            launch("fork", task, detached.json, &None)
        }
        Parsed::Send {
            detach: true,
            json,
            prompt_file,
            task,
            ..
        } => launch("send", task, *json, prompt_file),
        _ => None,
    }
}

/// Why this `--detach` cannot run: in remote mode, inside a harness, or
/// with an answer only a terminal can give.
pub fn refusal(launch: &Launch, remote: bool, in_harness: bool) -> Option<String> {
    if remote {
        return Some(format!(
            "--detach is for local mode: with --remote the turn already runs on the server and \
             outlives this command (Ctrl-C stops only the following); run by {} without \
             --detach and follow it again with by log -f or by watch",
            launch.command
        ));
    }
    if in_harness {
        return Some(
            "--detach is for a person's terminal; inside a harness, by spawn and by send \
             return while the child runs"
                .into(),
        );
    }
    if launch.permissions == Permissions::Ask {
        // `--ask` itself is a usage error; this is the configuration's.
        return Some(
            "permissions = \"ask\" in the configuration asks on a terminal, and a detached turn \
             has none: pass --yes or --permissions PRESET with --detach"
                .into(),
        );
    }
    if launch.prompt_file.as_deref() == Some("-") {
        return Some(
            "--prompt-file - reads standard input, which a detached turn does not have: pass \
             the prompt or a file"
                .into(),
        );
    }
    None
}

/// Reports the background command's branch and the start of its turn to
/// a file, once each; see the module's documentation.
pub struct Reporter {
    path: PathBuf,
    seen: Mutex<Report>,
}

/// What a [`Reporter`] wrote, read back by the launcher.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// The branch the first event was about.
    pub branch: Option<String>,
    /// Its prompt was recorded: the turn started.
    pub started: bool,
}

impl Report {
    /// Parse a report file's lines (`branch NAME`, `started NAME`).
    pub fn parse(text: &str) -> Report {
        let mut report = Report::default();
        for line in text.lines() {
            match line.split_once(' ') {
                Some(("branch", name)) if report.branch.is_none() => {
                    report.branch = Some(name.to_owned())
                }
                Some(("started", name)) if report.branch.as_deref() == Some(name) => {
                    report.started = true
                }
                _ => {}
            }
        }
        report
    }
}

impl Reporter {
    pub fn new(path: PathBuf) -> Reporter {
        Reporter {
            path,
            seen: Mutex::new(Report::default()),
        }
    }

    /// Note `event`: the first names the branch; that branch's prompt
    /// starts its turn. Nothing is written after that.
    pub fn observe(&self, event: &BranchEvent) {
        let mut seen = self.seen.lock_recovering("detached report");
        if seen.started {
            return;
        }
        if seen.branch.is_none() {
            seen.branch = Some(event.branch.clone());
            self.write(&format!("branch {}\n", event.branch));
        }
        if seen.branch.as_deref() == Some(event.branch.as_str())
            && matches!(event.activity, Activity::Prompt(_))
        {
            seen.started = true;
            self.write(&format!("started {}\n", event.branch));
        }
    }

    fn write(&self, line: &str) {
        let written = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut file| file.write_all(line.as_bytes()));
        branchyard_support::best_effort("report to the launching by", written);
    }
}

static REPORTER: OnceLock<Reporter> = OnceLock::new();

/// Whether this process is a background command `--detach` started; if so,
/// its branch events are reported from now on ([`observe`]). The variable
/// is removed so the harness and what it runs do not inherit it.
pub fn become_child() -> bool {
    match std::env::var_os(REPORT_ENV).filter(|v| !v.is_empty()) {
        Some(path) => {
            std::env::remove_var(REPORT_ENV);
            REPORTER.get_or_init(|| Reporter::new(PathBuf::from(path)));
            true
        }
        None => REPORTER.get().is_some(),
    }
}

/// Report `event` when this is a background command; see [`become_child`].
pub fn observe(event: &BranchEvent) {
    if let Some(reporter) = REPORTER.get() {
        reporter.observe(event);
    }
}

/// What `--detach` printed, as `--json` prints it.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Detached {
    /// The branch, unless the command had not named it yet.
    pub branch: Option<String>,
    /// `run`, `send` or `fork`.
    pub command: String,
    /// The background command's process ID.
    pub pid: u32,
    /// Its output.
    pub log: String,
    /// Its turn had started when this returned.
    pub started: bool,
    /// It had already ended, successfully.
    pub exited: bool,
}

/// `--detach`: start the command line again in the background, wait until
/// its turn has started or it exits, and say how to follow it.
pub fn launch(target: &Target, launch: &Launch) -> Outcome {
    let in_harness = std::env::var_os(branchyard::ENV_BRANCH).is_some_and(|v| !v.is_empty());
    if let Some(why) = refusal(launch, matches!(target, Target::Remote(_)), in_harness) {
        return Err(Failure::Message(why));
    }
    let yard = commands::open()?;
    let dir = yard.root().join(".branchyard").join("detached");
    let stamp = branchyard_support::time::now_ms();
    let log = dir.join(format!("{}-{stamp}.log", launch.command));
    let report = dir.join(format!("{}-{stamp}.report", launch.command));
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(std::env::args_os().skip(1))
        .env(REPORT_ENV, &report);
    let mut child = spawn(&mut command, &log)?;
    let pid = child.id();
    let (seen, exited) = wait_for_start(&mut child, &report)?;
    branchyard_support::cleanup_file(&report);
    // One log per branch, once the branch is known.
    let log = match &seen.branch {
        Some(branch) => {
            let named = dir.join(log_name(branch, launch.command, stamp));
            match branchyard_support::best_effort(
                "name the log after its branch",
                fs::rename(&log, &named),
            ) {
                Some(()) => named,
                None => log,
            }
        }
        None => log,
    };
    if let Some(status) = exited.filter(|status| !status.success()) {
        eprintln!(
            "by: the background by {} failed ({status}); the end of {}:\n{}",
            launch.command,
            log.display(),
            tail(&fs::read_to_string(&log).unwrap_or_default())
        );
        return Err(Failure::Reported);
    }
    if launch.permissions == Permissions::Unset {
        eprintln!("by: {}", DENY_NOTICE);
    }
    let detached = Detached {
        branch: seen.branch,
        command: launch.command.to_owned(),
        pid,
        log: log.display().to_string(),
        started: seen.started,
        exited: exited.is_some(),
    };
    match launch.json {
        true => commands::print(&format!("{}\n", commands::to_json(&detached))),
        false => commands::print(&text(&detached)),
    }
}

/// Said when a detached turn's permission requests have no answer but a
/// denial.
const DENY_NOTICE: &str = "the detached turn has no terminal, so tool permission requests are \
     denied; pass --yes or --permissions PRESET with --detach to allow them";

/// Poll `report` until the turn started, `child` exited, or [`STARTS`]
/// passed. Whether it exited is read first, so a report written just
/// before it exited is seen.
fn wait_for_start(child: &mut Child, report: &Path) -> io::Result<(Report, Option<ExitStatus>)> {
    let deadline = Instant::now() + STARTS;
    loop {
        let exited = child.try_wait()?;
        let seen = Report::parse(&fs::read_to_string(report).unwrap_or_default());
        if seen.started || exited.is_some() || Instant::now() >= deadline {
            return Ok((seen, exited));
        }
        std::thread::sleep(POLL);
    }
}

/// `<branch>-<command>-<stamp>.log`, the branch's name made safe for a
/// file name.
fn log_name(branch: &str, command: &str, stamp: u64) -> String {
    let safe: String = branch
        .chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                true => c,
                false => '_',
            },
        )
        .collect();
    format!("{safe}-{command}-{stamp}.log")
}

/// The last [`TAIL`] lines of `log`, indented.
fn tail(log: &str) -> String {
    let lines: Vec<&str> = log.lines().collect();
    lines[lines.len().saturating_sub(TAIL)..]
        .iter()
        .map(|line| format!("  {line}\n"))
        .collect()
}

/// What `--detach` prints without `--json`.
fn text(d: &Detached) -> String {
    let Some(branch) = &d.branch else {
        return format!(
            "by {} is still starting in the background (pid {}); its output goes to {}\n",
            d.command, d.pid, d.log
        );
    };
    let state = match (d.started, d.exited) {
        (_, true) => "has already ended",
        (true, false) => "runs in the background",
        (false, false) => "is starting in the background; its turn has not started yet",
    };
    format!(
        "{branch} {state} (pid {}); its output goes to {}\n  \
         follow: by log -f {branch}   (or by watch)\n  \
         wait:   by wait {branch}\n  \
         stop:   by cancel {branch}\n",
        d.pid, d.log
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use branchyard_testkit::Scratch;

    fn launch(permissions: Permissions, prompt_file: Option<&str>) -> Launch {
        Launch {
            command: "run",
            permissions,
            json: false,
            prompt_file: prompt_file.map(str::to_owned),
        }
    }

    #[test]
    fn detach_is_found_on_run_send_and_fork_only() {
        let parse = |line: &str| {
            let argv: Vec<String> = std::iter::once("by".to_owned())
                .chain(line.split(' ').map(str::to_owned))
                .collect();
            crate::args::parse_from(argv).unwrap().command.unwrap()
        };
        let found = requested(&parse("run go --detach --json --yes")).unwrap();
        assert_eq!(found.command, "run");
        assert!(found.json);
        assert_eq!(found.permissions, Permissions::Yes);
        let found = requested(&parse("send b go --detach --prompt-file p.md")).unwrap();
        assert_eq!((found.command, found.json), ("send", false));
        assert_eq!(found.prompt_file.as_deref(), Some("p.md"));
        assert_eq!(
            requested(&parse("fork b go --detach")).unwrap().command,
            "fork"
        );
        assert_eq!(requested(&parse("run go")), None);
        assert_eq!(requested(&parse("send b go --json")), None);
    }

    #[test]
    fn what_a_detached_turn_cannot_do_is_refused() {
        let plain = launch(Permissions::Unset, None);
        assert_eq!(refusal(&plain, false, false), None);
        assert!(refusal(&plain, true, false).unwrap().contains("local mode"));
        assert!(refusal(&plain, false, true)
            .unwrap()
            .contains("inside a harness"));
        let ask = launch(Permissions::Ask, None);
        assert!(refusal(&ask, false, false).unwrap().contains("--yes"));
        let stdin = launch(Permissions::Yes, Some("-"));
        assert!(refusal(&stdin, false, false)
            .unwrap()
            .contains("standard input"));
        assert_eq!(
            refusal(&launch(Permissions::Yes, Some("p.md")), false, false),
            None
        );
    }

    fn event(branch: &str, activity: Activity) -> BranchEvent {
        BranchEvent {
            branch: branch.into(),
            activity,
        }
    }

    #[test]
    fn the_reporter_names_the_first_branch_and_the_start_of_its_turn() {
        let scratch = Scratch::new("detach-report");
        let path = scratch.path().join("r");
        let reporter = Reporter::new(path.clone());
        reporter.observe(&event(
            "a",
            Activity::Status(branchyard::BranchStatus::Running),
        ));
        assert_eq!(
            Report::parse(&fs::read_to_string(&path).unwrap()),
            Report {
                branch: Some("a".into()),
                started: false
            }
        );
        // Another branch's prompt (a delegated child) is not this turn's.
        reporter.observe(&event("a-child", Activity::Prompt("x".into())));
        reporter.observe(&event("a", Activity::Prompt("go".into())));
        reporter.observe(&event("a", Activity::Prompt("again".into())));
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, "branch a\nstarted a\n");
        assert_eq!(
            Report::parse(&text),
            Report {
                branch: Some("a".into()),
                started: true
            }
        );
        assert_eq!(Report::parse("started b\n"), Report::default());
    }

    #[test]
    fn spawn_detaches_and_logs_both_streams() {
        let scratch = Scratch::new("detach-spawn");
        let log = scratch.path().join("logs/x.log");
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "echo out; echo err >&2; read line || echo no-stdin; \
             echo \"group $(cut -d' ' -f5 /proc/$$/stat)\"",
        ]);
        let mut child = spawn(&mut command, &log).unwrap();
        let pid = child.id();
        assert!(child.wait().unwrap().success());
        let text = fs::read_to_string(&log).unwrap();
        assert!(text.contains("out\n") && text.contains("err\n") && text.contains("no-stdin"));
        // Its own process group: the group's ID is its own process ID.
        assert!(text.contains(&format!("group {pid}\n")), "{text}");
    }

    #[test]
    fn names_tails_and_text() {
        assert_eq!(log_name("feat/x y", "run", 7), "feat_x_y-run-7.log");
        let long: String = (0..50).map(|n| format!("{n}\n")).collect();
        let shown = tail(&long);
        assert!(shown.starts_with("  10\n") && shown.ends_with("  49\n"));
        let mut d = Detached {
            branch: Some("b".into()),
            command: "run".into(),
            pid: 9,
            log: "/l".into(),
            started: true,
            exited: false,
        };
        let shown = text(&d);
        assert!(shown.starts_with("b runs in the background (pid 9); its output goes to /l"));
        for follow in ["by log -f b", "by watch", "by wait b", "by cancel b"] {
            assert!(shown.contains(follow), "{follow}: {shown}");
        }
        d.started = false;
        assert!(text(&d).contains("has not started yet"));
        d.branch = None;
        assert!(text(&d).starts_with("by run is still starting"));
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["log"], "/l");
        assert_eq!(json["branch"], serde_json::Value::Null);
    }
}
