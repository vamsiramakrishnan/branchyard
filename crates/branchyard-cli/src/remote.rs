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

use branchyard::{BranchEvent, BranchInfo};
use branchyard_client::api::{
    BudgetSpec, ErrorBody, ForkRequest, MergeRequest, Operation, OperationResult, OperationState,
    PolicySpec, SendRequest, TaskRequest,
};
use branchyard_client::{new_key, Client, Repo};

use crate::args::{Globals, Permissions, TaskArgs};
use crate::commands::{self, branch_outcome, print, Env, Failure, Outcome};
use crate::console::Console;
use crate::render::{self, Renderer};

pub struct Remote {
    pub client: Client,
    pub repo: Repo,
}

impl Remote {
    /// Connect as `globals` say, choosing the server's only repository when
    /// none is named.
    pub fn connect(globals: &Globals) -> Result<Remote, Failure> {
        let url = globals
            .remote
            .as_deref()
            .ok_or_else(|| Failure::Message("no server URL".into()))?;
        let token_file = globals.token_file.as_deref().ok_or_else(|| {
            Failure::Message(
                "remote mode needs a token: pass --token-file or set BRANCHYARD_TOKEN_FILE".into(),
            )
        })?;
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
        Ok(Remote { client, repo })
    }

    /// Where commands run, for messages.
    pub fn label(&self) -> String {
        format!("{} ({})", self.client.endpoint(), self.repo.name())
    }
}

pub const REMOTE_DENY_NOTICE: &str = "remote mode cannot ask, so tool permission requests will \
     be denied; pass --yes to allow them all";

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
        Permissions::Unset => Ok((PolicySpec::default(), Some(REMOTE_DENY_NOTICE))),
    }
}

fn budget(task: &TaskArgs) -> BudgetSpec {
    BudgetSpec {
        max_usd: task.budget_usd,
        max_turns: task.max_turns,
        max_seconds: task.max_duration.map(|d| d.as_secs_f64()),
    }
}

fn announce(remote: &Remote, notice: Option<&str>) {
    eprintln!(
        "by: remote mode on {}: harnesses run as the server's user, with no isolation beyond it",
        remote.label()
    );
    if let Some(notice) = notice {
        eprintln!("by: {notice}");
    }
}

fn live_console(env: &Env, prefixed: bool) -> Arc<Console> {
    Arc::new(Console::new(
        Renderer::new(render::Style { color: env.color }, prefixed),
        Box::new(io::stdout()),
        Box::new(|_| Err(io::Error::other("remote mode does not ask"))),
    ))
}

fn failed(error: ErrorBody) -> Failure {
    Failure::Remote(branchyard_client::Error::Api {
        status: 0,
        error: Box::new(error),
    })
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
    let branches: HashSet<String> = op.branches.iter().cloned().collect();
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
                    if branches.contains(&entry.branch) {
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
            if op.state.is_terminal() {
                finished = Some(op);
            }
        }
    }
}

pub fn run(env: &Env, remote: &Remote, prompt: &str, task: &TaskArgs) -> Outcome {
    let (policy, notice) = permissions(task)?;
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
    };
    let op = remote.repo.submit_task(&request, &new_key())?;
    announce(remote, notice);
    finish_one(env, remote, &op)
}

/// Follow an operation that runs one branch, then print its summary.
fn finish_one(env: &Env, remote: &Remote, op: &Operation) -> Outcome {
    let console = live_console(env, false);
    let done = follow(remote, op, &console);
    console.finish();
    let info = result(done?)?
        .branches
        .into_iter()
        .next()
        .ok_or_else(|| Failure::Message("the server returned no branch".into()))?;
    print(&format!(
        "\n{}",
        render::summary(&info, render::Style { color: env.color })
    ))?;
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
    };
    let op = remote.repo.submit_task(&request, &new_key())?;
    announce(remote, notice);
    let console = live_console(env, true);
    console.reserve(&op.branches);
    let done = follow(remote, &op, &console);
    console.finish();
    let infos: Vec<BranchInfo> = result(done?)?.branches;
    commands::fan_summary(env, &infos.iter().collect::<Vec<_>>())
}

pub fn send(env: &Env, remote: &Remote, branch: &str, prompt: &str, task: &TaskArgs) -> Outcome {
    let (policy, notice) = permissions(task)?;
    let request = SendRequest {
        prompt: prompt.to_owned(),
        budget: budget(task),
        policy,
        check: task.check.clone(),
        command: task.command.clone(),
    };
    let op = remote.repo.send(branch, &request, &new_key())?;
    announce(remote, notice);
    finish_one(env, remote, &op)
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
    };
    let op = remote.repo.fork(branch, &request, &new_key())?;
    announce(remote, notice);
    finish_one(env, remote, &op)
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
