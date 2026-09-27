//! Run driver qualification scenarios against a real harness binary.
//!
//! This is a development tool, not the Branchyard node. It starts the
//! harness as a local process with a scrubbed environment and a private
//! home through `branchyard-runtime`, standing in for `SandboxProvider.exec`,
//! and drives it through the profile's driver. It qualifies protocol behavior
//! against the real binary:
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
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use branchyard_harness::profiles::{self, Profile};
use branchyard_harness::{
    Event, NativeSession, Open, PermissionDecision, SessionMode, TurnOutcome,
};
use branchyard_runtime::{Environment, RuntimeError, Session};
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

/// How to answer permission requests while pumping.
#[derive(Clone, Copy)]
enum Policy {
    Allow,
    Deny,
    /// Leave requests unanswered.
    Hold,
}

impl Policy {
    fn answer(self) -> PermissionDecision {
        match self {
            Policy::Allow => PermissionDecision::Allow,
            Policy::Deny | Policy::Hold => PermissionDecision::Deny {
                message: "Branchyard qualification denies this action.".into(),
            },
        }
    }
}

/// A runtime error as a scenario detail, naming the refused operation.
fn why(operation: &str, error: RuntimeError) -> String {
    match error {
        RuntimeError::Rejected(rejected) => format!("{operation}: {rejected}"),
        other => other.to_string(),
    }
}

/// Feed harness output to the driver until `until` matches an event or
/// `timeout` passes. Permission requests are answered by `policy`.
fn pump(
    session: &mut Session,
    timeout: Duration,
    policy: Policy,
    until: impl Fn(&Event) -> bool,
) -> Result<Event, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = session
            .next_event(remaining)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("timed out after {timeout:?}"))?;
        if let Event::PermissionRequested { request, .. } = &event {
            if !matches!(policy, Policy::Hold) {
                session
                    .respond(&request.key, policy.answer())
                    .map_err(|e| why("respond", e))?;
            }
        }
        if until(&event) {
            return Ok(event);
        }
    }
}

fn turn(
    session: &mut Session,
    prompt: &str,
    policy: Policy,
    timeout: Duration,
) -> Result<(u64, TurnOutcome, String), String> {
    let report = session
        .run_turn(prompt, &mut |_| policy.answer(), timeout)
        .map_err(|e| why("submit", e))?;
    Ok((report.turn, report.outcome, report.text))
}

fn submit(session: &mut Session, prompt: &str) -> Result<u64, String> {
    session.submit(prompt).map_err(|e| why("submit", e))
}

fn session_started(session: &Session) -> Option<(NativeSession, Option<NativeSession>)> {
    session.events().iter().find_map(|e| match e {
        Event::SessionStarted {
            session,
            forked_from,
        } => Some((session.clone(), forked_from.clone())),
        _ => None,
    })
}

fn saw(session: &Session, predicate: impl Fn(&Event) -> bool) -> bool {
    session.events().iter().any(predicate)
}

/// Close stdin and wait for the harness to exit, then tear down its process
/// group. Returns the session cost and the descendants that outlived the
/// harness, if any.
fn close(session: Session) -> Result<(Option<f64>, Option<String>), String> {
    let grace = Duration::from_secs(30);
    let closed = session.close(grace).map_err(|e| e.to_string())?;
    if closed.forced {
        return Err(format!("timed out after {grace:?}"));
    }
    let survivors = (!closed.survivors.is_empty()).then(|| closed.survivors.join(", "));
    Ok((closed.cost_usd, survivors))
}

/// Kill the harness mid-session. Returns the events the driver produced as
/// the connection closed and the session cost.
fn kill(session: Session) -> Result<(Vec<Event>, Option<f64>), String> {
    let before = session.cost_usd();
    let events = session.kill().map_err(|e| e.to_string())?;
    let cost = events
        .iter()
        .filter_map(|e| match e {
            Event::UsageObserved { usage, .. } if usage.cumulative => usage.cost_usd,
            _ => None,
        })
        .fold(before, |acc, cost| Some(acc.map_or(cost, |a| a.max(cost))));
    Ok((events, cost))
}

struct Outcome {
    name: &'static str,
    passed: bool,
    detail: String,
    seconds: f64,
}

struct Run<'a> {
    config: &'a Config,
    env: Environment,
    results: Vec<Outcome>,
    spent_usd: f64,
    /// Whether any harness reported a cost. Without one, spend is unknown
    /// and the cap cannot apply.
    cost_observed: bool,
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

    /// Whether spend so far, including the live session's running total,
    /// exceeds the cap. Checked before every scenario that calls a model.
    fn over_budget_with(&self, live: &Session) -> bool {
        self.spent_usd + live.cost_usd().unwrap_or(0.0) > self.config.max_cost_usd
    }

    /// Add a session's cost increment over `baseline`, noting whether any
    /// cost was reported at all.
    fn add_cost(&mut self, cost: Option<f64>, baseline: f64) {
        if let Some(cost) = cost {
            self.cost_observed = true;
            self.spent_usd += (cost - baseline).max(0.0);
        }
    }

    fn skip_over_budget(&mut self, name: &'static str, started: Instant) {
        let detail = format!("skipped: spent ${:.2} over the cap", self.spent_usd);
        self.record(name, started, Err(detail));
    }

    fn cwd(&self) -> PathBuf {
        self.config.workdir.join("workspace")
    }

    fn start(&self, mode: SessionMode, label: &str) -> Result<Session, String> {
        let driver = self.config.profile.driver_with(self.config.command.clone());
        let open = Open {
            mode,
            cwd: self.cwd().display().to_string(),
            model: None,
            mcp_servers: Vec::new(),
            instructions: None,
        };
        let transcript = self
            .config
            .workdir
            .join(format!("{label}.transcript.jsonl"));
        let mut session = Session::start(driver, open, &self.env, Some(&transcript))
            .map_err(|e| why("open rejected", e))?;
        let opened = pump(&mut session, Duration::from_secs(90), Policy::Deny, |e| {
            matches!(e, Event::Ready | Event::OpenFailed { .. })
        })?;
        if let Event::OpenFailed { reason } = opened {
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
    // Keep proxy and system settings; drop credentials and nested-session
    // variables unless kept explicitly; give the harness a private home.
    let env = config
        .keep_env
        .iter()
        .fold(Environment::new(home), |env, name| env.keep(name.as_str()));
    let mut run = Run {
        config: &config,
        env,
        results: Vec::new(),
        spent_usd: 0.0,
        cost_observed: false,
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
        let (_, outcome, text) = turn(&mut main, &prompt, Policy::Deny, turn_timeout)?;
        check(outcome == TurnOutcome::Completed, || {
            format!("outcome {outcome:?}")
        })?;
        check(!text.trim().is_empty(), || "no message text".into())?;
        let (session, _) = session_started(&main).ok_or("no SessionStarted event")?;
        check(
            !capabilities.usage || saw(&main, |e| matches!(e, Event::UsageObserved { .. })),
            || "no usage reported".into(),
        )?;
        Ok(format!("session {session}; replied {:?}", text.trim()))
    })();
    run.record("fresh_turn", t, result);

    let t = Instant::now();
    if run.over_budget_with(&main) {
        run.skip_over_budget("permission_denied", t);
    } else {
        let marker = run.cwd().join("deny-marker.txt");
        let result = (|| {
            let before = main.events().len();
            let prompt = "Use your shell/Bash tool to run exactly this command: echo qualified > deny-marker.txt \
                      If you cannot, reply with the word DENIED and do not try another way.";
            let (_, outcome, _) = turn(&mut main, prompt, Policy::Deny, turn_timeout)?;
            let requests = main.events()[before..]
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
    }

    let t = Instant::now();
    if run.over_budget_with(&main) {
        run.skip_over_budget("permission_allowed", t);
    } else {
        let marker = run.cwd().join("allow-marker.txt");
        let result = (|| {
            let prompt = "Use your shell/Bash tool to run exactly this command: echo qualified > allow-marker.txt \
                      Then reply with the word DONE.";
            let (_, outcome, _) = turn(&mut main, prompt, Policy::Allow, turn_timeout)?;
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
    }

    let t = Instant::now();
    if run.over_budget_with(&main) {
        run.skip_over_budget("interrupt_during_permission", t);
    } else {
        let marker = run.cwd().join("hold-marker.txt");
        let result = (|| {
            let turn = submit(
            &mut main,
            "Use your shell/Bash tool to run exactly this command: echo held > hold-marker.txt Then reply DONE.",
        )?;
            pump(&mut main, turn_timeout, Policy::Hold, |e| {
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
                let withdrawn = saw(&main, |e| matches!(e, Event::PermissionWithdrawn { .. }));
                Ok(format!(
                    "{detail}; pending request withdrawn by harness: {withdrawn}"
                ))
            })
        })();
        run.record("interrupt_during_permission", t, result);
    }

    let t = Instant::now();
    if run.over_budget_with(&main) {
        run.skip_over_budget("interrupt_during_tool", t);
    } else {
        let result = (|| {
            let turn = submit(
            &mut main,
            "Use your shell/Bash tool to run exactly this command in the foreground, not in the background: \
             python3 -c \"import time; time.sleep(45)\" Then reply DONE.",
        )?;
            pump(&mut main, turn_timeout, Policy::Allow, |e| {
                matches!(e, Event::PermissionRequested { .. })
            })?;
            // Let the command start before interrupting it.
            if let Ok(ended) = pump(
                &mut main,
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
    }

    let t = Instant::now();
    let parent = session_started(&main).map(|(s, _)| s);
    let mut parent_cost = 0.0;
    let result = close(main).map(|(cost, survivors)| {
        run.add_cost(cost, 0.0);
        let cost = cost.unwrap_or(0.0);
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
                mcp_servers: Vec::new(),
                instructions: None,
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
            let (_, outcome, text) = turn(
                &mut session,
                "What code word did I ask you to remember? Reply with only the code word.",
                Policy::Deny,
                turn_timeout,
            )?;
            let started = session_started(&session);
            let (cost, _) = close(session)?;
            // A resumed or forked session reports totals that include its
            // parent's cost; count only what this session added.
            run.add_cost(cost, parent_cost);
            let cost = cost.unwrap_or(0.0);
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
        let turn = submit(
            &mut session,
            "Use your shell/Bash tool to run exactly this command: sleep 60 Then reply DONE.",
        )?;
        pump(&mut session, turn_timeout, Policy::Allow, |e| {
            matches!(
                e,
                Event::ToolStarted { .. } | Event::PermissionRequested { .. }
            )
        })?;
        let (events, cost) = kill(session)?;
        run.add_cost(cost, 0.0);
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
    session.interrupt().map_err(|e| why("interrupt", e))?;
    let ended = pump(
        session,
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
    let acknowledged = saw(
        session,
        |e| matches!(e, Event::InterruptAcknowledged { turn: t } if *t == turn),
    );
    check(!acknowledgment || acknowledged, || {
        "no interrupt acknowledgment".into()
    })?;
    Ok(format!(
        "stopped {waited:.1}s after interrupt; acknowledged: {acknowledged}"
    ))
}

fn version(config: &Config, env: &Environment) -> String {
    let mut command = Command::new(&config.command[0]);
    command.arg("--version").stdin(Stdio::null());
    env.apply(&mut command);
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
        "harness_version": version(run.config, &run.env),
        "driver_version": env!("CARGO_PKG_VERSION"),
        "host": host(),
        "execution": "local process with scrubbed environment and private home; not a Branchyard sandbox",
        "seconds": elapsed.as_secs_f64().round(),
        "harness_estimated_cost_usd": if run.cost_observed {
            json!((run.spent_usd * 10000.0).round() / 10000.0)
        } else {
            json!("not reported by the harness; the cost cap did not apply")
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
