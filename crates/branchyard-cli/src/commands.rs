//! One function per command, each a thin call into the SDK locally or into
//! `branchyard-client` remotely ([`crate::remote`]).

use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use branchyard::{
    Branch, BranchInfo, BranchStatus, Budget, Delegate, Envelope, Policy, Provider, SandboxOptions,
    Spawn, SubstrateOptions, TaskOptions, Yard, ENV_BRANCH, ENV_TOKEN,
};
use serde::Serialize;

use crate::args::{self, shell_quote, SpawnArgs, TaskArgs};
use crate::console::{self, Choice, Console};
use crate::json;
use crate::remote::{self, Remote};
use crate::render::{self, Renderer, Style, Tone};

/// Where commands run.
pub enum Target {
    /// In-process, on the repository containing the current directory.
    Local,
    Remote(Box<Remote>),
}

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
    Remote(branchyard_client::Error),
    Io(io::Error),
    Message(String),
    /// Already explained on stdout, such as a branch that failed; exit 1.
    Reported,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Sdk(error) => write!(f, "{error}"),
            Failure::Remote(error) => write!(f, "{error}"),
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

impl From<branchyard_client::Error> for Failure {
    fn from(error: branchyard_client::Error) -> Self {
        Failure::Remote(error)
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

/// The yard: the harness's repository when `by` runs inside a harness,
/// whose working directory is its branch's worktree, else the current one.
pub fn open() -> Result<Yard, Failure> {
    Ok(open_yard()?)
}

fn open_yard() -> Result<Yard, branchyard::Error> {
    match std::env::var_os(branchyard::ENV_ROOT).filter(|v| !v.is_empty()) {
        Some(root) => Yard::open(root),
        None => Yard::open("."),
    }
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
    /// `branch` is the provider a send or fork inherits when the flags name
    /// none.
    fn start(env: &Env, task: &TaskArgs, prefixed: bool, branch: Option<Provider>) -> Live {
        Live::start_to(env, task, prefixed, false, branch)
    }

    /// With `json`, activity goes to stderr so stdout holds only the result.
    fn start_to(
        env: &Env,
        task: &TaskArgs,
        prefixed: bool,
        json: bool,
        branch: Option<Provider>,
    ) -> Live {
        let out: Box<dyn Write + Send> = match json {
            true => Box::new(io::stderr()),
            false => Box::new(io::stdout()),
        };
        let console = Arc::new(Console::new(
            Renderer::new(env.style(), prefixed),
            out,
            Box::new(console::terminal_prompt),
        ));
        let choice = console::choose(task.permissions, env.stdin_tty, env.stderr_tty);
        match provider(task).or(branch) {
            None | Some(Provider::Local) => {
                eprintln!("by: local mode: harnesses run as your user, with no isolation beyond it")
            }
            Some(Provider::Microsandbox(sandbox)) => eprintln!(
                "by: harnesses run in Microsandbox microVMs from {}; the worktree is mounted at {}",
                sandbox.image,
                branchyard::SANDBOX_WORKSPACE
            ),
            Some(Provider::Substrate(options)) => eprintln!(
                "by: harnesses run in Agent Substrate actors from template {} (unqualified); \
                 the worktree is copied to {} and back",
                options.template,
                options.workdir()
            ),
        }
        if choice == (Choice::DenyAll { notice: true }) {
            eprintln!("by: {}", console::DENY_NOTICE);
        }
        let policy = console::policy(choice, console.clone());
        Live { console, policy }
    }

    fn options(&self, task: &TaskArgs) -> TaskOptions {
        let console = self.console.clone();
        let exe = std::env::current_exe().ok();
        let policy = match (&exe, task.allow_delegation) {
            (Some(by), true) => self.policy.clone().allow_delegation_commands(by),
            _ => self.policy.clone(),
        };
        TaskOptions {
            harness: task.harness.clone(),
            name: task.name.clone(),
            base: task.base.clone(),
            budget: Budget {
                max_usd: task.budget_usd,
                max_turns: task.max_turns,
                max_duration: task.max_duration,
            },
            policy,
            check: task.check.clone(),
            observer: Some(Arc::new(move |event| console.event(event))),
            isolated: task.isolated,
            command: task.command.clone(),
            provider: provider(task),
            delegation: task.delegate.map(Envelope::depth),
            delegation_cli: exe,
            delegation_server: None,
            unapproved_tools: task.unapproved_tools,
        }
    }

    /// Print the closing summary for one branch, once every branch it
    /// delegated to on this process has finished.
    fn finish(self, env: &Env, result: Result<Branch, branchyard::Error>) -> Outcome {
        let branch = match result {
            Ok(branch) => branch,
            Err(error) => {
                self.console.finish();
                return Err(error.into());
            }
        };
        let descendants = wait_for_descendants(&[&branch]);
        self.console.finish();
        print(&format!(
            "\n{}",
            render::summary(branch.info(), env.style())
        ))?;
        if let Some(descendants) = descendants? {
            let infos: Vec<&BranchInfo> = descendants.iter().collect();
            print(&format!(
                "\ndelegated\n{}",
                render::comparison_table(&infos, env.style())
            ))?;
        }
        branch_outcome(branch.info())
    }
}

/// Wait for every branch these delegated to, in this process or another,
/// saying which, and return them; `None` if there were none.
fn wait_for_descendants(branches: &[&Branch]) -> Result<Option<Vec<BranchInfo>>, Failure> {
    let mut all = Vec::new();
    for branch in branches {
        let running: Vec<String> = branch
            .descendants()?
            .into_iter()
            .filter(|info| info.status == BranchStatus::Running)
            .map(|info| info.name)
            .collect();
        if !running.is_empty() {
            eprintln!(
                "by: waiting for {} delegated branch{} still running: {}",
                running.len(),
                if running.len() == 1 { "" } else { "es" },
                running.join(", ")
            );
        }
        all.extend(branch.wait_subtree()?);
    }
    Ok((!all.is_empty()).then_some(all))
}

/// A branch that failed is an error for scripts; one that stopped at a
/// limit or produced nothing is not.
pub fn branch_outcome(info: &BranchInfo) -> Outcome {
    match info.status {
        BranchStatus::Failed { .. } => Err(Failure::Reported),
        _ => Ok(()),
    }
}

pub fn run(env: &Env, target: &Target, prompt: &str, task: &TaskArgs) -> Outcome {
    if let Target::Remote(remote) = target {
        return remote::run(env, remote, prompt, task);
    }
    let yard = open()?;
    let live = Live::start(env, task, task.delegate.is_some(), None);
    let result = yard.task(prompt).options(live.options(task)).run();
    live.finish(env, result)
}

pub fn fan(
    env: &Env,
    target: &Target,
    prompt: &str,
    harnesses: &[String],
    task: &TaskArgs,
) -> Outcome {
    if let Target::Remote(remote) = target {
        return remote::fan(env, remote, prompt, harnesses, task);
    }
    let yard = open()?;
    let live = Live::start(env, task, true, None);
    let ids: Vec<&str> = harnesses.iter().map(String::as_str).collect();
    let builder = yard.task(prompt).options(live.options(task));
    // Knowing the names up front lines the prefixes up from the first line.
    if let Ok(names) = builder.planned_names(&ids) {
        live.console.reserve(&names);
    }
    let result = builder.run_on(&ids);
    let branches = match result {
        Ok(branches) => branches,
        Err(error) => {
            live.console.finish();
            return Err(error.into());
        }
    };
    let descendants = wait_for_descendants(&branches.iter().collect::<Vec<_>>());
    live.console.finish();
    let descendants = descendants?.unwrap_or_default();
    let infos: Vec<&BranchInfo> = branches
        .iter()
        .map(Branch::info)
        .chain(descendants.iter())
        .collect();
    fan_summary(env, &infos)
}

/// The comparison closing `by fan`, and its exit status: failure only when
/// every branch failed.
pub fn fan_summary(env: &Env, infos: &[&BranchInfo]) -> Outcome {
    let style = env.style();
    let mut text = format!("\n{}", render::comparison_table(infos, style));
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

pub fn send(
    env: &Env,
    target: &Target,
    branch: &str,
    prompt: &str,
    task: &TaskArgs,
    json: bool,
) -> Outcome {
    if let Some(delegate) = harness_delegate(json)? {
        if *task != TaskArgs::default() {
            return fail(
                json,
                &branchyard::Error::Denied(
                    "inside a harness, send takes only --json; the child keeps its own limits"
                        .into(),
                ),
            );
        }
        return emit(json, delegate.send(branch, prompt), |sent| {
            format!("sent to {}; its turn is running\n", sent.name)
        });
    }
    if let Target::Remote(remote) = target {
        if json {
            let sent = remote::send_json(env, remote, branch, prompt, task);
            return emit(true, sent, |_| String::new());
        }
        return remote::send(env, remote, branch, prompt, task);
    }
    let branch = open()?.branch(branch)?;
    if json {
        let live = Live::start_to(env, task, true, true, branch.provider()?);
        let result = branch.send(prompt, live.options(task));
        live.console.finish();
        let branch = match result {
            Ok(branch) => branch,
            Err(error) => return fail(true, &error),
        };
        let descendants = wait_for_descendants(&[&branch]);
        descendants?;
        let sent = branchyard::Sent {
            name: branch.info().name.clone(),
            status: branch.info().status.clone(),
        };
        return print(&format!("{}\n", to_json(&sent)));
    }
    let delegating = task.delegate.is_some() || !branch.info().children.is_empty();
    let live = Live::start(env, task, delegating, branch.provider()?);
    let result = branch.send(prompt, live.options(task));
    live.finish(env, result)
}

pub fn fork(
    env: &Env,
    target: &Target,
    branch: &str,
    prompt: &str,
    fresh_session: bool,
    task: &TaskArgs,
) -> Outcome {
    if let Target::Remote(remote) = target {
        return remote::fork(env, remote, branch, prompt, fresh_session, task);
    }
    let branch = open()?.branch(branch)?;
    let live = Live::start(env, task, task.delegate.is_some(), branch.provider()?);
    let result = branch.fork(prompt, fresh_session, live.options(task));
    live.finish(env, result)
}

pub fn ls(env: &Env, target: &Target, as_json: bool) -> Outcome {
    let infos = match target {
        Target::Local => open()?.branches()?,
        Target::Remote(remote) => remote.repo.branches()?,
    };
    if as_json {
        let list = infos.iter().map(json::branch).collect();
        return print(&json::text(&serde_json::Value::Array(list)));
    }
    if infos.is_empty() {
        return print("no branches; start one with: by run \"<prompt>\"\n");
    }
    print(&render::branch_table(&infos, now(), env.style()))
}

pub fn show(env: &Env, target: &Target, branch: &str, as_json: bool) -> Outcome {
    let info = match target {
        Target::Local => open()?.branch(branch)?.info().clone(),
        Target::Remote(remote) => remote.repo.branch(branch)?,
    };
    if as_json {
        return print(&json::text(&json::branch(&info)));
    }
    print(&render::details(&info, now(), env.style()))
}

pub fn diff(env: &Env, target: &Target, branch: &str) -> Outcome {
    let diff = match target {
        Target::Local => open()?.branch(branch)?.diff()?,
        Target::Remote(remote) => remote.repo.diff(branch)?,
    };
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

pub fn log(env: &Env, target: &Target, branch: &str, as_json: bool) -> Outcome {
    let events = match target {
        Target::Local => open()?.branch(branch)?.events()?,
        Target::Remote(remote) => remote.repo.events(branch, 0)?.events,
    };
    if as_json {
        let list = events.iter().map(json::recorded).collect();
        return print(&json::text(&serde_json::Value::Array(list)));
    }
    print(&render::log_text(&events, env.style()))
}

pub fn merge(target: &Target, branch: &str, into: Option<&str>) -> Outcome {
    if let Target::Remote(remote) = target {
        return remote::merge(remote, branch, into);
    }
    let yard = open()?;
    let target = match into {
        Some(target) => target.to_owned(),
        None => current_branch(yard.root())?,
    };
    let merged = yard.merge(branch, &target)?;
    print_merged(
        &merged.branch,
        &merged.target,
        &merged.previous,
        &merged.commit,
    )
}

pub fn print_merged(branch: &str, target: &str, previous: &str, commit: &str) -> Outcome {
    let short = |commit: &str| commit.get(..10).unwrap_or(commit).to_owned();
    print(&format!(
        "merged {branch} into {target} ({}..{})\n",
        short(previous),
        short(commit)
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

pub fn rm(target: &Target, branch: &str) -> Outcome {
    match target {
        Target::Local => open()?.remove(branch)?,
        Target::Remote(remote) => remote.repo.remove(branch)?,
    }
    print(&format!("removed {branch}\n"))
}

/// Serve a branch's delegation tools on stdio; see `branchyard-mcp`.
pub fn mcp(args: &[String]) -> Outcome {
    branchyard_mcp::main_with_args(args).map_err(|e| Failure::Message(e.to_string()))
}

/// The delegate for this harness's branch when `by` runs inside a
/// delegating harness; `None` outside one.
fn harness_delegate(json: bool) -> Result<Option<Delegate>, Failure> {
    let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    if set(ENV_TOKEN) {
        return match Delegate::from_env() {
            Ok(delegate) => Ok(Some(delegate)),
            Err(error) => fail(json, &error).map(|()| None),
        };
    }
    if set(ENV_BRANCH) {
        let error = branchyard::Error::Denied(format!(
            "{ENV_BRANCH} is set but {ENV_TOKEN} is not: this harness was not given \
             delegation, and by will not act with your authority inside it"
        ));
        return fail(json, &error).map(|()| None);
    }
    Ok(None)
}

fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).expect("results serialize")
}

/// Report `error`: `{"error": {"kind", "message"}}` on stdout with
/// `--json`, else on stderr; exit 1 either way.
fn fail(json: bool, error: &branchyard::Error) -> Outcome {
    if json {
        let value =
            serde_json::json!({"error": {"kind": error.kind(), "message": error.to_string()}});
        print(&format!("{}\n", to_json(&value)))?;
        return Err(Failure::Reported);
    }
    eprintln!("by: {error}");
    Err(Failure::Reported)
}

/// Print a result as JSON or as text.
fn emit<T: Serialize>(
    json: bool,
    result: Result<T, branchyard::Error>,
    text: impl Fn(&T) -> String,
) -> Outcome {
    match result {
        Ok(value) if json => print(&format!("{}\n", to_json(&value))),
        Ok(value) => print(&text(&value)),
        Err(error) => fail(json, &error),
    }
}

/// Outside a harness: act as `branch` with your own authority.
fn as_user(branch: &str, options: TaskOptions) -> Result<Delegate, branchyard::Error> {
    open_yard()?.branch(branch)?.delegate(options)
}

fn required_outside(branch: Option<String>, command: &str) -> Result<String, branchyard::Error> {
    branch.ok_or_else(|| {
        branchyard::Error::Denied(format!("outside a harness, by {command} needs a branch"))
    })
}

pub fn spawn(env: &Env, target: &Target, prompt: &str, args: &SpawnArgs) -> Outcome {
    let json = args.json;
    let task = &args.task;
    let request = Spawn {
        prompt: prompt.to_owned(),
        harness: task.harness.clone(),
        name: task.name.clone(),
        base: task.base.clone(),
        budget: Budget {
            max_usd: task.budget_usd,
            max_turns: task.max_turns,
            max_duration: task.max_duration,
        },
        check: task.check.clone(),
        max_depth: args.max_depth,
        deny: args.deny.clone(),
        ..Spawn::default()
    };
    if let Some(delegate) = harness_delegate(json)? {
        if args.parent.is_some() || task.permissions != args::Permissions::Unset {
            let error = branchyard::Error::Denied(
                "inside a harness, the parent is the harness's own branch and the child \
                 inherits its policy; drop --parent, --yes and --ask"
                    .into(),
            );
            return fail(json, &error);
        }
        let spawned = match delegate.spawn(request) {
            Ok(spawned) => spawned,
            Err(error) => return fail(json, &error),
        };
        if !args.wait {
            return emit(json, Ok(spawned), |s| {
                format!(
                    "spawned {} on {} from {}\n",
                    s.name,
                    s.profile,
                    short(&s.base)
                )
            });
        }
        let done = delegate.wait(&spawned.name, std::time::Duration::MAX);
        return emit(json, done, |i| render::inspection(i, env.style()));
    }
    // Outside a harness the child runs on this process's threads, so the
    // command waits for it.
    let parent = match required_outside(args.parent.clone(), "spawn --parent") {
        Ok(parent) => parent,
        Err(error) => return fail(json, &error),
    };
    if let Target::Remote(remote) = target {
        let result = remote::spawn(env, remote, &parent, prompt, args);
        return emit(json, result, |i| render::inspection(i, env.style()));
    }
    let live = Live::start_to(env, task, true, json, None);
    let result = (|| {
        // One yard, so the wait sees the child's thread.
        let parent = open_yard()?.branch(&parent)?;
        let delegate = parent.delegate(live.options(task))?;
        let spawned = delegate.spawn(request)?;
        parent.wait_subtree()?;
        delegate.inspect(&spawned.name)
    })();
    live.console.finish();
    emit(json, result, |i| render::inspection(i, env.style()))
}

pub fn inspect(env: &Env, target: &Target, branch: Option<String>, json: bool) -> Outcome {
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => {
            let branch = branch.unwrap_or_else(|| delegate.branch().to_owned());
            delegate.inspect(&branch)
        }
        (None, Target::Remote(remote)) => {
            required_outside(branch, "inspect").and_then(|b| remote::inspect(remote, &b))
        }
        (None, Target::Local) => required_outside(branch, "inspect")
            .and_then(|b| as_user(&b, TaskOptions::default())?.inspect(&b)),
    };
    emit(json, result, |i| render::inspection(i, env.style()))
}

pub fn events(
    env: &Env,
    target: &Target,
    branch: Option<String>,
    cursor: Option<usize>,
    limit: Option<usize>,
    json: bool,
) -> Outcome {
    let limit = limit.unwrap_or(50);
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => {
            let branch = branch.unwrap_or_else(|| delegate.branch().to_owned());
            delegate.events(&branch, cursor, limit)
        }
        (None, Target::Remote(remote)) => required_outside(branch, "events")
            .and_then(|b| remote::events(remote, &b, cursor, limit)),
        (None, Target::Local) => required_outside(branch, "events")
            .and_then(|b| as_user(&b, TaskOptions::default())?.events(&b, cursor, limit)),
    };
    emit(json, result, |page| {
        format!(
            "{}next cursor: {} of {}\n",
            render::log_text(&page.events, env.style()),
            page.next_cursor,
            page.total
        )
    })
}

pub fn integrate(target: &Target, branch: &str, json: bool) -> Outcome {
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => delegate.integrate(branch),
        (None, Target::Remote(remote)) => remote::integrate(remote, branch),
        // A person integrates a child into the parent that delegated it.
        (None, Target::Local) => (|| {
            let yard = open_yard()?;
            let info = yard.branch(branch)?.info().clone();
            let parent = info
                .parent
                .filter(|p| {
                    yard.branch(p)
                        .is_ok_and(|p| p.info().children.iter().any(|c| c == branch))
                })
                .ok_or_else(|| {
                    branchyard::Error::Denied(format!(
                        "{branch} was not delegated by another branch; merge it with by merge"
                    ))
                })?;
            as_user(&parent, TaskOptions::default())?.integrate(branch)
        })(),
    };
    emit(json, result, |m| {
        format!(
            "merged {} into {} ({}..{})\n",
            m.branch,
            m.target,
            short(&m.previous),
            short(&m.commit)
        )
    })
}

/// Inside a harness, cancel a descendant with the branch's authority;
/// otherwise cancel the branch and its descendants, locally or on the
/// server.
pub fn cancel(target: &Target, branch: &str, json: bool) -> Outcome {
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => delegate.cancel(branch),
        (None, Target::Remote(remote)) => {
            let cancelled = remote.repo.cancel(branch)?;
            Ok(branchyard::Cancelled { cancelled })
        }
        (None, Target::Local) => open_yard()
            .and_then(|yard| yard.cancel_as(branch, "by cancel"))
            .map(|cancelled| branchyard::Cancelled { cancelled }),
    };
    emit(json, result, |c| match c.cancelled.is_empty() {
        true => "nothing was running\n".into(),
        false => format!("asked {} to stop\n", c.cancelled.join(", ")),
    })
}

pub fn children(env: &Env, target: &Target, branch: Option<String>, json: bool) -> Outcome {
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => match branch {
            Some(other) if other != delegate.branch() => Err(branchyard::Error::Denied(
                "inside a harness, by children lists your own branch's descendants".into(),
            )),
            _ => delegate.children(),
        },
        (None, Target::Remote(remote)) => {
            required_outside(branch, "children").and_then(|b| remote::children(remote, &b))
        }
        (None, Target::Local) => required_outside(branch, "children")
            .and_then(|b| as_user(&b, TaskOptions::default())?.children()),
    };
    emit(json, result, |c| match c.descendants.is_empty() {
        true => format!("{} has no children\n", c.branch),
        false => render::branch_table(&c.descendants, now(), env.style()),
    })
}

fn short(commit: &str) -> &str {
    commit.get(..10).unwrap_or(commit)
}

pub fn harnesses(env: &Env, target: &Target, as_json: bool) -> Outcome {
    let harnesses = match target {
        Target::Local => open()?.harnesses(),
        Target::Remote(remote) => remote.client.harnesses()?,
    };
    if as_json {
        let list = harnesses.iter().map(json::harness).collect();
        return print(&json::text(&serde_json::Value::Array(list)));
    }
    print(&render::harness_table(&harnesses, env.style()))
}

/// `path` made absolute against the current directory, since the branch
/// stores it and later commands may run elsewhere.
fn absolute(path: &str) -> std::path::PathBuf {
    let path = std::path::PathBuf::from(path);
    match path.is_absolute() {
        true => path,
        false => std::env::current_dir()
            .map(|dir| dir.join(&path))
            .unwrap_or(path),
    }
}

/// The SDK provider for `--provider`, if given.
pub(crate) fn provider(task: &TaskArgs) -> Option<Provider> {
    if let Some(substrate) = &task.substrate {
        return Some(Provider::Substrate(SubstrateOptions {
            endpoint: substrate.endpoint.clone(),
            router: substrate.router.clone(),
            atespace: substrate.atespace.clone().unwrap_or_default(),
            template: substrate.template.clone(),
            key: absolute(&substrate.key),
            workdir: substrate.workdir.clone().unwrap_or_default(),
            home: substrate.home.clone().unwrap_or_default(),
            pass_env: substrate.pass_env.clone(),
        }));
    }
    match (&task.sandbox, task.local) {
        (Some(sandbox), _) => Some(Provider::Microsandbox(SandboxOptions {
            image: sandbox.image.clone(),
            cpus: sandbox.cpus,
            memory_mib: sandbox.memory_mib,
            pass_env: sandbox.pass_env.clone(),
        })),
        (None, true) => Some(Provider::Local),
        (None, false) => None,
    }
}
