//! Pull-request work using the existing checked `by pr` implementation.
//!
//! The caller holds the ordinary durable branch-operation lock and supplies
//! a recheck of workspace, repository and scope authority. Neither a path nor
//! an executable can come from the browser request. Timeout or malformed
//! publication output is an uncertain effect, never permission to replay it.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use branchyard::{
    Activity, CheckRun, PullRequestActivity, PullRequestObservation, PullRequestRef, Yard,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};

const MAX_OUTPUT: usize = 2 * 1024 * 1024;

/// The authority rechecked before each service operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Read or refresh existing PR/check observations.
    Read,
    /// Push a checked candidate and create or update its PR.
    Publish,
}

type Authorizer = Arc<dyn Fn(Action) -> Result<(), Error> + Send + Sync>;

/// An internal branch binding and live authorization check.
///
/// A queued caller must hold the branch's normal operation lock. The callback
/// must re-read membership rather than capture an earlier boolean decision.
pub struct AuthorizedBranch {
    yard: Yard,
    branch: String,
    candidate: Option<String>,
    allowed: Action,
    authorize: Authorizer,
}

impl AuthorizedBranch {
    /// Compare server-resolved bindings without exposing their host paths.
    pub(crate) fn same_binding(&self, other: &Self) -> bool {
        self.branch == other.branch && self.yard.root() == other.yard.root()
    }

    /// Bind a server-resolved yard/branch after checking the required authority.
    pub fn checked(
        yard: Yard,
        branch: &str,
        allowed: Action,
        authorize: impl Fn(Action) -> Result<(), Error> + Send + Sync + 'static,
    ) -> Result<Self, Error> {
        authorize(allowed)?;
        let found = yard.branch(branch).map_err(Error::engine)?;
        let candidate = found
            .info()
            .candidate
            .as_ref()
            .map(|value| value.commit.clone());
        Ok(Self {
            yard,
            branch: branch.into(),
            candidate,
            allowed,
            authorize: Arc::new(authorize),
        })
    }

    fn recheck(&self, action: Action) -> Result<(), Error> {
        if action == Action::Publish && self.allowed != Action::Publish {
            return Err(Error::denied("this context permits PR observation only"));
        }
        (self.authorize)(action)?;
        let found = self.yard.branch(&self.branch).map_err(Error::engine)?;
        if action == Action::Publish {
            let current = found.info().candidate.as_ref().map(|value| &value.commit);
            if current != self.candidate.as_ref() {
                return Err(Error::new(
                    "candidate_changed",
                    "the candidate changed after PR admission",
                    false,
                ));
            }
            if current.is_none() {
                return Err(Error::new(
                    "no_candidate",
                    "this conversation has no code candidate to publish",
                    false,
                ));
            }
        }
        Ok(())
    }
}

/// Server-owned executable and wall deadline. No executable is accepted from a request.
#[derive(Clone, Debug)]
pub struct Service {
    executable: PathBuf,
    gh: PathBuf,
    timeout: Duration,
}

/// Safe publication choices; bypasses and force pushes are deliberately absent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Publish {
    /// Optional plain-text PR title.
    pub title: Option<String>,
    /// Optional base branch, interpreted by the existing CLI.
    pub base: Option<String>,
    /// Create a draft PR.
    #[serde(default)]
    pub draft: bool,
}

impl Publish {
    /// Validate publication choices before queue admission or process creation.
    pub fn validate(&self) -> Result<(), Error> {
        if let Some(title) = &self.title {
            bounded_text(title, "title", 500)?;
        }
        if let Some(base) = &self.base {
            bounded_text(base, "base", 256)?;
        }
        Ok(())
    }
}

/// Existing durable PR and CI observations, read from engine events.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    /// The task's branch name, distinct from its remote Git branch.
    pub branch: String,
    /// Current code candidate, if any.
    pub candidate: Option<String>,
    /// Last opened, updated or imported PR reference.
    pub pull_request: Option<PullRequestRef>,
    /// Most recent forge status, including CI and unresolved review count.
    pub observation: Option<PullRequestObservation>,
    /// Most recent candidate check, including its commit and outcome.
    pub check: Option<CheckRun>,
    /// Fresh forge comments, reviews and check details, populated by refresh.
    pub forge: Option<Value>,
}

/// A publication/refresh error with explicit effect uncertainty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// Stable machine-readable category.
    pub code: String,
    /// Safe human explanation; subprocess output and credentials are never included.
    pub message: String,
    /// Publication may have reached the forge; reconcile before another write.
    pub unknown_effect: bool,
}

impl Error {
    /// Construct a structured service error for an authorization callback.
    pub fn new(code: impl Into<String>, message: impl Into<String>, unknown_effect: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            unknown_effect,
        }
    }

    /// Deny access without starting a subprocess or revealing branch contents.
    pub fn denied(message: impl Into<String>) -> Self {
        Self::new("denied", message, false)
    }

    fn engine(error: branchyard::Error) -> Self {
        Self::new(error.kind(), error.to_string(), false)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl Service {
    /// Use an operator-selected `by` binary; ordinary publication has a five-minute deadline.
    pub fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            gh: PathBuf::from("gh"),
            timeout: Duration::from_secs(300),
        }
    }

    /// Select the operator-owned GitHub CLI executable used for review details.
    pub fn with_gh(mut self, executable: PathBuf) -> Self {
        self.gh = executable;
        self
    }

    /// Override the wall deadline, including for hermetic timeout checks.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Read durable state without contacting a forge or starting a process.
    pub fn inspect(&self, context: &AuthorizedBranch) -> Result<Snapshot, Error> {
        context.recheck(Action::Read)?;
        snapshot(context)
    }

    /// Refresh forge observations through the existing CLI, preserving its review/check behavior.
    pub async fn refresh(&self, context: &AuthorizedBranch) -> Result<Snapshot, Error> {
        context.recheck(Action::Read)?;
        self.command(
            context,
            &self.executable,
            vec![
                "show".into(),
                "--refresh".into(),
                "--json".into(),
                "--".into(),
                context.branch.clone(),
            ],
            false,
        )
        .await?;
        context.recheck(Action::Read)?;
        let mut state = snapshot(context)?;
        if let Some(pr) = &state.pull_request {
            state.forge = Some(
                self.command(
                    context,
                    &self.gh,
                    vec![
                        "pr".into(),
                        "view".into(),
                        pr.number.to_string(),
                        "--json".into(),
                        "comments,reviews,statusCheckRollup,reviewDecision,mergeStateStatus".into(),
                    ],
                    false,
                )
                .await?,
            );
        }
        context.recheck(Action::Read)?;
        Ok(state)
    }

    /// Check, push and create/update the candidate PR using `by pr --json`.
    ///
    /// The caller must not retry an error marked `unknown_effect` automatically.
    pub async fn publish(
        &self,
        context: &AuthorizedBranch,
        request: &Publish,
    ) -> Result<Value, Error> {
        context.recheck(Action::Publish)?;
        request.validate()?;
        let mut args = vec!["pr".into(), "--json".into()];
        if let Some(title) = &request.title {
            args.push(format!("--title={title}"));
        }
        if let Some(base) = &request.base {
            args.push(format!("--base={base}"));
        }
        if request.draft {
            args.push("--draft".into());
        }
        args.push("--".into());
        args.push(context.branch.clone());
        let result = self.command(context, &self.executable, args, true).await?;
        // Revocation during a forge call cannot undo its effects, but it must
        // prevent the result from being disclosed through this stale context.
        context.recheck(Action::Publish).map_err(|mut error| {
            error.unknown_effect = true;
            error
        })?;
        Ok(result)
    }

    async fn command(
        &self,
        context: &AuthorizedBranch,
        executable: &Path,
        args: Vec<String>,
        mutation: bool,
    ) -> Result<Value, Error> {
        let mut command = tokio::process::Command::new(executable);
        command
            .args(args)
            .current_dir(context.yard.root())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("BRANCHYARD_") {
                command.env_remove(name);
            }
        }
        command.env_remove("GIT_DIR").env_remove("GIT_WORK_TREE");
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|_| {
            Error::new(
                "pr_tool_unavailable",
                "the configured by executable could not start",
                false,
            )
        })?;
        let group = Group(child.id());
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::new("pr_output", "by stdout was not available", mutation))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::new("pr_output", "by stderr was not available", mutation))?;
        let result = tokio::time::timeout(self.timeout, async {
            tokio::try_join!(read_limited(stdout), read_limited(stderr), child.wait())
        })
        .await;
        let (stdout, _stderr, status) = match result {
            Ok(Ok(output)) => output,
            Ok(Err(_)) => {
                group.kill();
                branchyard_support::best_effort("stop PR command", child.start_kill());
                branchyard_support::best_effort(
                    "reap PR command after capture failure",
                    child.wait().await,
                );
                return Err(Error::new(
                    "pr_output_limit",
                    "PR command output could not be captured within its limit",
                    mutation,
                ));
            }
            Err(_) => {
                group.kill();
                branchyard_support::best_effort("stop PR command", child.start_kill());
                branchyard_support::best_effort("reap timed-out PR command", child.wait().await);
                return Err(Error::new(
                    "pr_timeout",
                    "PR command exceeded its wall deadline; inspect forge state before retrying",
                    mutation,
                ));
            }
        };
        if !status.success() {
            return Err(Error::new("pr_command_failed","the PR command failed; inspect the branch's checks and forge status before retrying",mutation));
        }
        serde_json::from_slice(&stdout).map_err(|_| {
            Error::new(
                "pr_protocol",
                "the PR command returned invalid JSON; inspect forge state before retrying",
                mutation,
            )
        })
    }
}

fn bounded_text(text: &str, label: &str, max: usize) -> Result<(), Error> {
    if text.is_empty() || text.len() > max || text.chars().any(char::is_control) {
        return Err(Error::new(
            "invalid_pr_request",
            format!("{label} must be nonempty, at most {max} bytes and contain no controls"),
            false,
        ));
    }
    Ok(())
}

fn snapshot(context: &AuthorizedBranch) -> Result<Snapshot, Error> {
    let branch = context
        .yard
        .branch(&context.branch)
        .map_err(Error::engine)?;
    let mut state = Snapshot {
        branch: context.branch.clone(),
        candidate: branch.info().candidate.as_ref().map(|c| c.commit.clone()),
        pull_request: None,
        observation: None,
        check: None,
        forge: None,
    };
    for event in branch.events().map_err(Error::engine)? {
        if let Activity::PullRequest(activity) = event.activity {
            match *activity {
                PullRequestActivity::Opened(pr)
                | PullRequestActivity::Updated(pr)
                | PullRequestActivity::Started(pr) => state.pull_request = Some(pr),
                PullRequestActivity::Observed(observation) => state.observation = Some(observation),
                PullRequestActivity::Checked(check) => state.check = Some(check),
                _ => {}
            }
        }
    }
    Ok(state)
}

async fn read_limited(reader: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_OUTPUT as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > MAX_OUTPUT {
        return Err(std::io::Error::other("PR output exceeded its limit"));
    }
    Ok(bytes)
}

struct Group(Option<u32>);

impl Group {
    fn kill(&self) {
        if let Some(pid) = self.0 {
            branchyard_support::kill_group(pid);
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
    }
}
