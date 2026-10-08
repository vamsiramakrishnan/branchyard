//! One function per command, each a thin call into the SDK locally or into
//! `branchyard-client` remotely ([`crate::remote`]).

use branchyard_support::best_effort;
use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use branchyard::{
    Activity, Branch, BranchInfo, BranchStatus, Budget, Delegate, Envelope, Event, Policy,
    Provider, RecordedEvent, RemoveOptions, SandboxOptions, Spawn, SubstrateOptions, TaskOptions,
    Yard, ENV_BRANCH, ENV_TOKEN,
};
use serde::Serialize;

use crate::args::{
    self, shell_quote, ArtifactArgs, GraphArgs, Permissions, ScratchArgs, SpawnArgs, TaskArgs,
};
use crate::console::{self, Choice, Console};
use crate::json;
use crate::remote::{self, Remote};
use crate::render::{self, Renderer, Style, Tone};
use crate::rig;

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
    /// Whether and how to say a branch needs you or ended; off until
    /// `main` resolves it from the flags and configuration.
    pub notify: crate::notify::Settings,
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
            notify: crate::notify::Settings::default(),
        }
    }

    pub(crate) fn style(&self) -> Style {
        Style { color: self.color }
    }

    /// The notifier for a command that waits for branches: its escapes go
    /// to stderr when that is a terminal.
    pub fn notifier(&self) -> Option<crate::notify::Notifier> {
        let out: Option<Box<dyn Write + Send>> = match self.stderr_tty {
            true => Some(Box::new(io::stderr())),
            false => None,
        };
        crate::notify::Notifier::new(self.notify, out)
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

/// An argument clap requires for the action it is parsed under. A missing
/// one is reported, never a panic.
fn given<T>(value: Option<T>, name: &str) -> Result<T, Failure> {
    value.ok_or_else(|| Failure::Message(format!("missing the {name} argument")))
}

pub fn print(text: &str) -> Outcome {
    let mut stdout = io::stdout().lock();
    stdout.write_all(text.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

/// The yard: the harness's repository when `by` runs inside a harness,
/// whose working directory is its branch's worktree, else the current one.
/// An inherited `BRANCHYARD_ROOT` that is neither is refused; see
/// [`crate::inherited`].
pub fn open() -> Result<Yard, Failure> {
    Ok(open_yard()?)
}

fn open_yard() -> Result<Yard, branchyard::Error> {
    let yard = match crate::inherited::root()? {
        Some(root) => Yard::open(root),
        None => Yard::open("."),
    }?;
    configure(yard)
}

/// `yard` with the gateways and knowledge settings the configuration
/// gives it, as [`open`] returns it.
pub(crate) fn configure(yard: Yard) -> Result<Yard, branchyard::Error> {
    // `[connectors]`: the gateway its branches' turns are given.
    crate::gateway_cmd::configure(&yard)
        .map_err(|e| branchyard::Error::Unsupported(format!("[connectors]: {e}")))?;
    // `[approvals]`: your policy for tools and connector operations
    // (docs/effects.md).
    crate::effects_cmd::configure(&yard)
        .map_err(|e| branchyard::Error::Unsupported(format!("[approvals]: {e}")))?;
    // `[models]`: the model gateway its branches' turns may use
    // (docs/model-gateway.md).
    crate::models_cmd::configure(&yard)
        .map_err(|e| branchyard::Error::Unsupported(format!("[models]: {e}")))?;
    // `[knowledge]`: what its branches are given and when they are
    // distilled (docs/knowledge.md).
    crate::knowledge_cmd::configure(&yard)
        .map_err(|e| branchyard::Error::Unsupported(format!("[knowledge]: {e}")))?;
    Ok(yard)
}

/// Console and policy for commands that run a harness.
pub(crate) struct Live {
    pub(crate) console: Arc<Console>,
    policy: Policy,
}

impl Live {
    /// `branch` is the provider a send or fork inherits when the flags name
    /// none.
    pub(crate) fn start(
        env: &Env,
        task: &TaskArgs,
        prefixed: bool,
        branch: Option<Provider>,
    ) -> Live {
        Live::start_to(env, task, prefixed, false, branch)
    }

    /// With `json`, activity goes to stderr so stdout holds only the result.
    pub(crate) fn start_to(
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
        let console = Arc::new(
            Console::new(
                Renderer::new(env.style(), prefixed),
                out,
                Box::new(console::terminal_prompt),
            )
            .with_notifier(env.notifier()),
        );
        let choice = console::choose(task.permissions, env.stdin_tty, env.stderr_tty);
        // A recipe that cannot be used is refused by `options`, below.
        match provider(task).ok().flatten().or(branch) {
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
            Some(Provider::Recipe(options)) => eprintln!(
                "by: harnesses run on machines recipe {} makes; the worktree is copied there and \
                 back",
                options.name
            ),
        }
        if choice == (Choice::DenyAll { notice: true }) {
            eprintln!("by: {}", console::DENY_NOTICE);
        }
        let policy = console::policy(choice, console.clone());
        Live { console, policy }
    }

    pub(crate) fn options(&self, task: &TaskArgs) -> Result<TaskOptions, Failure> {
        if !task.require_labels.is_empty() {
            return Err(Failure::Message(
                "--require-label chooses among a server's workers: use it with --remote".into(),
            ));
        }
        if task.priority.is_some() {
            return Err(Failure::Message(
                "--priority orders a server's queue: use it with --remote".into(),
            ));
        }
        let console = self.console.clone();
        let exe = std::env::current_exe().ok();
        let policy = match (&exe, task.allow_delegation) {
            (Some(by), true) => self.policy.clone().allow_delegation_commands(by),
            _ => self.policy.clone(),
        };
        Ok(TaskOptions {
            harness: task.harness.clone(),
            name: task.name.clone(),
            base: task.base.clone(),
            budget: Budget {
                max_usd: task.budget_usd,
                max_turns: task.max_turns,
                max_duration: task.max_duration,
                stall_after: task.stall_after,
                stall_action: task.stall_action,
            },
            policy,
            check: task.check.clone(),
            observer: Some(Arc::new(move |event| console.event(event))),
            isolated: task.isolated,
            command: task.command.clone(),
            provider: provider(task)?,
            delegation: task.delegate.map(|depth| match task.no_wake {
                true => Envelope::depth(depth).no_wake(),
                false => Envelope::depth(depth),
            }),
            delegation_cli: exe,
            delegation_server: None,
            unapproved_tools: task.unapproved_tools,
            provision: provision(task)?,
            seats: None,
            workspace: None,
            actor: None,
            // A local harness inherits this process's environment,
            // `TRACEPARENT` included.
            trace_parent: None,
            plan: task.plan,
            goal: crate::plan_cmd::goal(task),
            join_task: None,
            deny: task.deny.clone(),
        })
    }

    /// Print the closing summary for one branch, once every branch it
    /// delegated to on this process has finished.
    pub(crate) fn finish(self, env: &Env, result: Result<Branch, branchyard::Error>) -> Outcome {
        let branch = match result {
            Ok(branch) => branch,
            Err(error) => {
                self.console.finish();
                return Err(error.into());
            }
        };
        let descendants = wait_for_descendants(&[&branch]);
        // A branch woken when its children settled ran more turns since.
        let branch = branch.yard().branch(&branch.info().name).unwrap_or(branch);
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
pub(crate) fn wait_for_descendants(
    branches: &[&Branch],
) -> Result<Option<Vec<BranchInfo>>, Failure> {
    let mut all = Vec::new();
    for branch in branches {
        if branch.info().status == BranchStatus::WaitingOnChildren {
            eprintln!(
                "by: {}'s turn ended while its children run; its next turn starts when they \
                 settle",
                branch.info().name
            );
        }
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

/// The repository's `[workspace]` for a command that creates branches,
/// once its scripts are trusted (docs/workspace.md).
pub(crate) fn workspace(
    env: &Env,
    yard: &Yard,
) -> Result<Option<branchyard::WorkspaceSpec>, Failure> {
    crate::workspace_cmd::for_new_branch(env, yard.root())
}

pub fn run(env: &Env, target: &Target, prompt: &str, task: &TaskArgs) -> Outcome {
    if let Target::Remote(remote) = target {
        if crate::fleet_cmd::is_routed(task) || task.kind.is_some() {
            return Err(crate::fleet_cmd::local_only());
        }
        // The issue's link lives in the prompt's header on a server.
        let (prompt, task, _) = crate::pr::issue_task(prompt, task, None)?;
        return remote::run(env, remote, &prompt, &task);
    }
    run_in(env, &open()?, prompt, task, None)
}

/// `by run`'s local half, in `yard`. `join` makes the branch another
/// attempt of that task, from its `main` (a task with a repository of its
/// own; docs/task-repos.md).
pub(crate) fn run_in(
    env: &Env,
    yard: &Yard,
    prompt: &str,
    task: &TaskArgs,
    join: Option<&str>,
) -> Outcome {
    let yard = yard.clone();
    if !crate::fleet_cmd::is_routed(task) {
        // A login near its 5-hour or weekly limit (docs/usage.md).
        let harness = task.harness.clone().unwrap_or_else(|| "claude-code".into());
        crate::usage::guard(&[harness])?;
    }
    let (prompt, task, issue) = crate::pr::issue_task(prompt, task, Some(&yard))?;
    let task = &task;
    let workspace = workspace(env, &yard)?;
    let live = Live::start(env, task, task.delegate.is_some(), None);
    let mut options = TaskOptions {
        workspace,
        ..live.options(task)?
    };
    if let Some(id) = join {
        options.join_task = Some(id.to_owned());
        options.base = Some("main".into());
    }
    // `[fleet.<kind>] plan` and `goal_judge` (docs/plans-and-goals.md).
    let options = crate::plan_cmd::with_fleet(options, task, &prompt);
    // Routed (docs/fleet.md): the router picks the harness and fails over;
    // the branch that ends the chain is the one summarized.
    let result = match (crate::fleet_cmd::is_routed(task), task.kind) {
        (true, _) => match crate::fleet_cmd::routed(&yard, &prompt, &options, task, false, None) {
            Ok(mut routed) => Ok(routed.branches.remove(0)),
            Err(Failure::Sdk(error)) => Err(error),
            Err(other) => {
                live.console.finish();
                return Err(other);
            }
        },
        (false, Some(kind)) => yard.run_with_kind(&prompt, &options, kind),
        (false, None) => yard.task(prompt).options(options).run(),
    };
    if let (Ok(branch), Some(issue)) = (&result, &issue) {
        crate::pr::link_issue(branch, issue)?;
    }
    live.finish(env, result)
}

/// `by fan`'s routing and judging; see docs/fleet.md.
#[derive(Clone, Debug, Default)]
pub struct FanRoute {
    /// Branches to start when routed, instead of the entry's attempts.
    pub attempts: Option<u32>,
    /// Judge the attempts afterwards.
    pub judge: bool,
}

pub fn fan(
    env: &Env,
    target: &Target,
    prompt: &str,
    harnesses: Option<&[String]>,
    task: &TaskArgs,
    route: &FanRoute,
) -> Outcome {
    let routed = crate::fleet_cmd::is_routed(task);
    if harnesses.is_some() && task.auto {
        return Err(Failure::Message(
            "--auto routes through the [fleet] table; it takes no --harness".into(),
        ));
    }
    let harnesses = match (harnesses, routed) {
        (Some(harnesses), _) => harnesses,
        (None, true) => &[],
        (None, false) => {
            return Err(Failure::Message(
                "fan needs --harness ID,ID,... or a [fleet] table to route by (--auto); see \
                 docs/fleet.md"
                    .into(),
            ))
        }
    };
    if let Target::Remote(remote) = target {
        if routed || task.kind.is_some() || route.judge {
            return Err(crate::fleet_cmd::local_only());
        }
        let (prompt, task, _) = crate::pr::issue_task(prompt, task, None)?;
        return remote::fan(env, remote, &prompt, harnesses, &task);
    }
    let yard = open()?;
    if !routed {
        // Logins near their 5-hour or weekly limits (docs/usage.md).
        crate::usage::guard(harnesses)?;
    }
    let (prompt, task, issue) = crate::pr::issue_task(prompt, task, Some(&yard))?;
    let (prompt, task) = (prompt.as_str(), &task);
    let workspace = workspace(env, &yard)?;
    let live = Live::start(env, task, true, None);
    let ids: Vec<&str> = harnesses.iter().map(String::as_str).collect();
    let options = TaskOptions {
        workspace,
        ..live.options(task)?
    };
    let options = crate::plan_cmd::with_fleet(options, task, prompt);
    let result = match routed {
        true => {
            match crate::fleet_cmd::routed(&yard, prompt, &options, task, true, route.attempts) {
                Ok(routed) => Ok(routed.branches),
                Err(Failure::Sdk(error)) => Err(error),
                Err(other) => {
                    live.console.finish();
                    return Err(other);
                }
            }
        }
        false => {
            let builder = yard.task(prompt).options(options);
            // Knowing the names up front lines the prefixes up from the
            // first line.
            if let Ok(names) = builder.planned_names(&ids) {
                live.console.reserve(&names);
            }
            builder.run_on(&ids)
        }
    };
    let branches = match result {
        Ok(branches) => branches,
        Err(error) => {
            live.console.finish();
            return Err(error.into());
        }
    };
    if let Some(issue) = &issue {
        for branch in &branches {
            crate::pr::link_issue(branch, issue)?;
        }
    }
    let descendants = wait_for_descendants(&branches.iter().collect::<Vec<_>>());
    live.console.finish();
    let descendants = descendants?.unwrap_or_default();
    let infos: Vec<&BranchInfo> = branches
        .iter()
        .map(Branch::info)
        .chain(descendants.iter())
        .collect();
    let summary = fan_summary(env, &infos);
    if route.judge {
        let names: Vec<String> = branches.iter().map(|b| b.info().name.clone()).collect();
        let judgement =
            crate::fleet_cmd::judge_names(&yard, &names, None, None, task.fleet.as_ref())?;
        print(&format!(
            "\n{}",
            crate::fleet_cmd::judgement_table(&judgement, env.style())
        ))?;
        if judgement.pick.is_some() {
            print(&format!(
                "\n{}\n  by judge {} --pick\n",
                env.style().paint(Tone::Dim, "next"),
                names.join(" ")
            ))?;
        }
    }
    summary
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

/// What `by send` submits.
#[derive(Clone, Copy)]
pub enum Prompt<'a> {
    Text(&'a str),
    /// `--retry`: the prompt of the branch's last turn that was cut off.
    Retry,
}

pub fn send(
    env: &Env,
    target: &Target,
    branch: &str,
    prompt: Prompt<'_>,
    task: &TaskArgs,
    wait: bool,
    json: bool,
) -> Outcome {
    if let Some(delegate) = harness_delegate(json)? {
        if *task != TaskArgs::default() {
            return fail(
                json,
                &branchyard::Error::Denied(
                    "inside a harness, send takes only --wait and --json; the child keeps its \
                     own limits"
                        .into(),
                ),
            );
        }
        let sent = match prompt {
            Prompt::Text(prompt) => delegate.send(branch, prompt),
            Prompt::Retry => delegate.retry(branch),
        };
        if wait {
            let done = sent.and_then(|_| delegate.wait(branch, std::time::Duration::MAX));
            return emit(json, done, |i| render::inspection(i, env.style()));
        }
        return emit(json, sent, |sent| {
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
    let retried: String;
    let prompt = match prompt {
        Prompt::Text(prompt) => prompt,
        Prompt::Retry => {
            retried = match open_yard().and_then(|yard| yard.branch(branch)?.retry_prompt()) {
                Ok(prompt) => prompt,
                Err(error) => return fail(json, &error),
            };
            &retried
        }
    };
    let branch = open()?.branch(branch)?;
    if json {
        let live = Live::start_to(env, task, true, true, branch.provider()?);
        let options = live.options(task)?;
        let result = failed_over(branch.send(prompt, options.clone()), &options);
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
    let options = live.options(task)?;
    let result = failed_over(branch.send(prompt, options.clone()), &options);
    live.finish(env, result)
}

/// A routed branch whose turn failed for its harness goes on, on the next
/// candidate, when its route asked for failover (docs/fleet.md).
fn failed_over(
    result: Result<Branch, branchyard::Error>,
    options: &TaskOptions,
) -> Result<Branch, branchyard::Error> {
    let sent = result?;
    match sent.yard().failover(&sent.info().name, options)? {
        Some(next) => {
            eprintln!(
                "by: {}'s harness failed; the task went on as {}",
                sent.info().name,
                next.info().name
            );
            Ok(next)
        }
        None => Ok(sent),
    }
}

/// How long `by send --steer` waits for the engine running the turn to
/// deliver the input.
const STEER_WAIT: Duration = Duration::from_secs(10);

/// `by send --steer`: add `prompt` to `branch`'s running turn. Inside a
/// harness, as that harness's branch and only to a descendant; outside, with
/// your authority. Waits for delivery; a refusal exits with failure.
pub fn steer(target: &Target, branch: &str, prompt: &str, task: &TaskArgs, json: bool) -> Outcome {
    if *task != TaskArgs::default() {
        return fail(
            json,
            &branchyard::Error::Denied(
                "send --steer takes only --json: the running turn keeps its own limits and \
                 permissions"
                    .into(),
            ),
        );
    }
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => delegate.steer(branch, prompt),
        (None, Target::Remote(remote)) => {
            remote
                .repo
                .steer(branch, prompt)
                .map_err(|e| branchyard::Error::Remote {
                    kind: e.code().unwrap_or("remote").to_owned(),
                    message: e.to_string(),
                    detail: None,
                })
        }
        (None, Target::Local) => open_yard().and_then(|yard| {
            let steer = yard.steer_as(branch, prompt, "by send --steer")?;
            yard.wait_steer(branch, steer.id, STEER_WAIT)
        }),
    };
    let steer = match result {
        Ok(steer) => steer,
        Err(error) => return fail(json, &error),
    };
    match &steer.state {
        branchyard::SteerState::Refused { reason } if json => {
            let value = serde_json::json!({"error": {
                "kind": "steer_refused",
                "message": format!("{branch}'s turn did not take the input: {reason}"),
                "steer": steer,
            }});
            print(&format!("{}\n", to_json(&value)))?;
            Err(Failure::Reported)
        }
        branchyard::SteerState::Refused { reason } => {
            eprintln!("by: {branch}'s turn did not take the input: {reason}");
            Err(Failure::Reported)
        }
        _ if json => print(&format!("{}\n", to_json(&steer))),
        // The state first, in the words the delegate skill and the JSON
        // use, then what it means; the boundary in plain words.
        branchyard::SteerState::Pending => print(&format!(
            "pending: queued for {branch}'s running turn; its engine has not delivered it yet \
             (steer {})\n",
            steer.id
        )),
        branchyard::SteerState::Written => print(&format!(
            "written: sent to {branch}'s harness for its running turn; the harness has not \
             confirmed it yet (steer {})\n",
            steer.id
        )),
        branchyard::SteerState::Accepted => print(&format!(
            "accepted: it joined {branch}'s running turn, and the model reads it {} (steer {})\n",
            branchyard_harness::steer_boundary_text(steer.boundary.as_deref().unwrap_or("")),
            steer.id
        )),
    }
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
    let yard = open()?;
    let workspace = workspace(env, &yard)?;
    let branch = yard.branch(branch)?;
    let live = Live::start(env, task, task.delegate.is_some(), branch.provider()?);
    let options = TaskOptions {
        workspace,
        ..live.options(task)?
    };
    let result = branch.fork(prompt, fresh_session, options);
    live.finish(env, result)
}

/// `by fork BRANCH --at N`: a new branch from checkpoint N, saying how its
/// session continues.
pub fn fork_at(
    env: &Env,
    target: &Target,
    branch: &str,
    turn: u32,
    prompt: &str,
    task: &TaskArgs,
) -> Outcome {
    if let Target::Remote(_) = target {
        return Err(Failure::Message(
            "fork --at is not available in remote mode yet; run it on the server's host".into(),
        ));
    }
    let branch = open()?.branch(branch)?;
    let live = Live::start(env, task, task.delegate.is_some(), branch.provider()?);
    let result = branch.fork_at(turn, prompt, live.options(task)?);
    if let Ok(forked) = &result {
        crate::attempts::announce_fork(forked);
    }
    live.finish(env, result)
}

pub fn reincarnate(env: &Env, target: &Target, branch: &str, task: &TaskArgs) -> Outcome {
    if let Target::Remote(remote) = target {
        return remote::reincarnate(env, remote, branch, task);
    }
    let yard = open()?;
    let workspace = workspace(env, &yard)?;
    let branch = yard.branch(branch)?;
    let live = Live::start(env, task, task.delegate.is_some(), branch.provider()?);
    let options = TaskOptions {
        workspace,
        ..live.options(task)?
    };
    let result = branch.reincarnate(options);
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
    // Recorded maps and their progress follow the branches (docs/map.md).
    let maps = crate::map_cmd::ls_section(target, env.style()).unwrap_or_default();
    if infos.is_empty() {
        return print(&format!(
            "no branches; start one with: by run \"<prompt>\"\n{maps}"
        ));
    }
    print(&format!(
        "{}{maps}",
        render::branch_table(
            &infos,
            branchyard_support::time::now_ms() / 1000,
            env.style()
        )
    ))
}

/// `by show`, with the merge-readiness line `by pr` and `by pr --watch`
/// recorded; `refresh` asks GitHub first (local mode).
/// `by show` of a branch a queued operation will create.
fn show_queued(branch: &str, op: &branchyard_client::api::Operation, as_json: bool) -> Outcome {
    if as_json {
        let value = serde_json::json!({ "branch": branch, "operation": op });
        return print(&json::text(&value));
    }
    let word = |value: serde_json::Value| value.as_str().unwrap_or_default().to_owned();
    let mut text = format!(
        "branch    {branch} (not created yet)\noperation {} ({}, {})\n",
        op.id,
        word(serde_json::to_value(op.kind).unwrap_or_default()),
        word(serde_json::to_value(op.state).unwrap_or_default()),
    );
    if !op.requires.is_empty() {
        text.push_str(&format!("requires  {}\n", op.requires.join(", ")));
    }
    if let Some(waiting) = &op.waiting {
        text.push_str(&format!("waiting   {waiting}\n"));
    }
    print(&text)
}

pub fn show(env: &Env, target: &Target, branch: &str, as_json: bool, refresh: bool) -> Outcome {
    let mut listening = None;
    let (info, events) = match target {
        Target::Local => {
            let yard = open()?;
            if refresh {
                crate::pr::refresh(&yard, branch)?;
            }
            let branch = match yard.branch(branch) {
                Ok(branch) => branch,
                // A map's name shows the map (docs/map.md).
                Err(error @ branchyard::Error::UnknownBranch(_)) => {
                    return match crate::map_cmd::show_if_map(env, target, branch, as_json) {
                        Some(shown) => shown,
                        None => Err(error.into()),
                    }
                }
                Err(error) => return Err(error.into()),
            };
            listening = Some(
                crate::ports::of_yard(&yard)
                    .remove(&branch.info().name)
                    .unwrap_or_default(),
            );
            (branch.info().clone(), branch.events()?)
        }
        Target::Remote(_) if refresh => {
            return Err(Failure::Sdk(branchyard::Error::Unsupported(
                "by show --refresh asks GitHub about a pull request by pr opened from this \
                 repository; it works in local mode only"
                    .into(),
            )))
        }
        Target::Remote(remote) => match remote.repo.branch(branch) {
            Ok(info) => (info, remote.repo.events(branch, 0)?.events),
            // Not created yet: an operation that will create it may be
            // queued, and say why no worker has claimed it.
            Err(error) if error.code() == Some("unknown_branch") => {
                match remote.repo.operations(Some(branch))?.into_iter().next() {
                    Some(op) => return show_queued(branch, &op, as_json),
                    None => {
                        return match crate::map_cmd::show_if_map(env, target, branch, as_json) {
                            Some(shown) => shown,
                            None => Err(error.into()),
                        }
                    }
                }
            }
            Err(error) => return Err(error.into()),
        },
    };
    let checkpoints = crate::attempts::checkpoints(target, &info)?;
    let (readiness, line) = crate::pr::show_readiness(&info, &events, env.style());
    // The task it is an attempt of (docs/task-repos.md); a server too old
    // to say has none.
    let task = match target {
        Target::Local => open()
            .ok()
            .and_then(|yard| branchyard::tasks::view(&yard, &info.name).ok()),
        Target::Remote(remote) => remote.repo.task(&info.name).ok(),
    }
    .filter(|view| view.attempts.iter().any(|a| a.name == info.name))
    .map(|view| crate::task_cmd::show_line(&view, &info.name));
    if as_json {
        let mut value = json::branch(&info);
        if let Some((_, task)) = &task {
            value["task"] = task.clone();
        }
        value["checkpoints"] = serde_json::to_value(&checkpoints).unwrap_or_default();
        value["merge_readiness"] = readiness;
        // Only when something listens, so local and remote `show --json`
        // agree for a branch with no servers.
        if let Some(listening) = listening.as_ref().filter(|l| !l.is_empty()) {
            value["listening"] = serde_json::to_value(listening).unwrap_or_default();
        }
        // Its plan and goal, when it has them (docs/plans-and-goals.md).
        crate::plan_cmd::show_json(&info.name, &events, &mut value);
        // How its last turn's network policy was applied (docs/egress.md).
        if let Some((summary, _)) = egress_summary(&events) {
            value["egress"] = summary;
        }
        // Its calls through the model gateway (docs/model-gateway.md).
        if let Some((summary, _)) = crate::models_cmd::summary(&events) {
            value["models"] = summary;
        }
        // Its effects and waiting approvals (docs/effects.md).
        if let Some((summary, _)) = crate::effects_cmd::summary(&events) {
            value["effects"] = summary;
        }
        return print(&json::text(&value));
    }
    let mut extra: Vec<(&str, String)> = line
        .map(|line| ("merge readiness", line))
        .into_iter()
        .collect();
    if let Some((text, _)) = task {
        extra.push(("task", text));
    }
    if let Some((_, text)) = egress_summary(&events) {
        extra.push(("egress", text));
    }
    if let Some((_, text)) = crate::models_cmd::summary(&events) {
        extra.push(("models", text));
    }
    if let Some((_, text)) = crate::effects_cmd::summary(&events) {
        extra.push(("effects", text));
    }
    if let Some(listening) = listening.filter(|l| !l.is_empty()) {
        extra.push(("listening", crate::ports::lines(&listening).join("; ")));
    }
    extra.extend(crate::plan_cmd::show_lines(&info.name, &events));
    let mut text = render::details(
        &info,
        branchyard_support::time::now_ms() / 1000,
        env.style(),
        extra,
    );
    text.push_str(&crate::attempts::checkpoint_lines(
        &checkpoints,
        env.style(),
    ));
    print(&text)
}

/// How the branch's last turn with a network policy applied it, and what
/// its proxy decided in that turn: as JSON and as one line. `None` when no
/// turn had one.
pub(crate) fn egress_summary(
    events: &[branchyard::RecordedEvent],
) -> Option<(serde_json::Value, String)> {
    use branchyard::EgressActivity;
    let at = events.iter().rposition(|e| {
        matches!(&e.activity, Activity::Egress(egress)
            if matches!(egress.as_ref(), EgressActivity::Applied { .. }))
    })?;
    let Activity::Egress(applied) = &events[at].activity else {
        return None;
    };
    let EgressActivity::Applied {
        policy,
        allow,
        enforcement,
        reason,
    } = applied.as_ref()
    else {
        return None;
    };
    let (mut allowed, mut denied) = (0, 0);
    for event in &events[at + 1..] {
        if let Activity::Egress(egress) = &event.activity {
            if let EgressActivity::Decision { allowed: yes, .. } = egress.as_ref() {
                match yes {
                    true => allowed += 1,
                    false => denied += 1,
                }
            }
        }
    }
    let mut text = format!(
        "{} ({policy}); {allowed} allowed, {denied} denied",
        enforcement.as_str()
    );
    if let Some(reason) = reason {
        text.push_str(&format!("; not enforced: {reason}"));
    }
    let value = serde_json::json!({
        "policy": policy,
        "allow": allow,
        "enforcement": enforcement,
        "reason": reason,
        "allowed": allowed,
        "denied": denied,
    });
    Some((value, text))
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

pub fn log(env: &Env, target: &Target, branch: &str, as_json: bool, follow: bool) -> Outcome {
    if follow {
        return log_follow(env, target, branch, as_json);
    }
    let events = match target {
        Target::Local => {
            let yard = open()?;
            // The gateway's newest calls, if it has written any.
            best_effort(
                "ingest the connector audit log",
                yard.ingest_connector_audit(),
            );
            yard.branch(branch)?.events()?
        }
        Target::Remote(remote) => remote.repo.events(branch, 0)?.events,
    };
    if as_json {
        let list = events.iter().map(json::recorded).collect();
        return print(&json::text(&serde_json::Value::Array(list)));
    }
    print(&render::log_text(&events, env.style()))
}

/// How often `by log --follow` asks a server for new events.
const FOLLOW_POLL: Duration = Duration::from_millis(500);

/// `by log --follow`: print what is recorded, then each new event as it is
/// recorded, until interrupted. Locally it waits on the store; remotely it
/// polls the branch's events by cursor and rides out a server restart.
/// Message text still streaming is held back until the stream pauses, so
/// a reply is not cut into many stamped pieces.
fn log_follow(env: &Env, target: &Target, branch: &str, as_json: bool) -> Outcome {
    let yard = match target {
        Target::Local => Some(open()?),
        Target::Remote(_) => None,
    };
    let local = match &yard {
        Some(yard) => Some(yard.branch(branch)?),
        None => None,
    };
    let mut cursor = 0u64;
    let mut held: Vec<RecordedEvent> = Vec::new();
    let mut unreachable = false;
    loop {
        let (events, next) = match (&local, target) {
            (Some(branch), _) => {
                // The gateway's newest calls, as connector_call events.
                if let Some(yard) = &yard {
                    branchyard_support::best_effort_once(
                        "ingest the connector audit log",
                        yard.ingest_connector_audit(),
                    );
                }
                let page = branch.wait_for_events(cursor, 500, FOLLOW_POLL)?;
                (page.events, page.next_cursor)
            }
            (None, Target::Remote(remote)) => match remote.repo.events(branch, cursor) {
                Ok(page) => {
                    if unreachable {
                        unreachable = false;
                        eprintln!("by: reconnected to {}", remote.client.endpoint());
                    }
                    (page.events, page.cursor)
                }
                // A restarting server: keep the pane alive and try again.
                Err(error @ branchyard_client::Error::Transport { .. }) => {
                    if !unreachable {
                        unreachable = true;
                        eprintln!("by: {error}; retrying");
                    }
                    std::thread::sleep(FOLLOW_POLL * 2);
                    continue;
                }
                Err(error) => return Err(error.into()),
            },
            (None, Target::Local) => unreachable!("a local target opened its branch"),
        };
        let quiet = events.is_empty();
        cursor = next;
        held.extend(events);
        // Everything up to the last event that is not message text is
        // complete; trailing text waits for more unless the stream paused.
        let split = match quiet {
            true => held.len(),
            false => held
                .iter()
                .rposition(|e| !matches!(e.activity, Activity::Harness(Event::MessageDelta { .. })))
                .map_or(0, |i| i + 1),
        };
        let ready: Vec<RecordedEvent> = held.drain(..split).collect();
        if !ready.is_empty() {
            let text = match as_json {
                true => ready
                    .iter()
                    .map(|event| format!("{}\n", json::recorded(event)))
                    .collect(),
                false => render::log_text(&ready, env.style()),
            };
            print(&text)?;
        }
        // Locally the wait above already paused.
        if quiet && local.is_none() {
            std::thread::sleep(FOLLOW_POLL);
        }
    }
}

/// [`merge`] without removing, and locally without printing, for a command
/// whose stdout is JSON (`by compare --pick --json`, `by judge --pick
/// --json`). Remotely the server's merge line is still printed.
pub fn merge_quietly(target: &Target, branch: &str, into: Option<&str>) -> Outcome {
    if let Target::Remote(remote) = target {
        return remote::merge(remote, branch, into);
    }
    let yard = open()?;
    let target = match into {
        Some(target) => target.to_owned(),
        None => yard
            .current_branch()?
            .ok_or_else(|| Failure::Message("HEAD is detached; pass --into <branch>".into()))?,
    };
    yard.merge(branch, &target)?;
    Ok(())
}

pub fn merge(target: &Target, branch: &str, into: Option<&str>, remove: bool) -> Outcome {
    if let Target::Remote(remote) = target {
        remote::merge(remote, branch, into)?;
        return match remove {
            true => rm(target, branch, false),
            false => Ok(()),
        };
    }
    let yard = open()?;
    let target = match into {
        Some(target) => target.to_owned(),
        None => yard
            .current_branch()?
            .ok_or_else(|| Failure::Message("HEAD is detached; pass --into <branch>".into()))?,
    };
    let merged = yard.merge(branch, &target)?;
    print_merged(
        &merged.branch,
        &merged.target,
        &merged.previous,
        &merged.commit,
    )?;
    match remove {
        true => rm(&Target::Local, branch, false),
        false => Ok(()),
    }
}

pub fn print_merged(branch: &str, target: &str, previous: &str, commit: &str) -> Outcome {
    let short = |commit: &str| commit.get(..10).unwrap_or(commit).to_owned();
    print(&format!(
        "merged {branch} into {target} ({}..{})\n",
        short(previous),
        short(commit)
    ))
}

pub fn rm(target: &Target, branch: &str, keep_credentials: bool) -> Outcome {
    match target {
        Target::Local => {
            let teardown = open()?.remove_reporting(branch, &RemoveOptions { keep_credentials })?;
            if let Some(report) = teardown {
                // Best-effort: the branch is gone either way.
                eprintln!(
                    "by: {}",
                    render::workspace_line(&report, render::Style { color: false })
                );
            }
        }
        // The server decides what stays on its disk.
        Target::Remote(_) if keep_credentials => {
            return Err(Failure::Message(
                "--keep-credentials is for a local yard; a server always removes them".into(),
            ))
        }
        Target::Remote(remote) => remote.repo.remove(branch)?,
    }
    print(&format!("removed {branch}\n"))
}

/// Serve a branch's delegation tools on stdio; see `branchyard-mcp`.
pub fn mcp(root: &str, branch: &str) -> Outcome {
    branchyard_mcp::serve_branch(root.into(), branch).map_err(|e| Failure::Message(e.to_string()))
}

/// The delegate for this harness's branch when `by` runs inside a
/// delegating harness; `None` outside one.
pub(crate) fn harness_delegate(json: bool) -> Result<Option<Delegate>, Failure> {
    let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    if set(ENV_TOKEN) {
        if let Err(error) = crate::inherited::root() {
            return fail(json, &error).map(|()| None);
        }
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

#[allow(clippy::expect_used)] // ratchet: branchyard-cli
pub(crate) fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).expect("results serialize")
}

/// Report `error`: `{"error": {"kind", "message"}}` on stdout with
/// `--json`, else on stderr; exit 1 either way.
pub(crate) fn fail(json: bool, error: &branchyard::Error) -> Outcome {
    if json {
        let mut value =
            serde_json::json!({"error": {"kind": error.kind(), "message": error.to_string()}});
        if let Some(detail) = error.detail() {
            value["error"]["detail"] = detail;
        }
        print(&format!("{}\n", to_json(&value)))?;
        return Err(Failure::Reported);
    }
    eprintln!("by: {error}");
    Err(Failure::Reported)
}

/// Print a result as JSON or as text.
pub(crate) fn emit<T: Serialize>(
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

/// `--prompt-file PATH`: the prompt a file holds, `-` for standard input,
/// without its trailing newlines. An empty one is refused, as an empty
/// prompt is.
pub fn read_prompt_file(path: &str) -> Result<String, Failure> {
    let text = match path {
        "-" => {
            let mut text = String::new();
            io::Read::read_to_string(&mut io::stdin(), &mut text)
                .map_err(|e| Failure::Message(format!("--prompt-file -: {e}")))?;
            text
        }
        path => std::fs::read_to_string(path)
            .map_err(|e| Failure::Message(format!("--prompt-file {path}: {e}")))?,
    };
    let text = text.trim_end_matches(['\n', '\r']).to_owned();
    match text.trim().is_empty() {
        true => Err(Failure::Message(format!(
            "--prompt-file {path} holds no prompt"
        ))),
        false => Ok(text),
    }
}

pub fn spawn(env: &Env, target: &Target, prompt: &str, args: &SpawnArgs) -> Outcome {
    let json = args.json;
    // A child's issue link lives in its prompt's header: a harness has no
    // yard of its own to record it in.
    let (prompt, task, _) = crate::pr::issue_task(prompt, &args.task, None)?;
    let args = &SpawnArgs {
        task,
        ..args.clone()
    };
    let (prompt, task) = (prompt.as_str(), &args.task);
    let request = Spawn {
        prompt: prompt.to_owned(),
        harness: task.harness.clone(),
        name: task.name.clone(),
        base: task.base.clone(),
        budget: Budget {
            max_usd: task.budget_usd,
            max_turns: task.max_turns,
            max_duration: task.max_duration,
            stall_after: task.stall_after,
            stall_action: task.stall_action,
        },
        check: task.check.clone(),
        max_depth: args.max_depth,
        max_children: args.max_children,
        harnesses: args.harnesses.clone(),
        deny: args.deny.clone(),
        seat: args.seat.clone(),
        depends_on: args.depends_on.clone(),
        after: args.after,
        bindings: args.bindings.clone(),
        connectors: (!args.connectors.is_empty()).then(|| args.connectors.clone()),
        plan: args.plan,
        model: args.model.clone(),
    };
    if let Some(delegate) = harness_delegate(json)? {
        if args.parent.is_some()
            || task.permissions != args::Permissions::Unset
            || task.unapproved_tools
        {
            let error = branchyard::Error::Denied(
                "inside a harness, the parent is the harness's own branch and the child \
                 inherits its policy and approval routing; drop --parent, --yes, --ask, \
                 --permissions and --allow-unapproved-tools (by spawn --help says why)"
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
                let on = match &s.model {
                    Some(model) => format!(
                        "{} with model {model}",
                        render::harness_label(&s.harness, &s.profile)
                    ),
                    None => render::harness_label(&s.harness, &s.profile),
                };
                match &s.status {
                    branchyard::BranchStatus::Waiting => format!(
                        "created {} on {on}, waiting for {}\n",
                        s.name,
                        s.depends_on.join(", ")
                    ),
                    branchyard::BranchStatus::Blocked { reason } => {
                        format!("created {}, blocked: {reason}\n", s.name)
                    }
                    _ => format!(
                        "spawned {} on {on} from {}\n{}",
                        s.name,
                        short(&s.base),
                        check_note(s)
                    ),
                }
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
        let delegate = parent.delegate(live.options(task)?)?;
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

/// Run a branch's check on its current work as integrating it would:
/// inside a harness, on itself or a descendant; otherwise on any branch,
/// with yours. Exits 1 when the work would not get past its check.
pub fn check(env: &Env, target: &Target, branch: Option<String>, json: bool) -> Outcome {
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => {
            let branch = branch.unwrap_or_else(|| delegate.branch().to_owned());
            delegate.check(&branch)
        }
        (None, Target::Remote(_)) => Err(branchyard::Error::Unsupported(
            "the server has no check route; run by check on the repository's host".into(),
        )),
        (None, Target::Local) => required_outside(branch, "check")
            .and_then(|b| as_user(&b, TaskOptions::default())?.check(&b)),
    };
    let passed = result.as_ref().is_ok_and(branchyard::CheckReport::passed);
    emit(json, result, |r| render::check_report(r, env.style()))?;
    match passed {
        true => Ok(()),
        false => Err(Failure::Reported),
    }
}

/// Which check a spawned child must pass when it is integrated.
fn check_note(spawned: &branchyard::Spawned) -> String {
    match (&spawned.check, spawned.check_inherited) {
        (Some(check), true) => format!(
            "its check, inherited from its parent: {}; siblings that share it are integrated \
             together (by integrate a b), and a child that should land alone needs its own \
             (--check)\n",
            check.join(" ")
        ),
        (Some(check), false) => format!("its check: {}\n", check.join(" ")),
        (None, _) => String::new(),
    }
}

pub fn integrate(target: &Target, branches: &[String], json: bool) -> Outcome {
    let [branch] = branches else {
        return integrate_all(target, branches, json);
    };
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
            // One yard, so the wait sees a sibling this integration
            // started on this process's threads.
            let parent = yard.branch(&parent)?;
            let merged = parent.delegate(TaskOptions::default())?.integrate(branch)?;
            parent.wait_subtree()?;
            Ok(merged)
        })(),
    };
    emit(json, result, merged_line)
}

fn merged_line(m: &branchyard::Merged) -> String {
    match (&m.via, m.already) {
        (Some(via), true) => format!(
            "{} was already in {}, brought in by {via}; recorded as merged\n",
            m.branch, m.target
        ),
        _ => format!(
            "merged {} into {} ({}..{})\n",
            m.branch,
            m.target,
            short(&m.previous),
            short(&m.commit)
        ),
    }
}

/// `by integrate a b c`: several children of one parent, merged together,
/// checked once, all or none.
fn integrate_all(target: &Target, branches: &[String], json: bool) -> Outcome {
    let names: Vec<&str> = branches.iter().map(String::as_str).collect();
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => delegate.integrate_all(&names),
        (None, Target::Remote(remote)) => remote::integrate_all(remote, &names),
        (None, Target::Local) => (|| {
            let yard = open_yard()?;
            let mut parent = None;
            for branch in &names {
                let info = yard.branch(branch)?.info().clone();
                let found = info
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
                match &parent {
                    Some(p) if *p != found => {
                        return Err(branchyard::Error::Denied(format!(
                            "{branch} was delegated by {found}, not {p}; integrate together only \
                             children of one parent"
                        )))
                    }
                    _ => parent = Some(found),
                }
            }
            let parent = yard.branch(&parent.unwrap_or_default())?;
            let merged = parent
                .delegate(TaskOptions::default())?
                .integrate_all(&names)?;
            parent.wait_subtree()?;
            Ok(merged)
        })(),
    };
    emit(json, result, |all| {
        let mut text: String = all.branches.iter().map(merged_line).collect();
        if all.commit != all.previous {
            let checked = match all.check_results.is_empty() {
                true => "with no check: none of them has one".to_owned(),
                false => format!(
                    "after {} passed once on the result",
                    all.check_results
                        .iter()
                        .map(|c| format!("`{}` ({})", c.check.join(" "), c.branches.join(", ")))
                        .collect::<Vec<_>>()
                        .join(" and ")
                ),
            };
            text.push_str(&format!(
                "{} moved once: {}..{}, {checked}\n",
                all.target,
                short(&all.previous),
                short(&all.commit)
            ));
        }
        text
    })
}

/// `by wait [BRANCH...] [--any|--all] [--timeout S]`: block until
/// delegated branches settle, on the store's notifications.
pub fn wait(
    env: &Env,
    target: &Target,
    branches: &[String],
    any: bool,
    timeout: Option<f64>,
    json: bool,
) -> Outcome {
    let timeout = match timeout {
        None => None,
        Some(secs) if secs.is_finite() && secs >= 0.0 => {
            Some(std::time::Duration::try_from_secs_f64(secs).unwrap_or(std::time::Duration::MAX))
        }
        Some(secs) => {
            let error = branchyard::Error::Denied(format!(
                "--timeout takes a number of seconds, not {secs}"
            ));
            return fail(json, &error);
        }
    };
    let names: Vec<&str> = branches.iter().map(String::as_str).collect();
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => delegate.wait_for(&names, any, timeout),
        (None, _) if names.is_empty() => Err(branchyard::Error::Denied(
            "outside a harness, by wait needs the branches to wait for".into(),
        )),
        (None, Target::Remote(remote)) => remote
            .repo
            .wait_for(&names, any, timeout)
            .map_err(remote::sdk_error),
        (None, Target::Local) => open_yard().and_then(|yard| yard.wait_for(&names, any, timeout)),
    };
    let timed_out = result.as_ref().is_ok_and(|w| w.timed_out);
    emit(json, result, |w| {
        let mut text = String::new();
        for inspection in &w.settled {
            text.push_str(&render::inspection(inspection, env.style()));
        }
        if !w.pending.is_empty() {
            text.push_str(&format!(
                "{}: {}\n",
                match w.timed_out {
                    true => "timed out; still running",
                    false => "still running",
                },
                w.pending.join(", ")
            ));
        }
        text
    })?;
    match timed_out {
        true => Err(Failure::Reported),
        false => Ok(()),
    }
}

/// Inside a harness, cancel a descendant with the branch's authority;
/// otherwise cancel the branch and its descendants, locally or on the
/// server.
pub fn cancel(target: &Target, branch: &str, json: bool) -> Outcome {
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => delegate.cancel(branch),
        (None, Target::Remote(remote)) => {
            let cancelled = remote.repo.cancel(branch)?;
            let status = remote.repo.branch(branch)?.status;
            Ok(branchyard::Cancelled::of(cancelled, branch, &status))
        }
        (None, Target::Local) => open_yard().and_then(|yard| {
            let cancelled = yard.cancel_as(branch, "by cancel")?;
            let status = yard.branch(branch)?.info().status.clone();
            Ok(branchyard::Cancelled::of(cancelled, branch, &status))
        }),
    };
    emit(json, result, |c| match (&c.note, c.cancelled.is_empty()) {
        (Some(note), _) => format!("{note}\n"),
        (None, true) => "nothing was running\n".into(),
        (None, false) => format!("asked {} to stop\n", c.cancelled.join(", ")),
    })
}

/// Set a settled child aside: inside a harness, a descendant, with the
/// branch's authority; otherwise any branch, with yours.
pub fn discard(target: &Target, branch: &str, reason: Option<&str>, json: bool) -> Outcome {
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => delegate.discard(branch, reason),
        (None, Target::Remote(remote)) => remote
            .repo
            .discard(branch, reason)
            .map_err(remote::sdk_error),
        (None, Target::Local) => (|| {
            let reason = reason
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .unwrap_or("discarded with by discard");
            open_yard()?.discard(branch, reason)?;
            as_user(branch, TaskOptions::default())?.inspect(branch)
        })(),
    };
    // One line: a meta discards several children in a row, and `by
    // inspect` shows the rest.
    emit(json, result, |i| {
        let reason = match &i.status {
            branchyard::BranchStatus::Discarded { reason } => format!(": {reason}"),
            _ => String::new(),
        };
        format!(
            "discarded {}{reason}; its record, worktree and cost stay until `by rm {}`\n",
            i.name, i.name
        )
    })
}

/// Inside a harness, a command acts as the harness's own branch only: a
/// `--branch` or `--as` naming another is refused, not ignored.
fn as_itself(
    delegate: &Delegate,
    named: Option<&str>,
    flag: &str,
) -> Result<(), branchyard::Error> {
    match named {
        Some(other) if other != delegate.branch() => Err(branchyard::Error::Denied(format!(
            "inside a harness, by acts only as {}, the harness's own branch; {flag} {other} \
             names another",
            delegate.branch()
        ))),
        _ => Ok(()),
    }
}

/// The acting branch outside a harness: `--as`, required, since these
/// commands have no other way to name who is asking.
fn as_branch_required(
    as_branch: Option<String>,
    command: &str,
) -> Result<String, branchyard::Error> {
    as_branch.ok_or_else(|| {
        branchyard::Error::Denied(format!(
            "outside a harness, by {command} needs --as <branch>"
        ))
    })
}

pub fn ask(
    target: &Target,
    as_branch: Option<String>,
    text: &str,
    wait_seconds: Option<f64>,
    json: bool,
) -> Outcome {
    let wait = wait_seconds.map(std::time::Duration::from_secs_f64);
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => as_itself(&delegate, as_branch.as_deref(), "--as")
            .and_then(|()| delegate.ask(text, wait)),
        (None, Target::Remote(remote)) => as_branch_required(as_branch, "ask").and_then(|b| {
            remote
                .repo
                .ask(&b, text, wait_seconds)
                .map_err(remote::sdk_error)
        }),
        (None, Target::Local) => as_branch_required(as_branch, "ask")
            .and_then(|b| as_user(&b, TaskOptions::default())?.ask(text, wait)),
    };
    emit(json, result, |asked| match &asked.answer {
        Some(answer) => format!(
            "asked #{}: {}\nanswer: {}\n",
            asked.message.id, asked.message.text, answer.text
        ),
        None => format!(
            "asked #{}: {}\nno answer yet\n",
            asked.message.id, asked.message.text
        ),
    })
}

pub fn report(target: &Target, as_branch: Option<String>, text: &str, json: bool) -> Outcome {
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => {
            as_itself(&delegate, as_branch.as_deref(), "--as").and_then(|()| delegate.report(text))
        }
        (None, Target::Remote(remote)) => as_branch_required(as_branch, "report")
            .and_then(|b| remote.repo.report(&b, text).map_err(remote::sdk_error)),
        (None, Target::Local) => as_branch_required(as_branch, "report")
            .and_then(|b| as_user(&b, TaskOptions::default())?.report(text)),
    };
    emit(json, result, |m| {
        format!("reported #{}: {}\n", m.id, m.text)
    })
}

pub fn escalate(target: &Target, as_branch: Option<String>, text: &str, json: bool) -> Outcome {
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => as_itself(&delegate, as_branch.as_deref(), "--as")
            .and_then(|()| delegate.escalate(text)),
        (None, Target::Remote(remote)) => as_branch_required(as_branch, "escalate")
            .and_then(|b| remote.repo.escalate(&b, text).map_err(remote::sdk_error)),
        (None, Target::Local) => as_branch_required(as_branch, "escalate")
            .and_then(|b| as_user(&b, TaskOptions::default())?.escalate(text)),
    };
    emit(json, result, |m| {
        format!("escalated #{} to {}: {}\n", m.id, m.to, m.text)
    })
}

pub fn answer(
    target: &Target,
    as_branch: Option<String>,
    message_id: u64,
    text: &str,
    json: bool,
) -> Outcome {
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => as_itself(&delegate, as_branch.as_deref(), "--as")
            .and_then(|()| delegate.answer(message_id, text)),
        (None, Target::Remote(remote)) => as_branch_required(as_branch, "answer").and_then(|b| {
            remote
                .repo
                .answer(&b, message_id, text)
                .map_err(remote::sdk_error)
        }),
        (None, Target::Local) => as_branch_required(as_branch, "answer")
            .and_then(|b| as_user(&b, TaskOptions::default())?.answer(message_id, text)),
    };
    emit(json, result, |m| {
        format!("answered #{message_id} as #{}: {}\n", m.id, m.text)
    })
}

pub fn inbox(target: &Target, as_branch: Option<String>, unread: bool, json: bool) -> Outcome {
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => {
            as_itself(&delegate, as_branch.as_deref(), "--as").and_then(|()| delegate.inbox())
        }
        (None, Target::Remote(remote)) => as_branch_required(as_branch, "inbox")
            .and_then(|b| remote.repo.inbox(&b).map_err(remote::sdk_error)),
        (None, Target::Local) => as_branch_required(as_branch, "inbox")
            .and_then(|b| as_user(&b, TaskOptions::default())?.inbox()),
    };
    // --unread holds for the JSON too, which the Python module reads.
    let result = result.map(|inbox| match unread {
        true => inbox.unread_only(),
        false => inbox,
    });
    emit(json, result, |inbox| {
        let messages: Vec<&branchyard::Message> = inbox.messages.iter().collect();
        if messages.is_empty() {
            return "empty\n".to_owned();
        }
        messages
            .iter()
            .map(|m| {
                let reply = match m.in_reply_to {
                    Some(id) => format!(" (re #{id})"),
                    None => String::new(),
                };
                let read = if inbox.is_unread(m) { " [unread]" } else { "" };
                format!(
                    "#{} {} from {}{reply}{read}: {}\n",
                    m.id, m.kind, m.from, m.text
                )
            })
            .collect()
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
        false => render::branch_table(
            &c.descendants,
            branchyard_support::time::now_ms() / 1000,
            env.style(),
        ),
    })
}

/// `by graph show|apply|resume`; see `docs/graph.md`.
pub fn graph(env: &Env, target: &Target, args: &GraphArgs) -> Outcome {
    let json = args.json;
    let inside = harness_delegate(json)?;
    if inside.is_some() && (args.parent.is_some() || args.task.permissions != Permissions::Unset) {
        let error = branchyard::Error::Denied(
            "inside a harness, the graph is the harness's own branch's and its children inherit \
             its policy; drop --parent, --yes and --ask"
                .into(),
        );
        return fail(json, &error);
    }
    match args.action.as_str() {
        "show" => {
            let result = match (inside, target) {
                (Some(delegate), _) => {
                    let branch = args
                        .arg
                        .clone()
                        .unwrap_or_else(|| delegate.branch().to_owned());
                    delegate.graph(&branch)
                }
                (None, Target::Remote(remote)) => required_outside(args.arg.clone(), "graph show")
                    .and_then(|b| remote.repo.graph(&b).map_err(remote::sdk_error)),
                (None, Target::Local) => required_outside(args.arg.clone(), "graph show")
                    .and_then(|b| as_user(&b, TaskOptions::default())?.graph(&b)),
            };
            emit(json, result, |g| render::graph(g, env.style()))
        }
        "apply" => {
            let proposal = match proposal(args) {
                Ok(proposal) => proposal,
                Err(error) => return fail(json, &error),
            };
            let result = match (inside, target) {
                (Some(delegate), _) => {
                    delegate.apply_graph(proposal.edits, proposal.expected_revision)
                }
                (None, Target::Remote(remote)) => {
                    required_outside(args.parent.clone(), "graph apply --parent").and_then(
                        |parent| remote::apply_graph(remote, &parent, proposal, &args.task),
                    )
                }
                (None, Target::Local) => {
                    let parent = match required_outside(args.parent.clone(), "graph apply --parent")
                    {
                        Ok(parent) => parent,
                        Err(error) => return fail(json, &error),
                    };
                    // The children run on this process's threads, so the
                    // command waits for them, and for what they start.
                    let live = Live::start_to(env, &args.task, true, json, None);
                    let result = (|| {
                        let parent = open_yard()?.branch(&parent)?;
                        let delegate = parent.delegate(live.options(&args.task)?)?;
                        let applied =
                            delegate.apply_graph(proposal.edits, proposal.expected_revision)?;
                        parent.wait_subtree()?;
                        Ok(applied)
                    })();
                    live.console.finish();
                    result
                }
            };
            emit(json, result, |a| render::graph_applied(a, env.style()))
        }
        _ => {
            // resume
            let result = match (inside, target) {
                (Some(_), _) => Err(branchyard::Error::Denied(
                    "by graph resume is for a person, outside a harness".into(),
                )),
                (None, Target::Remote(_)) => Err(branchyard::Error::Unsupported(
                    "a server resumes its graphs itself, on its recovery interval".into(),
                )),
                (None, Target::Local) => {
                    let live = Live::start_to(env, &args.task, true, json, None);
                    let result = (|| {
                        let yard = open_yard()?;
                        let started = yard.resume_graph(&live.options(&args.task)?)?;
                        for name in &started {
                            if let Some(parent) = yard.branch(name)?.info().parent.clone() {
                                yard.branch(&parent)?.wait_subtree()?;
                            }
                        }
                        Ok(serde_json::json!({ "started": started }))
                    })();
                    live.console.finish();
                    result
                }
            };
            emit(json, result, |value| match value["started"].as_array() {
                Some(started) if !started.is_empty() => format!(
                    "started {}\n",
                    started
                        .iter()
                        .filter_map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                _ => "nothing was waiting on settled prerequisites\n".into(),
            })
        }
    }
}

/// The proposal `by graph apply` was given: a file (`-` for stdin) holding
/// `{"expected_revision", "edits"}`, or `--edits` with
/// `--expected-revision`, which also overrides a file's revision.
fn proposal(args: &GraphArgs) -> Result<branchyard::GraphProposal, branchyard::Error> {
    let invalid = |why: String| branchyard::Error::Denied(format!("invalid graph proposal: {why}"));
    let mut proposal = match (&args.edits, args.arg.as_deref()) {
        (Some(edits), _) => branchyard::GraphProposal {
            expected_revision: args.expected_revision.unwrap_or_default(),
            edits: serde_json::from_str(edits).map_err(|e| invalid(e.to_string()))?,
        },
        (None, Some(path)) => {
            let text = match path {
                "-" => {
                    let mut text = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
                    text
                }
                path => std::fs::read_to_string(path)?,
            };
            serde_json::from_str(&text).map_err(|e| invalid(e.to_string()))?
        }
        (None, None) => return Err(invalid("give a FILE or --edits".into())),
    };
    if let Some(revision) = args.expected_revision {
        proposal.expected_revision = revision;
    }
    Ok(proposal)
}

fn short(commit: &str) -> &str {
    commit.get(..10).unwrap_or(commit)
}

/// This command's acting branch: the harness's own, or `--branch` outside
/// one.
fn acting_branch(
    delegate: &Option<Delegate>,
    branch: &Option<String>,
    command: &str,
) -> Result<String, branchyard::Error> {
    match (delegate, branch) {
        (Some(delegate), named) => {
            as_itself(delegate, named.as_deref(), "--branch")?;
            Ok(delegate.branch().to_owned())
        }
        (None, Some(branch)) => Ok(branch.clone()),
        (None, None) => Err(branchyard::Error::Denied(format!(
            "outside a harness, by {command} needs --branch"
        ))),
    }
}

pub fn artifact(target: &Target, args: &ArtifactArgs) -> Outcome {
    let json = args.json;
    if let Target::Remote(remote) = target {
        return remote_artifact(remote, args);
    }
    let delegate = harness_delegate(json)?;
    let branch = match acting_branch(&delegate, &args.branch, "artifact") {
        Ok(branch) => branch,
        Err(error) => return fail(json, &error),
    };
    let act = match delegate {
        Some(delegate) => delegate,
        None => match as_user(&branch, TaskOptions::default()) {
            Ok(delegate) => delegate,
            Err(error) => return fail(json, &error),
        },
    };
    match args.action.as_str() {
        "publish" => {
            let path = absolute(given(args.arg.as_deref(), "arg")?);
            let labels = args.labels.iter().cloned().collect();
            let result =
                act.publish_artifact(&path, args.name.clone(), args.media_type.clone(), labels);
            emit(json, result, |a| {
                format!(
                    "published {} as {} ({} bytes, {}, blake3 {})\n",
                    a.name, a.id, a.size, a.media_type, a.digest
                )
            })
        }
        "list" => {
            let result = act.artifacts();
            emit(json, result, |list: &Vec<branchyard::ArtifactRef>| {
                if list.is_empty() {
                    return "no readable artifacts\n".into();
                }
                list.iter()
                    .map(|a| {
                        format!(
                            "{} {} {} ({} bytes)\n",
                            a.id, a.name, a.publisher_branch, a.size
                        )
                    })
                    .collect()
            })
        }
        "get" => {
            let id = given(args.arg.clone(), "arg")?;
            let out = absolute(given(args.out.as_deref(), "out")?);
            let result = act.read_artifact(&id, &out);
            emit(json, result, |a| {
                format!("wrote {} bytes of {} to {}\n", a.size, a.id, out.display())
            })
        }
        "share" => {
            let id = given(args.arg.clone(), "arg")?;
            let to = given(args.to.clone(), "to")?;
            let result = act.share_artifact(&id, &to).map(|()| Ack { ok: true });
            emit(json, result, |_| format!("shared {id} with {to}\n"))
        }
        "export" => {
            let out = absolute(given(args.out.as_deref(), "out")?);
            let result = act.export_artifacts(&args.ids, &out);
            emit(json, result, |entries: &Vec<branchyard::BundleEntry>| {
                format!(
                    "exported {} artifact{} to {}\n",
                    entries.len(),
                    if entries.len() == 1 { "" } else { "s" },
                    out.display()
                )
            })
        }
        "import" => {
            let path = absolute(given(args.arg.as_deref(), "arg")?);
            let result = act.import_artifacts(&path);
            emit(json, result, |imported: &Vec<branchyard::ArtifactRef>| {
                let mut out = format!(
                    "imported {} artifact{} from {}\n",
                    imported.len(),
                    if imported.len() == 1 { "" } else { "s" },
                    path.display()
                );
                for a in imported {
                    out.push_str(&format!("  {} {} ({} bytes)\n", a.id, a.name, a.size));
                }
                out
            })
        }
        other => unreachable!("artifact action {other} was validated in args"),
    }
}

/// A trivial success, printed as `{"ok": true}` with `--json` rather than
/// `null`: the Python module treats a `null` result the same as no output,
/// which is how a failed subprocess with nothing on stdout looks too.
#[derive(Serialize)]
struct Ack {
    ok: bool,
}

/// `--branch`, required in remote mode: there is no harness to delegate
/// as (a harness reaching the server acts through the delegation surfaces,
/// not `by --remote`), so a person must always be named explicitly.
fn remote_branch(
    json: bool,
    args_branch: &Option<String>,
    command: &str,
) -> Result<String, Outcome> {
    match args_branch {
        Some(branch) => Ok(branch.clone()),
        None => Err(fail(
            json,
            &branchyard::Error::Denied(format!(
                "--remote {command} needs --branch: there is no harness here to delegate as"
            )),
        )),
    }
}

/// `by --remote artifact …`: the same JSON as local mode, over
/// `branchyard_client`, acting with the server's authority as the named
/// `--branch`. A path here (`publish`'s file, `get`'s `--out`) is always
/// the caller's own local file: the bytes cross the wire, unlike a
/// `--substrate-key`-style path, which names a file on the server.
fn remote_artifact(server: &remote::Remote, args: &ArtifactArgs) -> Outcome {
    let json = args.json;
    let branch = match remote_branch(json, &args.branch, "artifact") {
        Ok(branch) => branch,
        Err(outcome) => return outcome,
    };
    match args.action.as_str() {
        "publish" => {
            let path = absolute(given(args.arg.as_deref(), "arg")?);
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    return fail(
                        json,
                        &branchyard::Error::State(format!("read {}: {error}", path.display())),
                    )
                }
            };
            let result = server
                .repo
                .publish_artifact(
                    &branch,
                    &bytes,
                    args.name.as_deref(),
                    args.media_type.as_deref(),
                    &args.labels,
                    &branchyard_client::new_key(),
                )
                .map_err(remote::sdk_error);
            emit(json, result, |a| {
                format!(
                    "published {} as {} ({} bytes, {})\n",
                    a.name, a.id, a.size, a.digest
                )
            })
        }
        "list" => {
            let result = server.repo.artifacts(&branch).map_err(remote::sdk_error);
            emit(json, result, |list: &Vec<branchyard::ArtifactRef>| {
                if list.is_empty() {
                    return "no readable artifacts\n".into();
                }
                list.iter()
                    .map(|a| {
                        format!(
                            "{} {} {} ({} bytes)\n",
                            a.id, a.name, a.publisher_branch, a.size
                        )
                    })
                    .collect()
            })
        }
        "get" => {
            let id = given(args.arg.clone(), "arg")?;
            let out = absolute(given(args.out.as_deref(), "out")?);
            let result = server
                .repo
                .read_artifact(&branch, &id)
                .map_err(remote::sdk_error)
                .and_then(|(a, bytes)| {
                    if let Some(dir) = out.parent() {
                        if !dir.as_os_str().is_empty() {
                            std::fs::create_dir_all(dir).map_err(|e| {
                                branchyard::Error::State(format!("create {}: {e}", dir.display()))
                            })?;
                        }
                    }
                    std::fs::write(&out, &bytes).map_err(|e| {
                        branchyard::Error::State(format!("write {}: {e}", out.display()))
                    })?;
                    Ok(a)
                });
            emit(json, result, |a| {
                format!("wrote {} bytes of {} to {}\n", a.size, a.id, out.display())
            })
        }
        "share" => {
            let id = given(args.arg.clone(), "arg")?;
            let to = given(args.to.clone(), "to")?;
            let result = server
                .repo
                .share_artifact(&branch, &id, &to, &branchyard_client::new_key())
                .map_err(remote::sdk_error)
                .map(|()| Ack { ok: true });
            emit(json, result, |_| format!("shared {id} with {to}\n"))
        }
        "export" | "import" => fail(
            json,
            &branchyard::Error::Unsupported(format!(
                "`by --remote artifact {action}` is not supported: the server has no bundle \
                 endpoint. Run `by artifact {action}` locally instead (see docs/storage.md \
                 \"Portable bundles\")",
                action = args.action
            )),
        ),
        other => unreachable!("artifact action {other} was validated in args"),
    }
}

pub fn scratch(target: &Target, args: &ScratchArgs) -> Outcome {
    let json = args.json;
    if let Target::Remote(remote) = target {
        return remote_scratch(remote, args);
    }
    let delegate = harness_delegate(json)?;
    let branch = match acting_branch(&delegate, &args.branch, "scratch") {
        Ok(branch) => branch,
        Err(error) => return fail(json, &error),
    };
    let act = match delegate {
        Some(delegate) => delegate,
        None => match as_user(&branch, TaskOptions::default()) {
            Ok(delegate) => delegate,
            Err(error) => return fail(json, &error),
        },
    };
    match args.action.as_str() {
        "create" => {
            let result = act.create_scratch(given(args.name.as_deref(), "name")?);
            emit(json, result, |a| {
                format!("created scratch area {}\n", a.name)
            })
        }
        "list" => {
            let result = act.scratch_areas();
            emit(json, result, |list: &Vec<branchyard::ScratchArea>| {
                if list.is_empty() {
                    return "no reachable scratch areas\n".into();
                }
                list.iter()
                    .map(|a| format!("{} {}\n", a.name, a.owner_branch))
                    .collect()
            })
        }
        "lock" => {
            let result = act.lock_scratch(given(args.name.as_deref(), "name")?);
            emit(json, result, |l| {
                format!("{} holds {}\n", l.holder_branch, l.name)
            })
        }
        "unlock" => {
            let result = act
                .unlock_scratch(given(args.name.as_deref(), "name")?)
                .map(|()| Ack { ok: true });
            emit(json, result, |_| "unlocked\n".to_owned())
        }
        "share" => {
            let name = given(args.name.clone(), "name")?;
            let to = given(args.to.clone(), "to")?;
            let result = act.share_scratch(&name, &to).map(|()| Ack { ok: true });
            emit(json, result, |_| format!("shared {name} with {to}\n"))
        }
        other => unreachable!("scratch action {other} was validated in args"),
    }
}

/// `by --remote scratch …`: the same JSON as local mode, acting with the
/// server's authority as the named `--branch`. The scratch directory
/// lives on the server host; a remote caller reaches it only through
/// `lock`/`unlock` (which fence honest callers going through the server,
/// same as local mode) and whatever harnesses the server runs, never by
/// reading or writing the directory itself.
fn remote_scratch(server: &remote::Remote, args: &ScratchArgs) -> Outcome {
    let json = args.json;
    let branch = match remote_branch(json, &args.branch, "scratch") {
        Ok(branch) => branch,
        Err(outcome) => return outcome,
    };
    match args.action.as_str() {
        "create" => {
            let name = given(args.name.as_deref(), "name")?;
            let result = server
                .repo
                .create_scratch(&branch, name, &branchyard_client::new_key())
                .map_err(remote::sdk_error);
            emit(json, result, |a| {
                format!("created scratch area {}\n", a.name)
            })
        }
        "list" => {
            let result = server
                .repo
                .scratch_areas(&branch)
                .map_err(remote::sdk_error);
            emit(json, result, |list: &Vec<branchyard::ScratchArea>| {
                if list.is_empty() {
                    return "no reachable scratch areas\n".into();
                }
                list.iter()
                    .map(|a| format!("{} {}\n", a.name, a.owner_branch))
                    .collect()
            })
        }
        "lock" => {
            let name = given(args.name.as_deref(), "name")?;
            let result = server
                .repo
                .lock_scratch(&branch, name, &branchyard_client::new_key())
                .map_err(remote::sdk_error);
            emit(json, result, |l| {
                format!("{} holds {}\n", l.holder_branch, l.name)
            })
        }
        "unlock" => {
            let name = given(args.name.as_deref(), "name")?;
            let result = server
                .repo
                .unlock_scratch(&branch, name, &branchyard_client::new_key())
                .map_err(remote::sdk_error)
                .map(|()| Ack { ok: true });
            emit(json, result, |_| "unlocked\n".to_owned())
        }
        "share" => {
            let name = given(args.name.clone(), "name")?;
            let to = given(args.to.clone(), "to")?;
            let result = server
                .repo
                .share_scratch(&branch, &name, &to, &branchyard_client::new_key())
                .map_err(remote::sdk_error)
                .map(|()| Ack { ok: true });
            emit(json, result, |_| format!("shared {name} with {to}\n"))
        }
        other => unreachable!("scratch action {other} was validated in args"),
    }
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
/// The provisioning flags, with `--instructions` read from its file.
pub(crate) fn provision(task: &TaskArgs) -> Result<Option<branchyard::Provisioning>, Failure> {
    let mut spec = task.provision.clone();
    if let Some(path) = &task.instructions {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Failure::Message(format!("--instructions {path}: {e}")))?;
        spec.get_or_insert_with(Default::default).instructions = Some(text);
    }
    Ok(spec)
}

/// The provider the flags name, if any. `--provider recipe:NAME` resolves
/// the recipe here and is refused unless it may run.
pub(crate) fn provider(task: &TaskArgs) -> Result<Option<Provider>, Failure> {
    if let Some(recipe) = &task.recipe {
        return crate::recipe_cmd::provider(recipe).map(|o| Some(Provider::Recipe(o)));
    }
    if let Some(substrate) = &task.substrate {
        return Ok(Some(Provider::Substrate(SubstrateOptions {
            endpoint: substrate.endpoint.clone(),
            router: substrate.router.clone(),
            atespace: substrate.atespace.clone().unwrap_or_default(),
            template: substrate.template.clone(),
            key: absolute(&substrate.key),
            workdir: substrate.workdir.clone().unwrap_or_default(),
            home: substrate.home.clone().unwrap_or_default(),
            pass_env: substrate.pass_env.clone(),
            ca: substrate.ca.as_deref().map(absolute),
            client_cert: substrate.client_cert.as_deref().map(absolute),
            client_key: substrate.client_key.as_deref().map(absolute),
            router_ca: substrate.router_ca.as_deref().map(absolute),
            insecure: substrate.insecure,
            keep: substrate.lifecycle.keep.unwrap_or_default(),
            snapshots: substrate.lifecycle.snapshots,
            max_paused: substrate.lifecycle.max_paused,
        })));
    }
    Ok(match (&task.sandbox, task.local) {
        (Some(sandbox), _) => Some(Provider::Microsandbox(SandboxOptions {
            image: sandbox.image.clone(),
            cpus: sandbox.cpus,
            memory_mib: sandbox.memory_mib,
            pass_env: sandbox.pass_env.clone(),
            keep: sandbox.lifecycle.keep.unwrap_or_default(),
            snapshots: sandbox.lifecycle.snapshots,
            max_paused: sandbox.lifecycle.max_paused,
            live_branch: sandbox.live_branch,
        })),
        (None, true) => Some(Provider::Local),
        (None, false) => None,
    })
}

/// `by rig check FILE` prints the plan; `by rig run FILE PROMPT` runs the
/// root seat with it, here or on the server, and waits for every seat it
/// spawned.
pub fn rig(env: &Env, target: &Target, args: &args::RigArgs) -> Outcome {
    let planned = rig::load(Path::new(&args.file)).and_then(|spec| rig::plan(&spec));
    let plan = match planned {
        Ok(plan) => plan,
        Err(error) => return rig_refused(args, &error),
    };
    let Some(prompt) = &args.prompt else {
        return match args.json {
            true => print(&format!("{}\n", to_json(&plan))),
            false => print(&rig::render(&plan)),
        };
    };
    if !plan.unapproved_tools.is_empty() && !args.unapproved_tools {
        let error = branchyard::Error::Unsupported(format!(
            "seats {} run profiles that do not route tool permission requests to Branchyard, so \
             their tools would run without the rig's policy; pass --allow-unapproved-tools to \
             run them anyway",
            plan.unapproved_tools.join(", ")
        ));
        return fail(args.json, &error);
    }
    let seats = match &plan.seats {
        Some(seats) => seats.delegates_to.join(", "),
        None => "none".into(),
    };
    eprintln!(
        "by: rig {}: root seat {} on {}; it may spawn seats {seats}",
        plan.rig, plan.root.seat, plan.root.harness
    );
    if let Target::Remote(remote) = target {
        return remote::rig(env, remote, &plan, prompt, args);
    }
    let root = &plan.root;
    let permissions = match root.policy.default {
        rig::Fallback::Allow => args::Permissions::Yes,
        rig::Fallback::Ask => args::Permissions::Ask,
        rig::Fallback::Deny => args::Permissions::Unset,
    };
    let task = TaskArgs {
        permissions,
        command: args.command.clone(),
        unapproved_tools: args.unapproved_tools,
        ..TaskArgs::default()
    };
    let workspace = workspace(env, &open()?)?;
    let live = Live::start_to(env, &task, true, args.json, None);
    let result = (|| {
        let options = TaskOptions {
            workspace,
            ..live.options(&task)?
        };
        let mut policy = match root.policy.default {
            rig::Fallback::Allow => Policy::allow_all(),
            rig::Fallback::Deny => Policy::deny_all(),
            rig::Fallback::Ask => live.policy.clone(),
        };
        for tool in &root.policy.deny {
            policy = policy.deny(tool.clone());
        }
        for tool in &root.policy.allow {
            policy = policy.allow(tool.clone());
        }
        if let (true, Some(by)) = (root.policy.delegation_commands, &options.delegation_cli) {
            policy = policy.allow_delegation_commands(by);
        }
        let options = TaskOptions {
            harness: Some(root.harness.clone()),
            name: Some(args.name.clone().unwrap_or_else(|| root.name.clone())),
            base: args.base.clone(),
            budget: Budget {
                max_usd: root.budget.max_usd,
                max_turns: root.budget.max_turns,
                max_duration: root
                    .budget
                    .max_minutes
                    .and_then(|m| std::time::Duration::try_from_secs_f64(m * 60.0).ok()),
                ..Budget::default()
            },
            policy,
            check: root.check.clone(),
            isolated: root.isolated,
            delegation: root.delegation.clone(),
            provision: Some(root.provision.clone()),
            seats: plan.seats.clone(),
            ..options
        };
        Ok::<_, Failure>(open()?.task(prompt.as_str()).options(options).run())
    })();
    let result = match result {
        Ok(result) => result,
        Err(failure) => {
            live.console.finish();
            return Err(failure);
        }
    };
    if !args.json {
        return live.finish(env, result);
    }
    let branch = match result {
        Ok(branch) => branch,
        Err(error) => {
            live.console.finish();
            return fail(true, &error);
        }
    };
    let descendants = wait_for_descendants(&[&branch]);
    live.console.finish();
    let descendants = descendants?.unwrap_or_default();
    // Read again: the root's record gained its children during its turn.
    let root = branch.yard().branch(&branch.info().name)?;
    print_rig_run(&plan.rig, root.info(), &descendants)
}

/// `by rig run --json`'s result: the root and every branch below it, the
/// same from a server.
pub fn print_rig_run(rig: &str, root: &BranchInfo, descendants: &[BranchInfo]) -> Outcome {
    let value = serde_json::json!({
        "rig": rig,
        "root": json::branch(root),
        "descendants": descendants.iter().map(json::branch).collect::<Vec<_>>(),
    });
    print(&json::text(&value))?;
    branch_outcome(root)
}

/// A spec `by rig` cannot honor: the field, its line and why; with
/// `--json`, `{"error": {"kind": "invalid_rig", "message", "field", "line",
/// "column"}}`.
fn rig_refused(args: &args::RigArgs, error: &rig::RigError) -> Outcome {
    let message = format!("{}: {error}", args.file);
    if args.json {
        let value = serde_json::json!({"error": {
            "kind": "invalid_rig",
            "message": message,
            "field": error.field,
            "line": error.line,
            "column": error.column,
        }});
        print(&json::text(&value))?;
        return Err(Failure::Reported);
    }
    eprintln!("by: {message}");
    Err(Failure::Reported)
}
