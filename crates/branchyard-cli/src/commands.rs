//! One function per command, each a thin call into the SDK.

use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use branchyard::{Branch, BranchInfo, BranchStatus, Budget, Policy, TaskOptions, Yard};

use crate::args::{self, shell_quote, TaskArgs};
use crate::console::{self, Choice, Console};
use crate::json;
use crate::render::{self, Renderer, Style, Tone};

/// What the process can see of its terminal.
pub struct Env {
    pub stdin_tty: bool,
    pub stdout_tty: bool,
    pub stderr_tty: bool,
    /// Color only on a terminal, and never when `NO_COLOR` is set.
    pub color: bool,
}

impl Env {
    pub fn detect() -> Env {
        let stdout_tty = io::stdout().is_terminal();
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        Env {
            stdin_tty: io::stdin().is_terminal(),
            stdout_tty,
            stderr_tty: io::stderr().is_terminal(),
            color: stdout_tty && !no_color,
        }
    }

    fn style(&self) -> Style {
        Style { color: self.color }
    }
}

#[derive(Debug)]
pub enum Failure {
    Sdk(branchyard::Error),
    Io(io::Error),
    Message(String),
    /// Already explained on stdout, such as a branch that failed; exit 1.
    Reported,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Sdk(error) => write!(f, "{error}"),
            Failure::Io(error) => write!(f, "{error}"),
            Failure::Message(message) => f.write_str(message),
            Failure::Reported => Ok(()),
        }
    }
}

impl From<branchyard::Error> for Failure {
    fn from(error: branchyard::Error) -> Self {
        Failure::Sdk(error)
    }
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Failure::Io(error)
    }
}

pub type Outcome = Result<(), Failure>;

pub fn print(text: &str) -> Outcome {
    let mut stdout = io::stdout().lock();
    stdout.write_all(text.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

fn open() -> Result<Yard, Failure> {
    Ok(Yard::open(".")?)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Console and policy for commands that run a harness.
struct Live {
    console: Arc<Console>,
    policy: Policy,
}

impl Live {
    fn start(env: &Env, task: &TaskArgs, prefixed: bool) -> Live {
        let console = Arc::new(Console::new(
            Renderer::new(env.style(), prefixed),
            Box::new(io::stdout()),
            Box::new(console::terminal_prompt),
        ));
        let choice = console::choose(task.permissions, env.stdin_tty, env.stderr_tty);
        eprintln!("by: local mode: harnesses run as your user, with no isolation beyond it");
        if choice == (Choice::DenyAll { notice: true }) {
            eprintln!("by: {}", console::DENY_NOTICE);
        }
        let policy = console::policy(choice, console.clone());
        Live { console, policy }
    }

    fn options(&self, task: &TaskArgs) -> TaskOptions {
        let console = self.console.clone();
        TaskOptions {
            harness: task.harness.clone(),
            name: task.name.clone(),
            base: task.base.clone(),
            budget: Budget {
                max_usd: task.budget_usd,
                max_turns: task.max_turns,
                max_duration: task.max_duration,
            },
            policy: self.policy.clone(),
            check: task.check.clone(),
            observer: Some(Arc::new(move |event| console.event(event))),
            isolated: task.isolated,
            command: task.command.clone(),
        }
    }

    /// Print the closing summary for one branch.
    fn finish(self, env: &Env, result: Result<Branch, branchyard::Error>) -> Outcome {
        self.console.finish();
        let branch = result?;
        print(&format!(
            "\n{}",
            render::summary(branch.info(), env.style())
        ))?;
        branch_outcome(branch.info())
    }
}

/// A branch that failed is an error for scripts; one that stopped at a
/// limit or produced nothing is not.
fn branch_outcome(info: &BranchInfo) -> Outcome {
    match info.status {
        BranchStatus::Failed { .. } => Err(Failure::Reported),
        _ => Ok(()),
    }
}

pub fn run(env: &Env, prompt: &str, task: &TaskArgs) -> Outcome {
    let yard = open()?;
    let live = Live::start(env, task, false);
    let result = yard.task(prompt).options(live.options(task)).run();
    live.finish(env, result)
}

pub fn fan(env: &Env, prompt: &str, harnesses: &[String], task: &TaskArgs) -> Outcome {
    let yard = open()?;
    let live = Live::start(env, task, true);
    let ids: Vec<&str> = harnesses.iter().map(String::as_str).collect();
    let builder = yard.task(prompt).options(live.options(task));
    // Knowing the names up front lines the prefixes up from the first line.
    if let Ok(names) = builder.planned_names(&ids) {
        live.console.reserve(&names);
    }
    let result = builder.run_on(&ids);
    live.console.finish();
    let branches = result?;
    let infos: Vec<&BranchInfo> = branches.iter().map(Branch::info).collect();
    let style = env.style();
    let mut text = format!("\n{}", render::comparison_table(&infos, style));
    let ready: Vec<&str> = infos
        .iter()
        .filter(|info| info.status == BranchStatus::Ready)
        .map(|info| info.name.as_str())
        .collect();
    if !ready.is_empty() {
        text.push_str(&format!("\n{}\n", style.paint(Tone::Dim, "next")));
        for name in &ready {
            text.push_str(&format!("  by diff {}\n", shell_quote(name)));
        }
        text.push_str("  by merge <branch>\n");
    }
    print(&text)?;
    if infos
        .iter()
        .all(|info| matches!(info.status, BranchStatus::Failed { .. }))
    {
        return Err(Failure::Reported);
    }
    Ok(())
}

pub fn send(env: &Env, branch: &str, prompt: &str, task: &TaskArgs) -> Outcome {
    let branch = open()?.branch(branch)?;
    let live = Live::start(env, task, false);
    let result = branch.send(prompt, live.options(task));
    live.finish(env, result)
}

pub fn fork(
    env: &Env,
    branch: &str,
    prompt: &str,
    fresh_session: bool,
    task: &TaskArgs,
) -> Outcome {
    let branch = open()?.branch(branch)?;
    let live = Live::start(env, task, false);
    let result = branch.fork(prompt, fresh_session, live.options(task));
    live.finish(env, result)
}

pub fn ls(env: &Env, as_json: bool) -> Outcome {
    let infos = open()?.branches()?;
    if as_json {
        let list = infos.iter().map(json::branch).collect();
        return print(&json::text(&serde_json::Value::Array(list)));
    }
    if infos.is_empty() {
        return print("no branches; start one with: by run \"<prompt>\"\n");
    }
    print(&render::branch_table(&infos, now(), env.style()))
}

pub fn show(env: &Env, branch: &str, as_json: bool) -> Outcome {
    let branch = open()?.branch(branch)?;
    if as_json {
        return print(&json::text(&json::branch(branch.info())));
    }
    print(&render::details(branch.info(), now(), env.style()))
}

pub fn diff(env: &Env, branch: &str) -> Outcome {
    let diff = open()?.branch(branch)?.diff()?;
    if !env.stdout_tty || diff.is_empty() {
        return print(&diff);
    }
    let text = if env.color { color_diff(&diff) } else { diff };
    page(&text)
}

/// Color a unified diff the way `git diff` does.
fn color_diff(diff: &str) -> String {
    let style = Style { color: true };
    diff.lines()
        .map(|line| {
            let tone = if line.starts_with("+++") || line.starts_with("---") {
                Some(Tone::Bold)
            } else if line.starts_with('+') {
                Some(Tone::Green)
            } else if line.starts_with('-') {
                Some(Tone::Red)
            } else if line.starts_with("@@") {
                Some(Tone::Cyan)
            } else if line.starts_with("diff ") {
                Some(Tone::Bold)
            } else {
                None
            };
            let line = match tone {
                Some(tone) => style.paint(tone, line),
                None => line.to_owned(),
            };
            line + "\n"
        })
        .collect()
}

/// Show `text` through `$PAGER` (default `less`), or print it if the pager
/// cannot start. Quitting the pager early is not an error.
fn page(text: &str) -> Outcome {
    let pager = std::env::var("PAGER")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| "less".into());
    let argv = args::split_words(&pager).map_err(|e| Failure::Message(format!("PAGER: {e}")))?;
    let Some((program, rest)) = argv.split_first() else {
        return print(text);
    };
    let mut command = Command::new(program);
    command.args(rest).stdin(Stdio::piped());
    if std::env::var_os("LESS").is_none() {
        // Quit if one screen, pass color through, keep the screen.
        command.env("LESS", "FRX");
    }
    let Ok(mut child) = command.spawn() else {
        return print(text);
    };
    if let Some(mut stdin) = child.stdin.take() {
        match stdin.write_all(text.as_bytes()) {
            Err(error) if error.kind() != io::ErrorKind::BrokenPipe => return Err(error.into()),
            _ => {}
        }
    }
    child.wait()?;
    Ok(())
}

pub fn log(env: &Env, branch: &str, as_json: bool) -> Outcome {
    let events = open()?.branch(branch)?.events()?;
    if as_json {
        let list = events.iter().map(json::recorded).collect();
        return print(&json::text(&serde_json::Value::Array(list)));
    }
    print(&render::log_text(&events, env.style()))
}

pub fn merge(branch: &str, into: Option<&str>) -> Outcome {
    let yard = open()?;
    let target = match into {
        Some(target) => target.to_owned(),
        None => current_branch(yard.root())?,
    };
    let merged = yard.merge(branch, &target)?;
    let short = |commit: &str| commit.get(..10).unwrap_or(commit).to_owned();
    print(&format!(
        "merged {} into {} ({}..{})\n",
        merged.branch,
        merged.target,
        short(&merged.previous),
        short(&merged.commit)
    ))
}

/// The repository's checked-out branch, the default merge target.
fn current_branch(root: &Path) -> Result<String, Failure> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(root)
        .output()
        .map_err(|e| Failure::Message(format!("could not run git: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Failure::Message(format!(
            "could not resolve the current branch: {}",
            stderr.trim()
        )));
    }
    let name = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if name == "HEAD" {
        return Err(Failure::Message(
            "HEAD is detached; pass --into <branch>".into(),
        ));
    }
    Ok(name)
}

pub fn rm(branch: &str) -> Outcome {
    open()?.remove(branch)?;
    print(&format!("removed {branch}\n"))
}

pub fn harnesses(env: &Env, as_json: bool) -> Outcome {
    let harnesses = open()?.harnesses();
    if as_json {
        let list = harnesses.iter().map(json::harness).collect();
        return print(&json::text(&serde_json::Value::Array(list)));
    }
    print(&render::harness_table(&harnesses, env.style()))
}
