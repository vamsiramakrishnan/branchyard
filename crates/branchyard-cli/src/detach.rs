//! `--detach` on `by run`, `by send` and `by fork` in local mode: the same
//! command line, started again in the background the way `by watch` starts
//! a turn ([`spawn`]: its own process group, no standard input, its output
//! in a log file), so the turn outlives the terminal and this process.
//!
//! The background command learns it is one from [`REPORT_ENV`], which
//! names the write end of a pipe it inherited ([`spawn_reporting`]). It
//! reports there ([`Reporter`]), one JSON line each: the branch its first
//! event is about, then that the branch's prompt was recorded, which the
//! engine does once the harness is up and the turn holds its lease, just
//! before the prompt is submitted; or that it failed first, with why. Then
//! it closes the pipe. The launching command blocks reading the pipe until
//! the turn started, the command failed or ended (the pipe closes), or
//! [`STARTS`] passed, and then prints the branch and how to follow it.
//! `by review --detach` waits on its background `by send` the same way.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use branchyard::{Activity, BranchEvent};
use branchyard_support::LockExt as _;
use serde::{Deserialize, Serialize};

use crate::args::{Command as Parsed, Permissions};
use crate::commands::{self, Failure, Outcome, Target};

/// In the background command's environment: the number of the inherited
/// file descriptor, the write end of a pipe, it reports to.
pub const REPORT_ENV: &str = "BRANCHYARD_DETACH_FD";

/// How long the launching command waits for the turn to start before it
/// returns anyway, saying the turn has not started yet.
const STARTS: Duration = Duration::from_secs(120);

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

/// [`spawn`], with a pipe the command reports to ([`REPORT_ENV`]); read
/// what it reports with [`Handshake::wait`].
#[cfg(unix)]
pub fn spawn_reporting(command: &mut Command, log: &Path) -> io::Result<(Child, Handshake)> {
    use std::os::fd::{AsRawFd as _, BorrowedFd};
    use std::os::unix::process::CommandExt as _;
    let (reader, writer) = io::pipe()?;
    let fd = writer.as_raw_fd();
    command.env(REPORT_ENV, fd.to_string());
    // SAFETY: runs in the forked child before exec, and only clears the
    // close-on-exec flag of a descriptor the child inherited, so the
    // command keeps the write end (and nothing else this process starts
    // does). fcntl is async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            rustix::io::fcntl_setfd(BorrowedFd::borrow_raw(fd), rustix::io::FdFlags::empty())
                .map_err(io::Error::from)
        });
    }
    let child = spawn(command, log)?;
    // Only the command holds the write end now: its closing it, or
    // exiting, ends the launcher's read.
    drop(writer);
    Ok((child, Handshake { reader }))
}

/// [`spawn`] where there are no inherited pipes: nothing is reported, and
/// [`Handshake::wait`] returns at once.
#[cfg(not(unix))]
pub fn spawn_reporting(command: &mut Command, log: &Path) -> io::Result<(Child, Handshake)> {
    Ok((spawn(command, log)?, Handshake {}))
}

/// The read end of a background command's report pipe.
pub struct Handshake {
    #[cfg(unix)]
    reader: io::PipeReader,
}

impl Handshake {
    /// Block until `child` reports that its turn started or that it
    /// failed, closes the pipe (it ended), or `within` passed. Says what it
    /// reported and, if it has exited, how.
    #[cfg(unix)]
    pub fn wait(
        self,
        child: &mut Child,
        within: Duration,
    ) -> io::Result<(Report, Option<ExitStatus>)> {
        use std::io::BufRead as _;
        use std::sync::mpsc::{self, RecvTimeoutError};
        let (tx, rx) = mpsc::channel();
        let reader = self.reader;
        // A thread, so the read can time out; it ends with the pipe.
        std::thread::spawn(move || {
            for line in io::BufReader::new(reader).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        let deadline = std::time::Instant::now() + within;
        let mut report = Report::default();
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(left) {
                Ok(line) => {
                    report.apply(&line);
                    if report.started {
                        return Ok((report, child.try_wait()?));
                    }
                    if report.failed.is_some() {
                        return Ok((report, Some(child.wait()?)));
                    }
                }
                // Closed without a start: the command ended.
                Err(RecvTimeoutError::Disconnected) => return Ok((report, Some(child.wait()?))),
                Err(RecvTimeoutError::Timeout) => return Ok((report, child.try_wait()?)),
            }
        }
    }

    #[cfg(not(unix))]
    pub fn wait(
        self,
        child: &mut Child,
        _within: Duration,
    ) -> io::Result<(Report, Option<ExitStatus>)> {
        Ok((Report::default(), child.try_wait()?))
    }
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

/// Reports the background command's branch and the start of its turn, or
/// its failure, to the launcher's pipe, once each; see the module's
/// documentation.
pub struct Reporter {
    seen: Mutex<Reporting>,
}

struct Reporting {
    /// The pipe, until the turn started or the command failed.
    sink: Option<Box<dyn Write + Send>>,
    report: Report,
}

/// What a [`Reporter`] wrote, read back by the launcher.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// The branch the first event was about.
    pub branch: Option<String>,
    /// Its prompt was recorded, or the server accepted it: the turn
    /// started.
    pub started: bool,
    /// The command failed before that, and why.
    pub failed: Option<String>,
}

/// One line a [`Reporter`] writes.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Line {
    Branch {
        branch: String,
    },
    Started {
        branch: String,
    },
    Failed {
        branch: Option<String>,
        detail: String,
    },
}

impl Report {
    /// Take in one line of a report; one that does not parse is ignored.
    pub fn apply(&mut self, line: &str) {
        let Ok(line) = serde_json::from_str::<Line>(line) else {
            return;
        };
        match line {
            Line::Branch { branch } => {
                self.branch.get_or_insert(branch);
            }
            Line::Started { branch } => {
                self.branch.get_or_insert(branch);
                self.started = true;
            }
            Line::Failed { branch, detail } => {
                if let Some(branch) = branch {
                    self.branch.get_or_insert(branch);
                }
                self.failed = Some(detail);
            }
        }
    }
}

impl Reporter {
    pub fn new(sink: Option<Box<dyn Write + Send>>) -> Reporter {
        Reporter {
            seen: Mutex::new(Reporting {
                sink,
                report: Report::default(),
            }),
        }
    }

    /// Note `event`: the first names the branch; that branch's prompt
    /// starts its turn. Nothing is written after that.
    pub fn observe(&self, event: &BranchEvent) {
        let mut seen = self.seen.lock_recovering("detached report");
        if seen.sink.is_none() {
            return;
        }
        if seen.report.branch.is_none() {
            seen.report.branch = Some(event.branch.clone());
            seen.write(&Line::Branch {
                branch: event.branch.clone(),
            });
        }
        if seen.report.branch.as_deref() == Some(event.branch.as_str())
            && matches!(event.activity, Activity::Prompt(_))
        {
            seen.started(&event.branch);
        }
    }

    /// The server accepted a turn for `branch` (a remote `by send`): it is
    /// queued or running there, as started as it gets from here.
    pub fn accepted(&self, branch: &str) {
        let mut seen = self.seen.lock_recovering("detached report");
        if seen.sink.is_some() {
            seen.started(branch);
        }
    }

    /// The command failed, with `detail`, before its turn started.
    pub fn failed(&self, detail: &str) {
        let mut seen = self.seen.lock_recovering("detached report");
        let line = Line::Failed {
            branch: seen.report.branch.clone(),
            detail: detail.to_owned(),
        };
        seen.write(&line);
        seen.sink = None;
    }
}

impl Reporting {
    fn started(&mut self, branch: &str) {
        self.report.started = true;
        self.write(&Line::Started {
            branch: branch.to_owned(),
        });
        // Closing it is what the launcher waits for.
        self.sink = None;
    }

    fn write(&mut self, line: &Line) {
        let Some(sink) = self.sink.as_mut() else {
            return;
        };
        let written = serde_json::to_string(line)
            .map_err(io::Error::from)
            .and_then(|text| sink.write_all(format!("{text}\n").as_bytes()))
            .and_then(|()| sink.flush());
        branchyard_support::best_effort("report to the launching by", written);
    }
}

static REPORTER: OnceLock<Reporter> = OnceLock::new();

/// Whether this process is a background command `--detach` (or `by review
/// --detach`) started; if so, its branch events are reported from now on
/// ([`observe`]). The variable is removed, and the pipe closed on exec, so
/// the harness and what it runs inherit neither. `main` calls this first,
/// before it starts anything that could hold the pipe open.
pub fn become_child() -> bool {
    match std::env::var(REPORT_ENV).ok().filter(|v| !v.is_empty()) {
        Some(fd) => {
            std::env::remove_var(REPORT_ENV);
            REPORTER.get_or_init(|| Reporter::new(inherited(&fd)));
            true
        }
        None => REPORTER.get().is_some(),
    }
}

/// The pipe the launcher passed as descriptor `fd`, owned from now on and
/// closed on exec.
#[cfg(unix)]
fn inherited(fd: &str) -> Option<Box<dyn Write + Send>> {
    use std::os::fd::{BorrowedFd, FromRawFd as _, OwnedFd, RawFd};
    // Never standard input, output or error.
    let fd: RawFd = fd.parse().ok().filter(|fd| *fd > 2)?;
    // SAFETY: borrowed only for fcntl, which fails harmlessly on a
    // descriptor that is not open.
    rustix::io::fcntl_getfd(unsafe { BorrowedFd::borrow_raw(fd) }).ok()?;
    // SAFETY: it is open, and the launching `by` passed it to this process
    // alone to report on: nothing else here uses or closes it.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    branchyard_support::best_effort(
        "keep the report pipe from the harness",
        rustix::io::fcntl_setfd(&owned, rustix::io::FdFlags::CLOEXEC),
    );
    Some(Box::new(fs::File::from(owned)))
}

#[cfg(not(unix))]
fn inherited(_fd: &str) -> Option<Box<dyn Write + Send>> {
    None
}

/// Report `event` when this is a background command; see [`become_child`].
pub fn observe(event: &BranchEvent) {
    if let Some(reporter) = REPORTER.get() {
        reporter.observe(event);
    }
}

/// Report that the server accepted a turn for `branch`, when this is a
/// background command.
pub fn accepted(branch: &str) {
    if let Some(reporter) = REPORTER.get() {
        reporter.accepted(branch);
    }
}

/// Report a background command's failure, if its turn had not started:
/// the launcher shows the end of its log.
pub fn finish(outcome: &Outcome) {
    if let (Some(reporter), Err(failure)) = (REPORTER.get(), outcome) {
        reporter.failed(&failure.to_string());
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
        return Err(crate::errors::Report::usage(why).into());
    }
    let yard = commands::open()?;
    let dir = yard.root().join(".branchyard").join("detached");
    let stamp = branchyard_support::time::now_ms();
    let log = dir.join(format!("{}-{stamp}.log", launch.command));
    let mut command = Command::new(std::env::current_exe()?);
    command.args(std::env::args_os().skip(1));
    let (mut child, handshake) = spawn_reporting(&mut command, &log)?;
    let pid = child.id();
    let (seen, exited) = handshake.wait(&mut child, STARTS)?;
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
        let found = requested(&parse("send b --detach --prompt-file p.md")).unwrap();
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

    /// A sink whose bytes the test reads back.
    #[derive(Clone, Default)]
    struct Shared(std::sync::Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock_recovering("test sink").extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock_recovering("test sink")).into_owned()
        }
    }

    fn parse(text: &str) -> Report {
        let mut report = Report::default();
        for line in text.lines() {
            report.apply(line);
        }
        report
    }

    #[test]
    fn the_reporter_names_the_first_branch_and_the_start_of_its_turn() {
        let sink = Shared::default();
        let reporter = Reporter::new(Some(Box::new(sink.clone())));
        reporter.observe(&event(
            "a",
            Activity::Status(branchyard::BranchStatus::Running),
        ));
        assert_eq!(
            parse(&sink.text()),
            Report {
                branch: Some("a".into()),
                ..Report::default()
            }
        );
        // Another branch's prompt (a delegated child) is not this turn's.
        reporter.observe(&event("a-child", Activity::Prompt("x".into())));
        reporter.observe(&event("a", Activity::Prompt("go".into())));
        reporter.observe(&event("a", Activity::Prompt("again".into())));
        // Nothing after the start, a failure included.
        reporter.failed("late");
        let text = sink.text();
        assert_eq!(
            text,
            "{\"state\":\"branch\",\"branch\":\"a\"}\n{\"state\":\"started\",\"branch\":\"a\"}\n"
        );
        assert_eq!(
            parse(&text),
            Report {
                branch: Some("a".into()),
                started: true,
                failed: None
            }
        );
        assert_eq!(parse("started b\n"), Report::default());

        // A failure before the start carries the branch, if known, and why.
        let sink = Shared::default();
        let reporter = Reporter::new(Some(Box::new(sink.clone())));
        reporter.observe(&event(
            "c",
            Activity::Status(branchyard::BranchStatus::Running),
        ));
        reporter.failed("no harness");
        reporter.accepted("c");
        assert_eq!(
            parse(&sink.text()),
            Report {
                branch: Some("c".into()),
                started: false,
                failed: Some("no harness".into())
            }
        );
        // A remote send is started once the server accepts it.
        let sink = Shared::default();
        Reporter::new(Some(Box::new(sink.clone()))).accepted("d");
        assert!(parse(&sink.text()).started);
    }

    /// The launcher reads the pipe: a start returns at once with the
    /// command still running, a close without one is the command's exit,
    /// and nothing within the time returns with neither.
    #[test]
    fn the_handshake_returns_on_a_start_an_exit_or_the_time_limit() {
        let scratch = Scratch::new("detach-handshake");
        let log = scratch.path().join("h.log");
        let sh = |script: &str| {
            let mut command = Command::new("sh");
            command.args(["-c", script]);
            spawn_reporting(&mut command, &log).unwrap()
        };
        // The command still holds the pipe (the sleep inherits it), so
        // only the line can end the wait.
        let (mut child, handshake) = sh(
            "printf '%s\\n' '{\"state\":\"started\",\"branch\":\"b\"}' >&$BRANCHYARD_DETACH_FD; \
             exec sleep 30",
        );
        let started = std::time::Instant::now();
        let (report, exited) = handshake.wait(&mut child, STARTS).unwrap();
        assert!(started.elapsed() < Duration::from_secs(20));
        assert_eq!(report.branch.as_deref(), Some("b"));
        assert!(report.started && exited.is_none());
        branchyard_support::best_effort("kill", child.kill());
        branchyard_support::best_effort("reap", child.wait());

        let (mut child, handshake) = sh(
            "printf '%s\\n' '{\"state\":\"branch\",\"branch\":\"c\"}' >&$BRANCHYARD_DETACH_FD; \
             echo bye; exit 3",
        );
        let (report, exited) = handshake.wait(&mut child, STARTS).unwrap();
        assert_eq!(report.branch.as_deref(), Some("c"));
        assert!(!report.started);
        assert_eq!(exited.and_then(|status| status.code()), Some(3));
        assert!(fs::read_to_string(&log).unwrap().contains("bye"));

        let (mut child, handshake) = sh("exec sleep 30");
        let (report, exited) = handshake
            .wait(&mut child, Duration::from_millis(200))
            .unwrap();
        assert_eq!((report, exited), (Report::default(), None));
        branchyard_support::best_effort("kill", child.kill());
        branchyard_support::best_effort("reap", child.wait());
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
