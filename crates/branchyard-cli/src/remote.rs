//! Remote mode: the same commands against a Branchyard server, through
//! `branchyard-client`. Output is rendered by the same code as local mode
//! from the same SDK values, so it matches line for line.
//!
//! A command that runs harnesses submits an operation, follows the
//! repository's event stream from the operation's cursor, and prints the
//! summary from the finished operation. Interrupting `by` stops only the
//! watching; the server finishes the work.

use std::collections::HashSet;
use std::io;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use branchyard::{
    Activity, BranchEvent, BranchInfo, Children, Envelope, EventPage, Inspection, Merged, Provider,
    Sent,
};
use branchyard_client::api::{
    BudgetSpec, ErrorBody, ForkRequest, GraphRequest, MapRequest, MergeRequest, Operation,
    OperationResult, OperationState, PolicyMode, PolicySpec, ReincarnateRequest, RuleSpec,
    SendRequest, SpawnRequest, TaskRequest,
};
use branchyard_client::{new_key, Client, Repo};

use crate::args::{Globals, Permissions, RigArgs, SpawnArgs, TaskArgs};
use crate::commands::{self, branch_outcome, print, Env, Failure, Outcome, Prompt};
use crate::console::Console;
use crate::render::{self, Renderer};
use crate::rig::{Fallback, RigPlan};

pub struct Remote {
    pub client: Client,
    pub repo: Repo,
    /// The global flags that reach this server and repository, for a `by`
    /// this one starts (as `by watch`'s actions do).
    pub args: Vec<String>,
}

impl Remote {
    /// Connect as `globals` say, choosing the server's only repository when
    /// none is named.
    pub fn connect(globals: &Globals) -> Result<Remote, Failure> {
        let url = globals
            .remote
            .as_deref()
            .ok_or_else(|| Failure::Message("no server URL".into()))?;
        // `ssh://`: a server this starts on the host, reached through a
        // forwarded Unix socket, with the token it fetched (docs/remote-ssh.md).
        let tunnel = match crate::ssh_remote::is_ssh(url) {
            true => {
                if globals.token_file.is_some() || globals.ca_file.is_some() {
                    return Err(Failure::Message(
                        "an ssh:// remote fetches its own token and needs no CA; drop                          --token-file and --ca-file (or BRANCHYARD_TOKEN_FILE and                          BRANCHYARD_CA_FILE)"
                            .into(),
                    ));
                }
                Some(crate::ssh_remote::connect(url)?)
            }
            false => None,
        };
        let tunnel_token = tunnel.as_ref().map(|t| t.token_file.display().to_string());
        let url = tunnel.as_ref().map_or(url, |t| t.url.as_str());
        let token_file = match &tunnel_token {
            Some(file) => file.as_str(),
            None => globals.token_file.as_deref().ok_or_else(|| {
                Failure::Message(
                    "remote mode needs a token: pass --token-file or set BRANCHYARD_TOKEN_FILE"
                        .into(),
                )
            })?,
        };
        let mut client = Client::from_token_file(url, token_file)?;
        if let Some(ca) = &globals.ca_file {
            client = client.with_ca_file(ca)?;
        }
        let name = match &globals.repo {
            Some(name) => name.clone(),
            None => {
                let repos = client.repos()?;
                match repos.as_slice() {
                    [only] => only.name.clone(),
                    [] => return Err(Failure::Message("the server serves no repositories".into())),
                    several => {
                        let names: Vec<&str> = several.iter().map(|r| r.name.as_str()).collect();
                        return Err(Failure::Message(format!(
                            "the server serves {}; pass --repo NAME or set BRANCHYARD_REPO",
                            names.join(", ")
                        )));
                    }
                }
            }
        };
        let repo = client.repo(&name);
        let mut args = vec![
            "--remote".to_owned(),
            url.to_owned(),
            "--token-file".to_owned(),
            token_file.to_owned(),
            "--repo".to_owned(),
            name,
        ];
        if let Some(ca) = &globals.ca_file {
            args.extend(["--ca-file".to_owned(), ca.clone()]);
        }
        Ok(Remote { client, repo, args })
    }

    /// Where commands run, for messages.
    pub fn label(&self) -> String {
        format!("{} ({})", self.client.endpoint(), self.repo.name())
    }
}

pub const REMOTE_DENY_NOTICE: &str = "remote mode cannot ask, so tool permission requests will \
     be denied; pass --yes to allow them all";

/// The provider `--provider` names, as the server will see it. Paths and
/// `--pass-env` names are the server's: a Substrate key is not made
/// absolute here, since a relative path would name a file on this machine.
fn provider(task: &TaskArgs) -> Result<Option<Provider>, Failure> {
    if let Some(recipe) = &task.recipe {
        return Err(Failure::Message(format!(
            "--provider recipe:{} runs on the machine that has the repository: a server does not \
             run environment recipes (docs/recipes.md). Run it without --remote",
            recipe.name
        )));
    }
    let mut provider = commands::provider(task)?;
    if let (Some(Provider::Substrate(options)), Some(args)) = (&mut provider, &task.substrate) {
        options.key = args.key.clone().into();
        if !options.key.is_absolute() {
            return Err(Failure::Message(format!(
                "--substrate-key names a file on the server in remote mode; give its absolute \
                 path there, not {}",
                args.key
            )));
        }
    }
    Ok(provider)
}

/// The policy to send, and the notice to print once the server accepts
/// the work. `--ask` needs a terminal where the harness runs.
fn permissions(task: &TaskArgs) -> Result<(PolicySpec, Option<&'static str>), Failure> {
    match task.permissions {
        Permissions::Yes => Ok((PolicySpec::allow_all(), None)),
        Permissions::Ask => Err(Failure::Message(
            "--ask is not available in remote mode, where the harness runs on the server; \
             pass --yes, or leave requests denied"
                .into(),
        )),
        Permissions::Preset(preset) => Ok((PolicySpec::preset(preset), None)),
        Permissions::Unset => Ok((PolicySpec::default(), Some(REMOTE_DENY_NOTICE))),
    }
}

fn budget(task: &TaskArgs) -> BudgetSpec {
    BudgetSpec {
        max_usd: task.budget_usd,
        max_turns: task.max_turns,
        max_seconds: task.max_duration.map(|d| d.as_secs_f64()),
        stall_after_seconds: task.stall_after.map(|d| d.as_secs_f64()),
        stall_action: task.stall_after.map(|_| task.stall_action),
    }
}

fn announce(remote: &Remote, notice: Option<&str>, provider: Option<&Provider>) {
    match provider {
        None | Some(Provider::Local) => eprintln!(
            "by: remote mode on {}: harnesses run as the server's user, with no isolation \
             beyond it",
            remote.label()
        ),
        Some(Provider::Microsandbox(sandbox)) => eprintln!(
            "by: remote mode on {}: harnesses run in Microsandbox microVMs on the server, from {}",
            remote.label(),
            sandbox.image
        ),
        Some(Provider::Substrate(options)) => eprintln!(
            "by: remote mode on {}: harnesses run in Agent Substrate actors from template {} \
             (unqualified), reached from the server",
            remote.label(),
            options.template
        ),
        // Refused before anything is sent (see `provider`).
        Some(Provider::Recipe(options)) => eprintln!(
            "by: remote mode on {}: recipe {} is not run by a server",
            remote.label(),
            options.name
        ),
    }
    if let Some(notice) = notice {
        eprintln!("by: {notice}");
    }
}

/// Activity on stdout, or on stderr with `--json` so stdout holds only the
/// result.
fn live_console(env: &Env, prefixed: bool, json: bool) -> Arc<Console> {
    let out: Box<dyn io::Write + Send> = match json {
        true => Box::new(io::stderr()),
        false => Box::new(io::stdout()),
    };
    Arc::new(
        Console::new(
            Renderer::new(render::Style { color: env.color }, prefixed),
            out,
            Box::new(|_| Err(io::Error::other("remote mode does not ask"))),
        )
        .with_notifier(env.notifier()),
    )
}

fn failed(error: ErrorBody) -> Failure {
    Failure::Remote(branchyard_client::Error::Api {
        status: 0,
        error: Box::new(error),
    })
}

/// The engine error a server error stands for, for `--json` output: its
/// kind is the one a local `by` reports.
pub fn sdk_error(error: branchyard_client::Error) -> branchyard::Error {
    match error {
        branchyard_client::Error::Api { error, .. } => {
            let kind = match error.code.as_str() {
                "git_error" => "git".to_owned(),
                "harness_error" => "harness".to_owned(),
                "io_error" => "io".to_owned(),
                "state_error" => "state".to_owned(),
                "remote_error" => error
                    .detail
                    .as_ref()
                    .and_then(|d| d["kind"].as_str())
                    .unwrap_or("remote")
                    .to_owned(),
                code => code.to_owned(),
            };
            // A failed check's siblings or the branches on whose own work
            // it failed, and every check of the integration, as a local
            // `by` reports them.
            let detail = error.detail.as_ref().and_then(|d| {
                let mut local = d
                    .get("shared")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                for key in ["checks", "own_work"] {
                    if let Some(value) = d.get(key) {
                        local[key] = value.clone();
                    }
                }
                local
                    .as_object()
                    .is_some_and(|l| !l.is_empty())
                    .then(|| Box::new(local))
            });
            branchyard::Error::Remote {
                kind,
                message: error.message,
                detail,
            }
        }
        other => branchyard::Error::Remote {
            kind: "unavailable".into(),
            message: other.to_string(),
            detail: None,
        },
    }
}

impl From<Failure> for branchyard::Error {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Sdk(error) => error,
            Failure::Remote(error) => sdk_error(error),
            other => branchyard::Error::Remote {
                kind: "unavailable".into(),
                message: other.to_string(),
                detail: None,
            },
        }
    }
}

/// The result of a finished operation, or its error.
fn result(op: Operation) -> Result<OperationResult, Failure> {
    match op.state {
        OperationState::Succeeded => Ok(op.result.unwrap_or_default()),
        _ => Err(failed(op.error.unwrap_or(ErrorBody {
            code: "unknown".into(),
            message: format!("operation {} ended {:?} without an error", op.id, op.state),
            detail: None,
        }))),
    }
}

/// Render the operation's activity as it streams, until the operation has
/// finished and every entry up to its end cursor has been shown.
fn follow(remote: &Remote, op: &Operation, console: &Console) -> Result<Operation, Failure> {
    follow_prefixed(remote, op, console, None)
}

/// [`follow`], also showing every branch whose name starts with `prefix`:
/// a map's, which its operation cannot name when it is accepted.
fn follow_prefixed(
    remote: &Remote,
    op: &Operation,
    console: &Console,
    prefix: Option<&str>,
) -> Result<Operation, Failure> {
    let mut branches: HashSet<String> = op.branches.iter().cloned().collect();
    let (tx, rx) = mpsc::channel();
    let stream = remote.repo.stream(Some(op.cursor));
    std::thread::spawn(move || {
        for item in stream {
            if tx.send(item).is_err() {
                return;
            }
        }
    });
    let mut seen = op.cursor;
    let mut finished: Option<Operation> = None;
    let mut streaming = true;
    let mut polled = Instant::now();
    let poll = Duration::from_millis(250);
    let mut said_waiting = false;
    loop {
        if let Some(done) = &finished {
            let drained = done.end_cursor.is_none_or(|end| seen >= end);
            if drained || !streaming {
                return Ok(done.clone());
            }
        }
        if streaming {
            match rx.recv_timeout(poll) {
                Ok(Ok(entry)) => {
                    seen = entry.seq;
                    // A child a followed branch spawned is followed too, as
                    // a local run shows its children.
                    if let Activity::Delegation {
                        tool,
                        branch,
                        refused: false,
                        ..
                    } = &entry.activity
                    {
                        if tool == "spawn" && branches.contains(&entry.branch) {
                            branches.insert(branch.clone());
                        }
                    }
                    let prefixed = prefix.is_some_and(|p| entry.branch.starts_with(p));
                    if prefixed || branches.contains(&entry.branch) {
                        console.event(&BranchEvent {
                            branch: entry.branch,
                            activity: entry.activity,
                        });
                    }
                }
                Ok(Err(error)) => {
                    console.finish();
                    eprintln!("by: lost the event stream ({error}); waiting for the result");
                    streaming = false;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => streaming = false,
            }
        } else {
            std::thread::sleep(poll);
        }
        if finished.is_none() && polled.elapsed() >= poll {
            polled = Instant::now();
            let op = remote.client.operation(&op.id)?;
            if let (Some(waiting), false) = (&op.waiting, said_waiting) {
                eprintln!("by: {} is still queued: {waiting}", op.id);
                said_waiting = true;
            }
            if op.state.is_terminal() {
                finished = Some(op);
            }
        }
    }
}

/// The provisioning flags for the server. `--instructions` is read here
/// and sent as text; a secret is sent by name only, because the server's
/// operator decides where each comes from.
fn provision(task: &TaskArgs) -> Result<Option<branchyard::Provisioning>, Failure> {
    let spec = commands::provision(task)?;
    if let Some(secret) = spec
        .iter()
        .flat_map(|s| &s.secrets)
        .find(|s| s.from.is_some())
    {
        return Err(Failure::Message(format!(
            "--secret {name}=...: with --remote, secrets come from the server, whose operator \
             decides where each is read; pass --secret {name}",
            name = secret.name
        )));
    }
    Ok(spec)
}

pub fn run(env: &Env, remote: &Remote, prompt: &str, task: &TaskArgs) -> Outcome {
    let (policy, notice) = permissions(task)?;
    let provider = provider(task)?;
    let request = TaskRequest {
        prompt: prompt.to_owned(),
        harness: task.harness.clone(),
        harnesses: Vec::new(),
        name: task.name.clone(),
        base: task.base.clone(),
        budget: budget(task),
        policy,
        check: task.check.clone(),
        isolated: task.isolated,
        command: task.command.clone(),
        delegation: task.delegate.map(Envelope::depth),
        allow_delegation: task.allow_delegation,
        unapproved_tools: task.unapproved_tools,
        provider: provider.clone(),
        provision: provision(task)?,
        seats: None,
        require_labels: task.require_labels.clone(),
        priority: task.priority,
        plan: task.plan,
        goal: goal_request(task),
        deny: task.deny.clone(),
    };
    let op = remote.repo.submit_task(&request, &new_key())?;
    announce(remote, notice, provider.as_ref());
    finish_one(env, remote, &op, task.delegate.is_some())
}

/// `--goal` and its options as the server takes them: the judge is one of
/// the server's harnesses, with the server's command.
fn goal_request(task: &TaskArgs) -> Option<branchyard_client::api::GoalRequest> {
    task.goal
        .as_ref()
        .map(|text| branchyard_client::api::GoalRequest {
            text: text.clone(),
            rounds: task.goal_rounds,
            judge: task.goal_judge.clone(),
        })
}

/// `by plan approve` on a server: an operation running the branch's next
/// turn with the plan, followed like a send.
pub fn plan_approve(
    env: &Env,
    remote: &Remote,
    branch: &str,
    edited: Option<&str>,
    task: &TaskArgs,
    json: bool,
) -> Outcome {
    let (send, notice) = send_request(task, Prompt::Text("plan approval"))?;
    let request = branchyard_client::knowledge_api::PlanApproveRequest {
        edited: edited.map(str::to_owned),
        send,
    };
    let op = remote.repo.approve_plan(branch, &request, &new_key())?;
    announce(remote, notice, None);
    plan_finish(env, remote, &op, json)
}

/// `by plan reject` on a server.
pub fn plan_reject(
    env: &Env,
    remote: &Remote,
    branch: &str,
    reason: Option<&str>,
    replan: bool,
    task: &TaskArgs,
    json: bool,
) -> Outcome {
    let (send, notice) = send_request(task, Prompt::Text("plan rejection"))?;
    let request = branchyard_client::knowledge_api::PlanRejectRequest {
        reason: reason.map(str::to_owned),
        replan,
        send,
    };
    let op = remote.repo.reject_plan(branch, &request, &new_key())?;
    announce(remote, notice, None);
    plan_finish(env, remote, &op, json)
}

fn plan_finish(env: &Env, remote: &Remote, op: &Operation, json: bool) -> Outcome {
    if !json {
        return finish_one(env, remote, op, false);
    }
    let sent = follow_result(env, remote, op).and_then(|result| {
        result
            .branches
            .into_iter()
            .next()
            .map(|info| Sent {
                name: info.name,
                status: info.status,
            })
            .ok_or_else(|| branchyard::Error::State("the server returned no branch".into()))
    });
    commands::emit(true, sent, |_| String::new())
}

/// Follow an operation that runs one branch, then print its summary and
/// the branches it delegated to.
fn finish_one(env: &Env, remote: &Remote, op: &Operation, prefixed: bool) -> Outcome {
    let console = live_console(env, prefixed, false);
    let done = follow(remote, op, &console);
    console.finish();
    let result = result(done?)?;
    let info = result
        .branches
        .into_iter()
        .next()
        .ok_or_else(|| Failure::Message("the server returned no branch".into()))?;
    let style = render::Style { color: env.color };
    print(&format!("\n{}", render::summary(&info, style)))?;
    if !result.descendants.is_empty() {
        let infos: Vec<&BranchInfo> = result.descendants.iter().collect();
        print(&format!(
            "\ndelegated\n{}",
            render::comparison_table(&infos, style)
        ))?;
    }
    branch_outcome(&info)
}

pub fn fan(
    env: &Env,
    remote: &Remote,
    prompt: &str,
    harnesses: &[String],
    task: &TaskArgs,
) -> Outcome {
    let (policy, notice) = permissions(task)?;
    let provider = provider(task)?;
    let request = TaskRequest {
        prompt: prompt.to_owned(),
        harness: None,
        harnesses: harnesses.to_vec(),
        name: task.name.clone(),
        base: task.base.clone(),
        budget: budget(task),
        policy,
        check: task.check.clone(),
        isolated: task.isolated,
        command: task.command.clone(),
        delegation: task.delegate.map(Envelope::depth),
        allow_delegation: task.allow_delegation,
        unapproved_tools: task.unapproved_tools,
        provider: provider.clone(),
        provision: provision(task)?,
        seats: None,
        require_labels: task.require_labels.clone(),
        priority: task.priority,
        plan: task.plan,
        goal: goal_request(task),
        deny: task.deny.clone(),
    };
    let op = remote.repo.submit_task(&request, &new_key())?;
    announce(remote, notice, provider.as_ref());
    let console = live_console(env, true, false);
    console.reserve(&op.branches);
    let done = follow(remote, &op, &console);
    console.finish();
    let result = result(done?)?;
    let infos: Vec<&BranchInfo> = result
        .branches
        .iter()
        .chain(result.descendants.iter())
        .collect();
    commands::fan_summary(env, &infos)
}

/// `by map --remote`: the items and schema are read here, the map runs as
/// one operation on the server, and `--out` and `--reduce-out` are
/// written here from its result.
pub fn map(env: &Env, remote: &Remote, args: &crate::args::MapArgs, json: bool) -> Outcome {
    let task: &TaskArgs = &args.task;
    if crate::fleet_cmd::is_routed(task) || task.kind.is_some() {
        return Err(crate::fleet_cmd::local_only());
    }
    let cwd = std::env::current_dir()?;
    let items = crate::map_cmd::read_items(args, &cwd)?;
    let schema = crate::map_cmd::read_schema(args)?;
    let (policy, notice) = permissions(task)?;
    let provider = provider(task)?;
    let prompt = args.prompt.clone().unwrap_or_default();
    let request = MapRequest {
        name: task.name.clone(),
        items,
        schema,
        concurrency: args.concurrency,
        retries: args.retries,
        total_usd: args.total_usd,
        reduce: args.reduce.clone(),
        remove_done: args.rm,
        retry_failed: args.retry_failed,
        task: TaskRequest {
            prompt: prompt.clone(),
            harness: task.harness.clone(),
            harnesses: Vec::new(),
            name: None,
            base: task.base.clone(),
            budget: budget(task),
            policy,
            check: task.check.clone(),
            isolated: task.isolated,
            command: task.command.clone(),
            delegation: None,
            allow_delegation: false,
            unapproved_tools: task.unapproved_tools,
            provider: provider.clone(),
            provision: provision(task)?,
            seats: None,
            require_labels: task.require_labels.clone(),
            priority: task.priority,
            plan: false,
            goal: None,
            deny: Vec::new(),
        },
    };
    let name = request
        .name
        .clone()
        .unwrap_or_else(|| branchyard::map_default_name(&prompt));
    let op = remote.repo.submit_map(&request, &new_key())?;
    announce(remote, notice, provider.as_ref());
    eprintln!(
        "by: map {name}: {} item(s), running on the server as {}",
        request.items.len(),
        op.id
    );
    map_finish(env, remote, &op, &name, args, json)
}

/// `by map resume --remote`: the server runs the map again with the
/// request that started it.
pub fn map_resume(
    env: &Env,
    remote: &Remote,
    name: &str,
    retry_failed: bool,
    json: bool,
) -> Outcome {
    let op = remote.repo.resume_map(
        name,
        &branchyard_client::api::MapResumeRequest { retry_failed },
        &new_key(),
    )?;
    eprintln!("by: map {name}: resumed on the server as {}", op.id);
    let none = crate::args::MapArgs {
        prompt: None,
        items: None,
        from_command: None,
        input_format: None,
        schema: None,
        out: None,
        concurrency: None,
        retries: None,
        total_usd: None,
        reduce: None,
        reduce_out: None,
        rm: false,
        retry_failed,
        task: crate::args::Checked::new(TaskArgs::default()),
    };
    map_finish(env, remote, &op, name, &none, json)
}

fn map_finish(
    env: &Env,
    remote: &Remote,
    op: &Operation,
    name: &str,
    args: &crate::args::MapArgs,
    json: bool,
) -> Outcome {
    let console = live_console(env, true, json);
    let done = follow_prefixed(remote, op, &console, Some(&format!("{name}-")));
    console.finish();
    let report = result(done?)?
        .map
        .ok_or_else(|| Failure::Message("the server returned no map".into()))?;
    let out = args.out.as_ref().map(std::path::PathBuf::from);
    if let Some(out) = &out {
        crate::map_cmd::write_atomic(out, &crate::map_cmd::results_text(out, &report))?;
    }
    if let (Some(path), Some(text)) = (
        &args.reduce_out,
        report.reduce.as_ref().and_then(|r| r.text.as_ref()),
    ) {
        crate::map_cmd::write_atomic(
            std::path::Path::new(path),
            &format!("{}\n", text.trim_end()),
        )?;
    }
    crate::map_cmd::finish(env, &report, out.as_deref(), json)
}

fn send_request(
    task: &TaskArgs,
    prompt: Prompt<'_>,
) -> Result<(SendRequest, Option<&'static str>), Failure> {
    let (policy, notice) = permissions(task)?;
    Ok((
        SendRequest {
            prompt: match prompt {
                Prompt::Text(text) => text.to_owned(),
                Prompt::Retry => String::new(),
            },
            retry: matches!(prompt, Prompt::Retry),
            budget: budget(task),
            policy,
            check: task.check.clone(),
            command: task.command.clone(),
            delegation: task.delegate.map(Envelope::depth),
            allow_delegation: task.allow_delegation,
            unapproved_tools: task.unapproved_tools,
            provision: provision(task)?,
            require_labels: task.require_labels.clone(),
            priority: task.priority,
        },
        notice,
    ))
}

/// `by --remote send`: `prompt`, or with `--retry` the cut-off turn's,
/// which the server reads.
pub fn send(
    env: &Env,
    remote: &Remote,
    branch: &str,
    prompt: Prompt<'_>,
    task: &TaskArgs,
) -> Outcome {
    let (request, notice) = send_request(task, prompt)?;
    let op = remote.repo.send(branch, &request, &new_key())?;
    announce(remote, notice, None);
    let delegating = task.delegate.is_some() || !remote.repo.branch(branch)?.children.is_empty();
    finish_one(env, remote, &op, delegating)
}

/// `send --json`: activity on stderr, then the branch's name and status,
/// as local mode prints them once the turn and its subtree have ended.
pub fn send_json(
    env: &Env,
    remote: &Remote,
    branch: &str,
    prompt: Prompt<'_>,
    task: &TaskArgs,
) -> Result<Sent, branchyard::Error> {
    let (request, notice) = send_request(task, prompt)?;
    let op = remote
        .repo
        .send(branch, &request, &new_key())
        .map_err(sdk_error)?;
    announce(remote, notice, None);
    let info = follow_result(env, remote, &op)?
        .branches
        .into_iter()
        .next()
        .ok_or_else(|| Failure::Message("the server returned no branch".into()))?;
    Ok(Sent {
        name: info.name,
        status: info.status,
    })
}

/// Follow an operation with activity on stderr, and return its result or
/// its error as the engine's.
fn follow_result(
    env: &Env,
    remote: &Remote,
    op: &Operation,
) -> Result<OperationResult, branchyard::Error> {
    let console = live_console(env, true, true);
    let done = follow(remote, op, &console);
    console.finish();
    Ok(result(done?)?)
}

pub fn fork(
    env: &Env,
    remote: &Remote,
    branch: &str,
    prompt: &str,
    fresh_session: bool,
    task: &TaskArgs,
) -> Outcome {
    let (policy, notice) = permissions(task)?;
    let provider = provider(task)?;
    let request = ForkRequest {
        prompt: prompt.to_owned(),
        name: task.name.clone(),
        fresh_session,
        harness: task.harness.clone(),
        budget: budget(task),
        policy,
        check: task.check.clone(),
        isolated: task.isolated,
        command: task.command.clone(),
        delegation: task.delegate.map(Envelope::depth),
        allow_delegation: task.allow_delegation,
        unapproved_tools: task.unapproved_tools,
        provider: provider.clone(),
        provision: provision(task)?,
        require_labels: task.require_labels.clone(),
        priority: task.priority,
    };
    let op = remote.repo.fork(branch, &request, &new_key())?;
    announce(remote, notice, provider.as_ref());
    finish_one(env, remote, &op, task.delegate.is_some())
}

pub fn reincarnate(env: &Env, remote: &Remote, branch: &str, task: &TaskArgs) -> Outcome {
    let (policy, notice) = permissions(task)?;
    let provider = provider(task)?;
    let request = ReincarnateRequest {
        name: task.name.clone(),
        harness: task.harness.clone(),
        budget: budget(task),
        policy,
        check: task.check.clone(),
        isolated: task.isolated,
        command: task.command.clone(),
        delegation: task.delegate.map(Envelope::depth),
        allow_delegation: task.allow_delegation,
        unapproved_tools: task.unapproved_tools,
        provider: provider.clone(),
        provision: provision(task)?,
        require_labels: task.require_labels.clone(),
        priority: task.priority,
    };
    let op = remote.repo.reincarnate(branch, &request, &new_key())?;
    announce(remote, notice, provider.as_ref());
    finish_one(env, remote, &op, task.delegate.is_some())
}

/// `spawn --parent` on the server: the child runs there, and this waits
/// for it as the local command does. Activity goes to stderr with
/// `--json`.
pub fn spawn(
    env: &Env,
    remote: &Remote,
    parent: &str,
    prompt: &str,
    args: &SpawnArgs,
) -> Result<Inspection, branchyard::Error> {
    let task = &args.task;
    let (policy, notice) = permissions(task)?;
    let request = SpawnRequest {
        prompt: prompt.to_owned(),
        harness: task.harness.clone(),
        name: task.name.clone(),
        base: task.base.clone(),
        budget: budget(task),
        policy,
        check: task.check.clone(),
        max_depth: args.max_depth,
        deny: args.deny.clone(),
        unapproved_tools: task.unapproved_tools,
        seat: args.seat.clone(),
        depends_on: args.depends_on.clone(),
        after: args.after,
        bindings: args.bindings.clone(),
        require_labels: task.require_labels.clone(),
        priority: task.priority,
        connectors: (!args.connectors.is_empty()).then(|| args.connectors.clone()),
        max_children: args.max_children,
        harnesses: args.harnesses.clone(),
        plan: args.plan,
        model: args.model.clone(),
    };
    let op = remote
        .repo
        .spawn(parent, &request, &new_key())
        .map_err(sdk_error)?;
    announce(remote, notice, None);
    let console = live_console(env, true, args.json);
    let done = follow(remote, &op, &console);
    console.finish();
    result(done?)?
        .inspection
        .ok_or_else(|| branchyard::Error::State("the server returned no inspection".into()))
}

/// `graph apply --parent` on the server: the proposal commits there, and
/// its children run there.
pub fn apply_graph(
    remote: &Remote,
    parent: &str,
    proposal: branchyard::GraphProposal,
    task: &TaskArgs,
) -> Result<branchyard::GraphApplied, branchyard::Error> {
    let (policy, notice) = permissions(task)?;
    let request = GraphRequest {
        expected_revision: proposal.expected_revision,
        edits: proposal.edits,
        policy,
        unapproved_tools: task.unapproved_tools,
    };
    let applied = remote
        .repo
        .apply_graph(parent, &request)
        .map_err(sdk_error)?;
    announce(remote, notice, None);
    Ok(applied)
}

/// `integrate` on the server: merge a delegated child into its parent.
pub fn integrate(remote: &Remote, branch: &str) -> Result<Merged, branchyard::Error> {
    let op = remote
        .repo
        .integrate(branch, &new_key())
        .map_err(sdk_error)?;
    let done = remote
        .client
        .wait(&op.id, Duration::from_millis(200))
        .map_err(sdk_error)?;
    result(done)?
        .merged
        .ok_or_else(|| branchyard::Error::State("the server returned no merge".into()))
}

/// `integrate a b c` on the server: several children of one parent,
/// together.
pub fn integrate_all(
    remote: &Remote,
    branches: &[&str],
) -> Result<branchyard::MergedAll, branchyard::Error> {
    let op = remote
        .repo
        .integrate_all(branches, &new_key())
        .map_err(sdk_error)?;
    let done = remote
        .client
        .wait(&op.id, Duration::from_millis(200))
        .map_err(sdk_error)?;
    result(done)?
        .merged_all
        .ok_or_else(|| branchyard::Error::State("the server returned no merge".into()))
}

pub fn inspect(remote: &Remote, branch: &str) -> Result<Inspection, branchyard::Error> {
    remote.repo.inspect(branch).map_err(sdk_error)
}

pub fn events(
    remote: &Remote,
    branch: &str,
    cursor: Option<usize>,
    limit: usize,
) -> Result<EventPage, branchyard::Error> {
    remote
        .repo
        .event_page(branch, cursor, limit)
        .map_err(sdk_error)
}

pub fn children(remote: &Remote, branch: &str) -> Result<Children, branchyard::Error> {
    remote.repo.children(branch).map_err(sdk_error)
}

pub fn merge(remote: &Remote, branch: &str, into: Option<&str>) -> Outcome {
    let request = MergeRequest {
        target: into.map(str::to_owned),
    };
    let op = match remote.repo.merge(branch, &request, &new_key()) {
        Err(error) if error.code() == Some("detached_head") => {
            return Err(Failure::Message(
                "HEAD is detached; pass --into <branch>".into(),
            ))
        }
        other => other?,
    };
    let done = remote.client.wait(&op.id, Duration::from_millis(200))?;
    let merged = result(done)?
        .merged
        .ok_or_else(|| Failure::Message("the server returned no merge".into()))?;
    commands::print_merged(
        &merged.branch,
        &merged.target,
        &merged.previous,
        &merged.commit,
    )
}

/// `by rig run` on the server: the lowered plan as one task request, with
/// its seats. Needs a server that allows delegation, since a rig's root
/// delegates.
pub fn rig(env: &Env, remote: &Remote, plan: &RigPlan, prompt: &str, args: &RigArgs) -> Outcome {
    let root = &plan.root;
    let (mode, notice) = match root.policy.default {
        Fallback::Allow => (PolicyMode::Allow, None),
        Fallback::Deny => (PolicyMode::Deny, Some(REMOTE_DENY_NOTICE)),
        Fallback::Ask => {
            let error = branchyard::Error::Unsupported(
                "policy.default = \"ask\" is not available in remote mode, where the harness \
                 runs on the server; use allow or deny"
                    .into(),
            );
            return rig_failed(args.json, error);
        }
    };
    let rules = root
        .policy
        .deny
        .iter()
        .map(|tool| RuleSpec {
            tool: tool.clone(),
            allow: false,
        })
        .chain(root.policy.allow.iter().map(|tool| RuleSpec {
            tool: tool.clone(),
            allow: true,
        }))
        .collect();
    let request = TaskRequest {
        prompt: prompt.to_owned(),
        harness: Some(root.harness.clone()),
        harnesses: Vec::new(),
        name: Some(args.name.clone().unwrap_or_else(|| root.name.clone())),
        base: args.base.clone(),
        budget: BudgetSpec {
            max_usd: root.budget.max_usd,
            max_turns: root.budget.max_turns,
            max_seconds: root.budget.max_minutes.map(|m| m * 60.0),
            ..BudgetSpec::default()
        },
        policy: PolicySpec {
            mode,
            rules,
            preset: None,
        },
        check: root.check.clone(),
        isolated: root.isolated,
        command: args.command.clone(),
        delegation: root.delegation.clone(),
        allow_delegation: root.policy.delegation_commands,
        unapproved_tools: args.unapproved_tools,
        provider: None,
        provision: Some(root.provision.clone()),
        seats: plan.seats.clone(),
        require_labels: Vec::new(),
        priority: None,
        plan: false,
        goal: None,
        deny: Vec::new(),
    };
    let op = match remote.repo.submit_task(&request, &new_key()) {
        Ok(op) => op,
        Err(error) if args.json => return rig_failed(true, sdk_error(error)),
        Err(error) => return Err(error.into()),
    };
    announce(remote, notice, None);
    if !args.json {
        return finish_one(env, remote, &op, true);
    }
    let result = match follow_result(env, remote, &op) {
        Ok(result) => result,
        Err(error) => return rig_failed(true, error),
    };
    let Some(info) = result.branches.first() else {
        return Err(Failure::Message("the server returned no branch".into()));
    };
    // Read again, as local mode does: the root gained its children.
    let root = remote.repo.branch(&info.name)?;
    commands::print_rig_run(&plan.rig, &root, &result.descendants)
}

fn rig_failed(json: bool, error: branchyard::Error) -> Outcome {
    if json {
        let value =
            serde_json::json!({"error": {"kind": error.kind(), "message": error.to_string()}});
        print(&crate::json::text(&value))?;
        return Err(Failure::Reported);
    }
    Err(Failure::Sdk(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server's failed integration check reads as a local one: the
    /// shared check's fields and every check of the integration, at the
    /// top of the detail.
    #[test]
    fn a_remote_check_failure_keeps_its_checks_and_siblings() {
        let checks = serde_json::json!([
            {"check": ["make", "test"], "branches": ["b"], "outcome": "failed"}
        ]);
        let error = sdk_error(branchyard_client::Error::Api {
            status: 422,
            error: Box::new(branchyard_client::api::ErrorBody {
                code: "check_failed".into(),
                message: "check failed:\nboom".into(),
                detail: Some(serde_json::json!({
                    "output_tail": "boom",
                    "checks": checks,
                    "shared": {"integrate_together": ["b", "e"]},
                })),
            }),
        });
        assert_eq!(error.kind(), "check_failed");
        let detail = error.detail().unwrap();
        assert_eq!(detail["checks"], checks);
        assert_eq!(detail["integrate_together"], serde_json::json!(["b", "e"]));
        let own = sdk_error(branchyard_client::Error::Api {
            status: 422,
            error: Box::new(branchyard_client::api::ErrorBody {
                code: "check_failed".into(),
                message: "check failed:\nboom".into(),
                detail: Some(serde_json::json!({ "output_tail": "boom", "own_work": ["b"] })),
            }),
        });
        assert_eq!(own.detail().unwrap()["own_work"], serde_json::json!(["b"]));
        let plain = sdk_error(branchyard_client::Error::Api {
            status: 422,
            error: Box::new(branchyard_client::api::ErrorBody {
                code: "check_failed".into(),
                message: "check failed:\nboom".into(),
                detail: Some(serde_json::json!({ "output_tail": "boom" })),
            }),
        });
        assert_eq!(plain.detail(), None);
        let not_started = sdk_error(branchyard_client::Error::Api {
            status: 422,
            error: Box::new(branchyard_client::api::ErrorBody {
                code: "check_not_started".into(),
                message: "check could not start: no such program".into(),
                detail: Some(serde_json::json!({ "checks": checks })),
            }),
        });
        assert_eq!(not_started.kind(), "check_not_started");
        assert_eq!(not_started.detail().unwrap()["checks"], checks);
    }
}
