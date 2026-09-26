//! Run driver qualification scenarios against a real harness binary.
//!
//! This is a development tool, not the Branchyard node. It starts the
//! harness as a local process with a scrubbed environment and a private
//! home, standing in for `SandboxProvider.exec`, and drives it through the
//! profile's driver. It qualifies protocol behavior against the real binary:
//! turns, permission answers, interrupts, resume, fork and a lost connection.
//! It does not qualify sandbox isolation.
//!
//! Scenarios make real model calls and spend the credentials the harness
//! finds. `--max-cost-usd` stops the run once the harness's own cost
//! estimates exceed the cap; a profile that reports no cost is not capped.
//!
//! ```text
//! branchyard-qualify --profile claude-code-stream-json --workdir DIR [--command PATH]
//!     [--max-cost-usd 3] [--keep-env NAME]... [--report FILE]
//! ```

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use branchyard_harness::profiles::{self, Profile};
use branchyard_harness::{
    Driver, Event, NativeSession, Open, PermissionDecision, PermissionRequest, SessionMode,
    TurnOutcome,
};
use serde_json::{json, Value};

const CODE_WORD: &str = "PELICAN-7";

struct Config {
    profile: &'static Profile,
    command: Vec<String>,
    workdir: PathBuf,
    max_cost_usd: f64,
    keep_env: BTreeSet<String>,
    report: Option<PathBuf>,
}

fn parse_args() -> Result<Config, String> {
    let mut args = std::env::args().skip(1);
    let (mut profile, mut command, mut workdir, mut report) = (None, None, None, None);
    let mut max_cost_usd = 3.0;
    let mut keep_env = BTreeSet::new();
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--profile" => profile = Some(value()?),
            "--command" => command = Some(value()?),
            "--workdir" => workdir = Some(PathBuf::from(value()?)),
            "--report" => report = Some(PathBuf::from(value()?)),
            "--max-cost-usd" => {
                max_cost_usd = value()?
                    .parse()
                    .map_err(|e| format!("--max-cost-usd: {e}"))?
            }
            "--keep-env" => {
                keep_env.insert(value()?);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let id = profile.ok_or("--profile is required")?;
    let profile = profiles::by_id(&id).ok_or(format!("unknown profile {id}"))?;
    let mut argv: Vec<String> = profile.command.iter().map(|s| (*s).to_owned()).collect();
    if let Some(command) = command {
        argv[0] = command;
    }
    Ok(Config {
        profile,
        command: argv,
        workdir: fs::canonicalize(workdir.ok_or("--workdir is required")?)
            .map_err(|e| format!("--workdir: {e}"))?,
        max_cost_usd,
        keep_env,
        report,
    })
}

/// A running harness process wired to its driver.
struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Vec<u8>>,
    driver: Box<dyn Driver>,
    stderr: Arc<Mutex<String>>,
    events: Vec<Event>,
    /// Latest cumulative cost the harness reported for this session.
    cost_usd: f64,
    transcript: fs::File,
}

/// How to answer permission requests while pumping.
#[derive(Clone, Copy)]
enum Policy {
    Allow,
    Deny,
    /// Leave requests unanswered.
    Hold,
}

impl Session {
    fn start(
        config: &Config,
        home: &Path,
        cwd: &Path,
        mode: SessionMode,
        label: &str,
    ) -> Result<Session, String> {
        let mut driver = config.profile.driver_with(config.command.clone());
        let opened = driver
            .open(Open {
                mode,
                cwd: cwd.display().to_string(),
                model: None,
            })
            .map_err(|e| format!("open rejected: {e}"))?;
        let mut command = Command::new(&opened.launch.argv[0]);
        command
            .args(&opened.launch.argv[1..])
            .current_dir(&opened.launch.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own process group, so teardown reaches every descendant.
            .process_group(0);
        scrub_env(&mut command, home, &config.keep_env);
        let mut child = command
            .spawn()
            .map_err(|e| format!("spawn {:?}: {e}", opened.launch.argv))?;

        let (sender, lines) = mpsc::channel();
        let stdout = child.stdout.take().expect("piped");
        thread::spawn(move || {
            for line in BufReader::new(stdout).split(b'\n') {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = stderr.clone();
        let mut pipe = child.stderr.take().expect("piped");
        thread::spawn(move || {
            let mut buffer = String::new();
            let _ = pipe.read_to_string(&mut buffer);
            sink.lock().unwrap().push_str(&buffer);
        });

        let transcript = fs::File::create(config.workdir.join(format!("{label}.transcript.jsonl")))
            .map_err(|e| format!("transcript: {e}"))?;
        let mut session = Session {
            stdin: child.stdin.take(),
            child,
            lines,
            driver,
            stderr,
            events: Vec::new(),
            cost_usd: 0.0,
            transcript,
        };
        session.write(&opened.frames)?;
        Ok(session)
    }

    fn write(&mut self, frames: &[Vec<u8>]) -> Result<(), String> {
        if frames.is_empty() {
            return Ok(());
        }
        let stdin = self.stdin.as_mut().ok_or("stdin is closed")?;
        for frame in frames {
            let _ = writeln!(
                self.transcript,
                "{}",
                json!({"dir": "out", "line": String::from_utf8_lossy(frame).trim()})
            );
            stdin.write_all(frame).map_err(|e| format!("write: {e}"))?;
        }
        stdin.flush().map_err(|e| format!("flush: {e}"))
    }

    /// Feed harness output to the driver until `until` matches an event or
    /// `timeout` passes. Permission requests are answered by `policy`.
    fn pump(
        &mut self,
        timeout: Duration,
        policy: Policy,
        until: impl Fn(&Event) -> bool,
    ) -> Result<Event, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let (events, frames) = match self.lines.recv_timeout(remaining) {
                Ok(line) => {
                    let _ = writeln!(
                        self.transcript,
                        "{}",
                        json!({"dir": "in", "line": String::from_utf8_lossy(&line)})
                    );
                    let output = self.driver.receive(&line);
                    (output.events, output.frames)
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(format!("timed out after {timeout:?}"))
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let events = self.driver.transport_closed();
                    if events.iter().all(|e| !until(e)) {
                        self.events.extend(events);
                        return Err(format!("harness exited: {}", self.stderr_tail()));
                    }
                    (events, Vec::new())
                }
            };
            self.write(&frames)?;
            // Handle the whole batch before returning, so no event is lost.
            let mut matched = None;
            for event in events {
                self.events.push(event.clone());
                match &event {
                    Event::UsageObserved { usage, .. } => {
                        if let Some(cost) = usage.cost_usd {
                            self.cost_usd = self.cost_usd.max(cost);
                        }
                    }
                    Event::PermissionRequested { request, .. }
                        if !matches!(policy, Policy::Hold) =>
                    {
                        let frames = self.answer(request, policy)?;
                        self.write(&frames)?;
                    }
                    _ => {}
                }
                if matched.is_none() && until(&event) {
                    matched = Some(event);
                }
            }
            if let Some(event) = matched {
                return Ok(event);
            }
        }
    }

    fn answer(
        &mut self,
        request: &PermissionRequest,
        policy: Policy,
    ) -> Result<Vec<Vec<u8>>, String> {
        let decision = match policy {
            Policy::Allow => PermissionDecision::Allow,
            Policy::Deny | Policy::Hold => PermissionDecision::Deny {
                message: "Branchyard qualification denies this action.".into(),
            },
        };
        self.driver
            .respond(&request.key, decision)
            .map_err(|e| format!("respond: {e}"))
    }

    fn turn(
        &mut self,
        prompt: &str,
        policy: Policy,
        timeout: Duration,
    ) -> Result<(u64, TurnOutcome, String), String> {
        let submitted = self
            .driver
            .submit(prompt)
            .map_err(|e| format!("submit: {e}"))?;
        let turn = submitted.turn;
        self.write(&submitted.frames)?;
        let ended = self.pump(
            timeout,
            policy,
            |e| matches!(e, Event::TurnEnded { turn: t, .. } if *t == turn),
        )?;
        let Event::TurnEnded { outcome, .. } = ended else {
            unreachable!()
        };
        Ok((turn, outcome, self.text(turn)))
    }

    fn text(&self, turn: u64) -> String {
        self.events
            .iter()
            .filter_map(|e| match e {
                Event::MessageDelta { turn: t, text } if *t == turn => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn session_started(&self) -> Option<(NativeSession, Option<NativeSession>)> {
        self.events.iter().find_map(|e| match e {
            Event::SessionStarted {
                session,
                forked_from,
            } => Some((session.clone(), forked_from.clone())),
            _ => None,
        })
    }

    fn saw(&self, predicate: impl Fn(&Event) -> bool) -> bool {
        self.events.iter().any(predicate)
    }

    /// Close stdin and wait for the harness to exit, then tear down its
    /// process group. Returns the session cost and whether any descendant
    /// outlived the harness.
    fn close(mut self) -> Result<(f64, Option<String>), String> {
        self.stdin.take();
        let result = self.pump(Duration::from_secs(30), Policy::Deny, |e| {
            matches!(e, Event::SessionClosed)
        });
        if result.is_err() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        let survivors = self.teardown();
        result.map(|_| (self.cost_usd, survivors))
    }

    fn kill(mut self) -> Result<(Vec<Event>, f64), String> {
        self.child.kill().map_err(|e| format!("kill: {e}"))?;
        let _ = self.child.wait();
        self.teardown();
        let before = self.events.len();
        self.pump(Duration::from_secs(10), Policy::Deny, |e| {
            matches!(e, Event::SessionClosed)
        })?;
        Ok((self.events[before..].to_vec(), self.cost_usd))
    }

    /// Kill the harness's process group, naming any survivors.
    fn teardown(&self) -> Option<String> {
        let pgid = self.child.id().to_string();
        let listing = Command::new("ps")
            .args(["-o", "comm=", "-g", &pgid])
            .output()
            .ok()?;
        let survivors: Vec<String> = String::from_utf8_lossy(&listing.stdout)
            .lines()
            .map(|l| l.trim().to_owned())
            .filter(|l| !l.is_empty())
            .collect();
        if survivors.is_empty() {
            return None;
        }
        let group = format!("-{pgid}");
        let _ = Command::new("kill")
            .args(["-KILL", "--", &group])
            .stderr(Stdio::null())
            .status();
        Some(survivors.join(", "))
    }

    fn stderr_tail(&self) -> String {
        let stderr = self.stderr.lock().unwrap();
        let start = stderr.len().saturating_sub(400);
        stderr[start..].trim().to_owned()
    }
}

/// Keep proxy and system settings; drop credentials and nested-session
/// variables unless kept explicitly; give the harness a private home.
fn scrub_env(command: &mut Command, home: &Path, keep: &BTreeSet<String>) {
    for (name, _) in std::env::vars() {
        let upper = name.to_ascii_uppercase();
        let sensitive = ["ANTHROPIC", "CLAUDE", "OPENAI", "CODEX"]
            .iter()
            .any(|p| upper.starts_with(p));
        if sensitive && !keep.contains(&name) {
            command.env_remove(&name);
        }
    }
    command.env("HOME", home);
}

struct Outcome {
    name: &'static str,
    passed: bool,
    detail: String,
    seconds: f64,
}

struct Run<'a> {
    config: &'a Config,
    home: PathBuf,
    results: Vec<Outcome>,
    spent_usd: f64,
}

impl Run<'_> {
    fn record(&mut self, name: &'static str, started: Instant, result: Result<String, String>) {
        let (passed, detail) = match result {
            Ok(detail) => (true, detail),
            Err(detail) => (false, detail),
        };
        eprintln!(
            "[{}] {name}: {detail}",
            if passed { "pass" } else { "FAIL" }
        );
        self.results.push(Outcome {
            name,
            passed,
            detail,
            seconds: started.elapsed().as_secs_f64(),
        });
    }

    fn over_budget(&self) -> bool {
        self.spent_usd > self.config.max_cost_usd
    }

    fn cwd(&self) -> PathBuf {
        self.config.workdir.join("workspace")
    }

    fn start(&self, mode: SessionMode, label: &str) -> Result<Session, String> {
        let mut session = Session::start(self.config, &self.home, &self.cwd(), mode, label)?;
        session.pump(Duration::from_secs(90), Policy::Deny, |e| {
            matches!(e, Event::Ready | Event::OpenFailed { .. })
        })?;
        if let Some(Event::OpenFailed { reason }) = session.events.last() {
            return Err(format!("open failed: {reason}"));
        }
        Ok(session)
    }
}

fn check(condition: bool, failure: impl FnOnce() -> String) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(failure())
    }
}

fn main() {
    let config = match parse_args() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    let home = config.workdir.join("home");
    for dir in [home.clone(), config.workdir.join("workspace")] {
        fs::create_dir_all(&dir).expect("create qualification directories");
    }
    let mut run = Run {
        config: &config,
        home,
        results: Vec::new(),
        spent_usd: 0.0,
    };
    let started = Instant::now();
    scenarios(&mut run);
    report(&run, started.elapsed());
    if run.results.iter().any(|r| !r.passed) {
        std::process::exit(1);
    }
}

fn scenarios(run: &mut Run) {
    let capabilities = run.config.profile.driver().capabilities();
    let turn_timeout = Duration::from_secs(240);

    // One session carries the first four scenarios, then closes cleanly.
    let t = Instant::now();
    let mut main = match run.start(SessionMode::Fresh, "main") {
        Ok(session) => session,
        Err(error) => return run.record("open", t, Err(error)),
    };
    let result = (|| {
        let prompt =
            format!("Remember this code word for later: {CODE_WORD}. Reply with exactly: OK");
        let (_, outcome, text) = main.turn(&prompt, Policy::Deny, turn_timeout)?;
        check(outcome == TurnOutcome::Completed, || {
            format!("outcome {outcome:?}")
        })?;
        check(!text.trim().is_empty(), || "no message text".into())?;
        let (session, _) = main.session_started().ok_or("no SessionStarted event")?;
        check(
            !capabilities.usage || main.saw(|e| matches!(e, Event::UsageObserved { .. })),
            || "no usage reported".into(),
        )?;
        Ok(format!("session {session}; replied {:?}", text.trim()))
    })();
    run.record("fresh_turn", t, result);

    let t = Instant::now();
    let marker = run.cwd().join("deny-marker.txt");
    let result = (|| {
        let before = main.events.len();
        let prompt = "Use your shell/Bash tool to run exactly this command: echo qualified > deny-marker.txt \
                      If you cannot, reply with the word DENIED and do not try another way.";
        let (_, outcome, _) = main.turn(prompt, Policy::Deny, turn_timeout)?;
        let requests = main.events[before..]
            .iter()
            .filter(|e| matches!(e, Event::PermissionRequested { .. }))
            .count();
        check(requests > 0, || {
            "no permission request reached the driver".into()
        })?;
        check(!marker.exists(), || "the denied command ran anyway".into())?;
        check(matches!(outcome, TurnOutcome::Completed), || {
            format!("outcome {outcome:?}")
        })?;
        Ok(format!(
            "{requests} request(s) denied; marker absent; turn {outcome:?}"
        ))
    })();
    run.record("permission_denied", t, result);

    let t = Instant::now();
    let marker = run.cwd().join("allow-marker.txt");
    let result = (|| {
        let prompt = "Use your shell/Bash tool to run exactly this command: echo qualified > allow-marker.txt \
                      Then reply with the word DONE.";
        let (_, outcome, _) = main.turn(prompt, Policy::Allow, turn_timeout)?;
        check(outcome == TurnOutcome::Completed, || {
            format!("outcome {outcome:?}")
        })?;
        let content = fs::read_to_string(&marker)
            .map_err(|_| "the allowed command did not run".to_string())?;
        check(content.trim() == "qualified", || {
            format!("marker holds {content:?}")
        })?;
        Ok("request allowed; command ran".into())
    })();
    run.record("permission_allowed", t, result);

    let t = Instant::now();
    let marker = run.cwd().join("hold-marker.txt");
    let result = (|| {
        let submitted = main
            .driver
            .submit("Use your shell/Bash tool to run exactly this command: echo held > hold-marker.txt Then reply DONE.")
            .map_err(|e| format!("submit: {e}"))?;
        let turn = submitted.turn;
        main.write(&submitted.frames)?;
        main.pump(turn_timeout, Policy::Hold, |e| {
            matches!(e, Event::PermissionRequested { .. })
        })?;
        interrupt(
            &mut main,
            turn,
            Policy::Hold,
            capabilities.turn_acknowledgment,
        )
        .and_then(|detail| {
            check(!marker.exists(), || "the unanswered command ran".into())?;
            let withdrawn = main.saw(|e| matches!(e, Event::PermissionWithdrawn { .. }));
            Ok(format!(
                "{detail}; pending request withdrawn by harness: {withdrawn}"
            ))
        })
    })();
    run.record("interrupt_during_permission", t, result);

    let t = Instant::now();
    let result = (|| {
        let submitted = main
            .driver
            .submit(
                "Use your shell/Bash tool to run exactly this command in the foreground, not in the background: \
                 python3 -c \"import time; time.sleep(45)\" Then reply DONE.",
            )
            .map_err(|e| format!("submit: {e}"))?;
        let turn = submitted.turn;
        main.write(&submitted.frames)?;
        main.pump(turn_timeout, Policy::Allow, |e| {
            matches!(e, Event::PermissionRequested { .. })
        })?;
        // Let the command start before interrupting it.
        if let Ok(ended) = main.pump(
            Duration::from_secs(4),
            Policy::Allow,
            |e| matches!(e, Event::TurnEnded { turn: t, .. } if *t == turn),
        ) {
            return Err(format!("the turn ended before the interrupt: {ended:?}"));
        }
        interrupt(
            &mut main,
            turn,
            Policy::Allow,
            capabilities.turn_acknowledgment,
        )
    })();
    run.record("interrupt_during_tool", t, result);

    let t = Instant::now();
    let parent = main.session_started().map(|(s, _)| s);
    let mut parent_cost = 0.0;
    let result = main.close().map(|(cost, survivors)| {
        run.spent_usd += cost;
        parent_cost = cost;
        match survivors {
            None => format!("closed cleanly; no descendants outlived the harness; session cost ${cost:.4}"),
            Some(names) => format!("closed; descendants outlived the harness and were killed: {names}; session cost ${cost:.4}"),
        }
    });
    run.record("clean_close", t, result);
    let Some(parent) = parent else { return };

    for (name, fork) in [("resume", false), ("fork", true)] {
        let t = Instant::now();
        if run.over_budget() {
            run.record(
                name,
                t,
                Err(format!("skipped: spent ${:.2} over the cap", run.spent_usd)),
            );
            continue;
        }
        if fork && !capabilities.fork {
            let mut driver = run.config.profile.driver();
            let rejected = driver.open(Open {
                mode: SessionMode::Fork(parent.clone()),
                cwd: run.cwd().display().to_string(),
                model: None,
            });
            let result = match rejected {
                Err(error) => Ok(format!("rejected before launch as declared: {error}")),
                Ok(_) => Err("the profile declares no fork but opened one".into()),
            };
            run.record(name, t, result);
            continue;
        }
        let mode = if fork {
            SessionMode::Fork(parent.clone())
        } else {
            SessionMode::Resume(parent.clone())
        };
        let result = (|| {
            let mut session = run.start(mode, name)?;
            let (_, outcome, text) = session.turn(
                "What code word did I ask you to remember? Reply with only the code word.",
                Policy::Deny,
                turn_timeout,
            )?;
            let started = session.session_started();
            let (cost, _) = session.close()?;
            // A resumed or forked session reports totals that include its
            // parent's cost; count only what this session added.
            run.spent_usd += (cost - parent_cost).max(0.0);
            check(outcome == TurnOutcome::Completed, || {
                format!("outcome {outcome:?}")
            })?;
            check(text.contains(CODE_WORD), || {
                format!("context lost: replied {:?}", text.trim())
            })?;
            let (session, forked_from) = started.ok_or("no SessionStarted event")?;
            if fork {
                check(
                    session != parent && forked_from.as_ref() == Some(&parent),
                    || format!("fork identity {session} from {forked_from:?}"),
                )?;
            } else {
                check(session == parent, || format!("resumed as {session}"))?;
            }
            Ok(format!(
                "session {session}; remembered {CODE_WORD}; cost ${cost:.4}"
            ))
        })();
        run.record(name, t, result);
    }

    let t = Instant::now();
    if run.over_budget() {
        return run.record(
            "connection_lost",
            t,
            Err(format!("skipped: spent ${:.2} over the cap", run.spent_usd)),
        );
    }
    let result = (|| {
        let mut session = run.start(SessionMode::Fresh, "connection-lost")?;
        let submitted = session
            .driver
            .submit(
                "Use your shell/Bash tool to run exactly this command: sleep 60 Then reply DONE.",
            )
            .map_err(|e| format!("submit: {e}"))?;
        let turn = submitted.turn;
        session.write(&submitted.frames)?;
        session.pump(turn_timeout, Policy::Allow, |e| {
            matches!(
                e,
                Event::ToolStarted { .. } | Event::PermissionRequested { .. }
            )
        })?;
        let (events, cost) = session.kill()?;
        run.spent_usd += cost;
        check(
            events
                .iter()
                .any(|e| matches!(e, Event::OutcomeUnknown { turn: t, .. } if *t == turn)),
            || format!("events after kill: {events:?}"),
        )?;
        Ok("killed mid-turn; outcome reported unknown".into())
    })();
    run.record("connection_lost", t, result);
}

/// Interrupt `turn` and require it to end as interrupted, promptly.
fn interrupt(
    session: &mut Session,
    turn: u64,
    policy: Policy,
    acknowledgment: bool,
) -> Result<String, String> {
    let interrupted_at = Instant::now();
    let frames = session
        .driver
        .interrupt()
        .map_err(|e| format!("interrupt: {e}"))?;
    session.write(&frames)?;
    let ended = session.pump(
        Duration::from_secs(45),
        policy,
        |e| matches!(e, Event::TurnEnded { turn: t, .. } if *t == turn),
    )?;
    let Event::TurnEnded { outcome, .. } = ended else {
        unreachable!()
    };
    let waited = interrupted_at.elapsed().as_secs_f64();
    check(outcome == TurnOutcome::Interrupted, || {
        format!("outcome {outcome:?}")
    })?;
    check(waited < 30.0, || format!("took {waited:.1}s to stop"))?;
    let acknowledged =
        session.saw(|e| matches!(e, Event::InterruptAcknowledged { turn: t } if *t == turn));
    check(!acknowledgment || acknowledged, || {
        "no interrupt acknowledgment".into()
    })?;
    Ok(format!(
        "stopped {waited:.1}s after interrupt; acknowledged: {acknowledged}"
    ))
}

fn version(config: &Config, home: &Path) -> String {
    let mut command = Command::new(&config.command[0]);
    command.arg("--version").stdin(Stdio::null());
    scrub_env(&mut command, home, &config.keep_env);
    command
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn host() -> String {
    Command::new("uname")
        .arg("-srm")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

fn report(run: &Run, elapsed: Duration) {
    let report = json!({
        "profile": run.config.profile.id,
        "harness": run.config.profile.harness,
        "command": run.config.command.join(" "),
        "harness_version": version(run.config, &run.home),
        "driver_version": env!("CARGO_PKG_VERSION"),
        "host": host(),
        "execution": "local process with scrubbed environment and private home; not a Branchyard sandbox",
        "seconds": elapsed.as_secs_f64().round(),
        "harness_estimated_cost_usd": if run.config.profile.driver().capabilities().usage {
            json!((run.spent_usd * 10000.0).round() / 10000.0)
        } else {
            json!("not reported by this protocol; the cost cap did not apply")
        },
        "scenarios": run.results.iter().map(|r| json!({
            "name": r.name,
            "passed": r.passed,
            "detail": r.detail,
            "seconds": (r.seconds * 10.0).round() / 10.0,
        })).collect::<Vec<Value>>(),
    });
    let text = serde_json::to_string_pretty(&report).unwrap();
    println!("{text}");
    if let Some(path) = &run.config.report {
        fs::write(path, text + "\n").expect("write report");
    }
}
