//! Delegate coding work to agent harnesses on git branches, and merge only
//! validated results.
//!
//! This is the public Branchyard SDK in local mode: the engine runs
//! in-process, harnesses run as local processes, and every branch is a git
//! worktree of your repository. State lives in `.branchyard/` at the
//! repository root. Local mode provides no isolation beyond your operating
//! system user; see `docs/design.md` §4.
//!
//! ```no_run
//! use branchyard::{Budget, Policy, Yard};
//!
//! let yard = Yard::open(".")?;
//! let branches = yard
//!     .task("Make the flaky parser test deterministic")
//!     .budget(Budget::usd(2.0))
//!     .policy(Policy::allow_all())
//!     .check(["cargo", "test"])
//!     .run_on(&["claude-code", "codex"])?;
//! for branch in &branches {
//!     println!("{}: {:?}", branch.info().name, branch.info().status);
//! }
//! yard.merge(&branches[0].info().name, "main")?;
//! # Ok::<(), branchyard::Error>(())
//! ```
//!
//! Vocabulary: a **task** is what you asked; a **branch** is one harness
//! working in its own worktree and session; a **fork** starts a new branch
//! from another branch's candidate and forks its conversation where the
//! harness supports it; a **candidate** is the exact commit a branch
//! proposes; a **merge** promotes a candidate only after checks pass
//! against the exact target revision.
//!
//! What the engine guarantees:
//!
//! - Every harness event, permission decision, candidate snapshot, status
//!   change and warning is appended to `.branchyard/events/<name>.jsonl`
//!   before the observer sees it. Every permission request reaches the
//!   [`Policy`]; nothing runs with a permission bypass.
//! - Branch records are written atomically, and branch names are reserved
//!   with an exclusive create, so parallel branches never share a name.
//! - A turn over budget is interrupted and waited for, never abandoned; the
//!   harness's process group is torn down when each call returns, and
//!   descendants that outlived it are named in the event log.
//! - A merge moves the target only by compare-and-swap from the revision
//!   read when the merge started, after the branch's check passed on the
//!   exact merge commit (`branchyard_workspace`).
//!
//! What it does not guarantee:
//!
//! - Isolation. By default a harness runs with your environment and your
//!   `HOME`, so it uses your own harness login and can read what you can;
//!   only nested-session markers are removed. [`TaskOptions::isolated`]
//!   gives it a scrubbed environment and a private home instead, which
//!   usually means it is not logged in.
//! - Coordination between processes beyond name reservation: two processes
//!   sending to the same branch at once race on its record.
//! - Resume and fork across working directories. Some harnesses keep
//!   sessions per directory (Claude Code keys them by project path), so a
//!   fork, which runs in a new worktree, may not find its parent's session.
//!   The engine reports that failure as the branch's status; it never
//!   substitutes a fresh session.
//! - Cost limits for harnesses that report no cumulative cost estimate.

mod engine;
mod git;
mod harness;
mod names;
mod ops;
mod policy;
mod record;
mod run;
mod state;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub use branchyard_harness::{
    Event, NativeSession, PermissionDecision, PermissionKey, PermissionRequest, TurnOutcome, Usage,
};
use branchyard_workspace::Repository;
use serde::{Deserialize, Serialize};

/// A repository with Branchyard state. Cheap to clone; clones share state.
#[derive(Clone, Debug)]
pub struct Yard {
    root: PathBuf,
    repo: Repository,
}

impl Yard {
    /// Open the git repository containing `path`, creating `.branchyard/`
    /// and excluding it from git through the repository's `info/exclude`.
    /// Never touches `.gitignore`.
    pub fn open(path: impl AsRef<Path>) -> Result<Yard, Error> {
        ops::open(path.as_ref())
    }

    /// Repository root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Start describing a task.
    pub fn task(&self, prompt: impl Into<String>) -> TaskBuilder {
        TaskBuilder {
            yard: self.clone(),
            prompt: prompt.into(),
            options: TaskOptions::default(),
        }
    }

    /// Every branch Branchyard knows, oldest first.
    pub fn branches(&self) -> Result<Vec<BranchInfo>, Error> {
        Ok(self
            .store()
            .list()?
            .into_iter()
            .map(|record| record.info)
            .collect())
    }

    /// Look up one branch by name.
    pub fn branch(&self, name: &str) -> Result<Branch, Error> {
        let record = self.store().read(name)?;
        Ok(Branch {
            yard: self.clone(),
            info: record.info,
        })
    }

    /// Merge `branch`'s candidate into the local branch `target`, running the
    /// branch's check. Refuses if `target` moved since the check started, if
    /// the check fails, or if the merge conflicts.
    pub fn merge(&self, branch: &str, target: &str) -> Result<Merged, Error> {
        ops::merge(self, branch, target)
    }

    /// Remove a branch's worktree and record; deletes the git branch unless
    /// it was merged.
    pub fn remove(&self, branch: &str) -> Result<(), Error> {
        ops::remove(self, branch)
    }

    /// Known harness profiles, whether their executable is on `PATH`, and
    /// their live qualification status.
    pub fn harnesses(&self) -> Vec<HarnessInfo> {
        harness::list()
    }

    fn store(&self) -> state::Store {
        state::Store::new(&self.root)
    }
}

/// Options shared by tasks, sends and forks.
#[derive(Clone, Default)]
pub struct TaskOptions {
    /// Harness ID (such as `claude-code`) or profile ID. Defaults to
    /// `claude-code`. A send keeps its branch's harness; a fork may name
    /// another only with a fresh session.
    pub harness: Option<String>,
    /// Branch name; defaults to a slug of the prompt, made unique.
    pub name: Option<String>,
    /// Base revision; defaults to `HEAD`.
    pub base: Option<String>,
    /// Limits for the branch. Cost and turns count across the branch's
    /// run and sends; the duration applies to each call.
    pub budget: Budget,
    pub policy: Policy,
    /// Check run in the merge worktree before promotion. Stored with the
    /// branch; a send or fork without one keeps the branch's.
    pub check: Option<Vec<String>>,
    pub observer: Option<Observer>,
    /// Run the harness with a scrubbed environment (no `ANTHROPIC*`,
    /// `CLAUDE*`, `OPENAI*` or `CODEX*` variables) and a private `HOME`
    /// under `.branchyard/homes/`. A harness then usually has no login.
    /// Off by default: local mode runs harnesses with your own login. A
    /// send keeps its branch's environment, since the session lives in it;
    /// a forked session shares its parent's private home.
    pub isolated: bool,
    /// Executable and fixed arguments replacing the profile's own, such as
    /// an absolute path to a harness installed elsewhere. The profile's
    /// driver still appends its protocol arguments. Stored with the branch
    /// for later sends and forks.
    pub command: Option<Vec<String>>,
}

/// Receives every activity as it is recorded, from any branch's thread.
pub type Observer = Arc<dyn Fn(&BranchEvent) + Send + Sync>;

/// Builds and runs a task.
pub struct TaskBuilder {
    yard: Yard,
    prompt: String,
    options: TaskOptions,
}

impl TaskBuilder {
    pub fn harness(mut self, harness: impl Into<String>) -> Self {
        self.options.harness = Some(harness.into());
        self
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.options.name = Some(name.into());
        self
    }

    pub fn base(mut self, rev: impl Into<String>) -> Self {
        self.options.base = Some(rev.into());
        self
    }

    pub fn budget(mut self, budget: Budget) -> Self {
        self.options.budget = budget;
        self
    }

    pub fn policy(mut self, policy: Policy) -> Self {
        self.options.policy = policy;
        self
    }

    pub fn check<I, S>(mut self, argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.options.check = Some(argv.into_iter().map(Into::into).collect());
        self
    }

    pub fn on_event(mut self, observer: impl Fn(&BranchEvent) + Send + Sync + 'static) -> Self {
        self.options.observer = Some(Arc::new(observer));
        self
    }

    /// See [`TaskOptions::isolated`].
    pub fn isolated(mut self, isolated: bool) -> Self {
        self.options.isolated = isolated;
        self
    }

    /// See [`TaskOptions::command`].
    pub fn command<I, S>(mut self, argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.options.command = Some(argv.into_iter().map(Into::into).collect());
        self
    }

    /// Replace every option at once.
    pub fn options(mut self, options: TaskOptions) -> Self {
        self.options = options;
        self
    }

    /// The branch names [`TaskBuilder::run`] (for an empty `harnesses`) or
    /// [`TaskBuilder::run_on`] would create now, in order. They agree unless
    /// another caller creates a branch in between.
    pub fn planned_names(&self, harnesses: &[&str]) -> Result<Vec<String>, Error> {
        run::planned_names(&self.yard, &self.prompt, &self.options, harnesses)
    }

    /// Create the branch, run the prompt as one turn, and snapshot the
    /// candidate. Returns once the turn ends; the branch's status says how.
    /// Errors mean nothing ran: an unknown or missing harness, an invalid
    /// name or base, or unwritable state.
    pub fn run(self) -> Result<Branch, Error> {
        run::run(&self.yard, &self.prompt, &self.options)
    }

    /// Run the same task on several harnesses in parallel, one branch each,
    /// named `<name>-<harness>`. Every harness is resolved and found before
    /// any branch is created. A failure on one branch does not stop the
    /// others; it is recorded in that branch's status.
    pub fn run_on(self, harnesses: &[&str]) -> Result<Vec<Branch>, Error> {
        run::run_on(&self.yard, &self.prompt, &self.options, harnesses)
    }
}

/// A branch: one harness session working in one worktree.
#[derive(Clone, Debug)]
pub struct Branch {
    yard: Yard,
    info: BranchInfo,
}

impl Branch {
    pub fn info(&self) -> &BranchInfo {
        &self.info
    }

    pub fn yard(&self) -> &Yard {
        &self.yard
    }

    /// Continue this branch's harness session with another prompt in a new
    /// harness process, then snapshot a new candidate.
    pub fn send(&self, prompt: &str, options: TaskOptions) -> Result<Branch, Error> {
        run::send(&self.yard, &self.info.name, prompt, &options)
    }

    /// A new branch from this branch's candidate. The harness session is
    /// forked when the harness supports it; otherwise this fails unless
    /// `fresh_session` is true, in which case the new branch starts a fresh
    /// session on the forked code.
    pub fn fork(
        &self,
        prompt: &str,
        fresh_session: bool,
        options: TaskOptions,
    ) -> Result<Branch, Error> {
        run::fork(&self.yard, &self.info.name, prompt, fresh_session, &options)
    }

    /// Unified diff of the candidate against the branch's base; empty when
    /// there is no candidate.
    pub fn diff(&self) -> Result<String, Error> {
        let info = self.yard.store().read(&self.info.name)?.info;
        match &info.candidate {
            None => Ok(String::new()),
            Some(candidate) => git::diff(&self.yard.root, &info.base, &candidate.commit),
        }
    }

    /// Recorded activity, oldest first.
    pub fn events(&self) -> Result<Vec<RecordedEvent>, Error> {
        record::read(&self.yard.store(), &self.info.name)
    }
}

/// A branch's durable record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BranchInfo {
    pub name: String,
    /// The git branch, `by/<name>`.
    pub git_branch: String,
    pub worktree: PathBuf,
    pub prompt: String,
    pub harness: String,
    pub profile: String,
    /// Native harness session, once known.
    pub session: Option<String>,
    /// Branch this one was forked from.
    pub parent: Option<String>,
    /// The commit the branch started from.
    pub base: String,
    pub candidate: Option<CandidateInfo>,
    pub status: BranchStatus,
    pub turns: u32,
    /// The harness's own cumulative cost estimate for this branch, when it
    /// reports one. A fork's excludes its parent's.
    pub cost_usd: Option<f64>,
    /// Seconds since the Unix epoch.
    pub created_at: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CandidateInfo {
    pub commit: String,
    pub files_changed: u32,
    pub insertions: u32,
    pub deletions: u32,
}

/// Serialized as an object tagged by `state`, such as
/// `{"state": "failed", "reason": "..."}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BranchStatus {
    Running,
    /// The last turn completed and produced a candidate.
    Ready,
    /// The last turn completed without changing any file.
    NoChanges,
    Interrupted,
    BudgetExceeded {
        limit: String,
    },
    Failed {
        reason: String,
    },
    Merged {
        target: String,
        commit: String,
    },
}

/// Limits enforced by the engine. The cost limit uses the harness's own
/// cumulative estimate and cannot apply to harnesses that report none.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Budget {
    pub max_usd: Option<f64>,
    pub max_turns: Option<u32>,
    pub max_duration: Option<Duration>,
}

impl Budget {
    pub fn usd(limit: f64) -> Self {
        Budget {
            max_usd: Some(limit),
            ..Budget::default()
        }
    }

    pub fn turns(mut self, limit: u32) -> Self {
        self.max_turns = Some(limit);
        self
    }

    pub fn duration(mut self, limit: Duration) -> Self {
        self.max_duration = Some(limit);
        self
    }
}

/// Answers permission requests, one invocation at a time. Never a bypass:
/// every request reaches the policy.
#[derive(Clone)]
pub struct Policy {
    rules: Vec<Rule>,
    fallback: Fallback,
}

/// Answers `(branch, request)` for requests no rule decides.
pub type Asker = Arc<dyn Fn(&str, &PermissionRequest) -> PermissionDecision + Send + Sync>;

#[derive(Clone)]
enum Fallback {
    Allow,
    Deny,
    Ask(Asker),
}

/// Decides requests whose tool name matches `tool` (exact, or `*` suffix
/// wildcard such as `mcp__*`).
#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub tool: String,
    pub allow: bool,
}

impl Default for Policy {
    /// Deny everything not allowed by a rule.
    fn default() -> Self {
        Policy::deny_all()
    }
}

impl Policy {
    pub fn allow_all() -> Self {
        Policy {
            rules: Vec::new(),
            fallback: Fallback::Allow,
        }
    }

    pub fn deny_all() -> Self {
        Policy {
            rules: Vec::new(),
            fallback: Fallback::Deny,
        }
    }

    /// Ask `ask(branch, request)` for anything no rule decides.
    pub fn ask(
        ask: impl Fn(&str, &PermissionRequest) -> PermissionDecision + Send + Sync + 'static,
    ) -> Self {
        Policy {
            rules: Vec::new(),
            fallback: Fallback::Ask(Arc::new(ask)),
        }
    }

    pub fn allow(mut self, tool: impl Into<String>) -> Self {
        self.rules.push(Rule {
            tool: tool.into(),
            allow: true,
        });
        self
    }

    pub fn deny(mut self, tool: impl Into<String>) -> Self {
        self.rules.push(Rule {
            tool: tool.into(),
            allow: false,
        });
        self
    }

    /// Decide one request: the first matching rule, else the fallback.
    pub fn decide(&self, branch: &str, request: &PermissionRequest) -> PermissionDecision {
        self.decide_with_source(branch, request).0
    }

    /// [`Policy::decide`], also saying what decided.
    pub fn decide_with_source(
        &self,
        branch: &str,
        request: &PermissionRequest,
    ) -> (PermissionDecision, DecisionSource) {
        policy::decide(self, branch, request)
    }
}

/// What answered a permission request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DecisionSource {
    /// The first rule whose pattern matched the tool.
    Rule { pattern: String },
    /// The policy's fallback: [`Policy::allow_all`] or [`Policy::deny_all`].
    Default,
    /// The [`Policy::ask`] callback.
    Asked,
    /// The engine, which could not deliver the policy's answer and
    /// interrupted the turn instead.
    Engine,
}

/// Something that happened on a branch, as recorded and observed.
///
/// Serialized with the variant as the key in snake case, such as
/// `{"harness": {"type": "ready"}}` or `{"warning": "..."}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    /// A normalized event from the harness driver.
    Harness(Event),
    /// A prompt submitted as a turn.
    Prompt(String),
    /// The answer to the permission request just before it.
    Decision {
        tool: String,
        allowed: bool,
        /// The denial message sent to the harness.
        message: Option<String>,
        source: DecisionSource,
    },
    /// A new candidate commit.
    Snapshot(CandidateInfo),
    /// The branch's status changed.
    Status(BranchStatus),
    /// Something the engine noticed, such as descendants that outlived the
    /// harness.
    Warning(String),
}

/// Activity from a named branch.
#[derive(Clone, Debug, PartialEq)]
pub struct BranchEvent {
    pub branch: String,
    pub activity: Activity,
}

/// Activity as recorded in `.branchyard/`, with its observation time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedEvent {
    /// Milliseconds since the Unix epoch.
    pub at_ms: u64,
    pub activity: Activity,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Merged {
    pub branch: String,
    pub target: String,
    pub previous: String,
    pub commit: String,
}

/// Known harness profile and local availability.
#[derive(Clone, Debug, PartialEq)]
pub struct HarnessInfo {
    pub harness: String,
    pub profile: String,
    pub default: bool,
    /// The executable was found on `PATH`.
    pub available: bool,
    /// Live qualification summary from `docs/qualification/`, such as
    /// "9/9 on 0.81.2".
    pub qualification: Option<String>,
}

#[derive(Debug)]
pub enum Error {
    /// Not inside a git work tree.
    NotARepository(PathBuf),
    UnknownBranch(String),
    BranchExists(String),
    /// Not a usable branch name: lowercase `[a-z0-9._-]`, starting with a
    /// letter or digit, one path segment.
    InvalidName {
        name: String,
        reason: String,
    },
    UnknownHarness(String),
    /// The harness executable could not be found or started.
    HarnessUnavailable {
        harness: String,
        reason: String,
    },
    /// The requested operation needs a capability the harness lacks.
    Unsupported(String),
    /// The branch has no candidate to merge or fork.
    NoCandidate(String),
    /// `actual` is `None` when the target branch no longer exists.
    TargetMoved {
        expected: String,
        actual: Option<String>,
    },
    Conflict {
        files: Vec<String>,
    },
    CheckFailed {
        output_tail: String,
    },
    CheckTimedOut {
        timeout: Duration,
        output_tail: String,
    },
    /// The check command could not be started.
    CheckNotStarted(String),
    /// A worktree with the target checked out has uncommitted changes.
    DirtyTarget(PathBuf),
    /// The candidate is already contained in the target.
    AlreadyMerged {
        target: String,
    },
    /// The recorded candidate is not a valid commit descending from its base.
    InvalidCandidate(String),
    Git(String),
    Harness(String),
    Io(std::io::Error),
    State(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotARepository(path) => {
                write!(f, "{} is not inside a git work tree", path.display())
            }
            Error::UnknownBranch(name) => write!(f, "no branch named {name}"),
            Error::BranchExists(name) => write!(f, "branch {name} already exists"),
            Error::InvalidName { name, reason } => {
                write!(f, "{name:?} is not a usable branch name: {reason}")
            }
            Error::UnknownHarness(name) => write!(f, "no harness or profile named {name}"),
            Error::HarnessUnavailable { harness, reason } => {
                write!(f, "{harness} is unavailable: {reason}")
            }
            Error::Unsupported(what) => write!(f, "unsupported: {what}"),
            Error::NoCandidate(name) => write!(f, "branch {name} has no candidate"),
            Error::TargetMoved {
                expected,
                actual: Some(actual),
            } => write!(
                f,
                "target moved from {expected} to {actual}; re-run the check"
            ),
            Error::TargetMoved {
                expected,
                actual: None,
            } => write!(f, "target at {expected} no longer exists"),
            Error::Conflict { files } => write!(f, "merge conflicts in {}", files.join(", ")),
            Error::CheckFailed { output_tail } => write!(f, "check failed:\n{output_tail}"),
            Error::CheckTimedOut {
                timeout,
                output_tail,
            } => write!(f, "check timed out after {timeout:?}:\n{output_tail}"),
            Error::CheckNotStarted(reason) => write!(f, "check could not start: {reason}"),
            Error::DirtyTarget(worktree) => write!(
                f,
                "the target is checked out with uncommitted changes in {}",
                worktree.display()
            ),
            Error::AlreadyMerged { target } => {
                write!(f, "the candidate is already contained in {target}")
            }
            Error::InvalidCandidate(message) => write!(f, "invalid candidate: {message}"),
            Error::Git(message) => write!(f, "git: {message}"),
            Error::Harness(message) => write!(f, "harness: {message}"),
            Error::Io(error) => write!(f, "{error}"),
            Error::State(message) => write!(f, "state: {message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::Io(error)
    }
}
