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

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub use branchyard_harness::{Event, PermissionDecision, PermissionRequest, TurnOutcome, Usage};

/// A repository with Branchyard state. Cheap to clone; clones share state.
#[derive(Clone)]
pub struct Yard {
    root: PathBuf,
}

impl Yard {
    /// Open the git repository containing `path`, creating `.branchyard/`
    /// and excluding it from git through `.git/info/exclude`.
    pub fn open(path: impl AsRef<Path>) -> Result<Yard, Error> {
        let _ = path;
        todo!("engine")
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
        todo!("engine")
    }

    /// Look up one branch by name.
    pub fn branch(&self, name: &str) -> Result<Branch, Error> {
        let _ = name;
        todo!("engine")
    }

    /// Merge `branch`'s candidate into the local branch `target`, running the
    /// branch's check. Refuses if `target` moved since the check started, if
    /// the check fails, or if the merge conflicts.
    pub fn merge(&self, branch: &str, target: &str) -> Result<Merged, Error> {
        let _ = (branch, target);
        todo!("engine")
    }

    /// Remove a branch's worktree and record; deletes the git branch unless
    /// it was merged.
    pub fn remove(&self, branch: &str) -> Result<(), Error> {
        let _ = branch;
        todo!("engine")
    }

    /// Known harness profiles, whether their executable is on `PATH`, and
    /// their live qualification status.
    pub fn harnesses(&self) -> Vec<HarnessInfo> {
        todo!("engine")
    }
}

/// Options shared by tasks, sends and forks.
#[derive(Clone, Default)]
pub struct TaskOptions {
    /// Harness ID (such as `claude-code`) or profile ID. Defaults to `claude-code`.
    pub harness: Option<String>,
    /// Branch name; defaults to a slug of the prompt, made unique.
    pub name: Option<String>,
    /// Base revision; defaults to `HEAD`.
    pub base: Option<String>,
    pub budget: Budget,
    pub policy: Policy,
    /// Check run in the merge worktree before promotion.
    pub check: Option<Vec<String>>,
    pub observer: Option<Observer>,
}

/// Receives every event as it happens, from any branch's thread.
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

    /// Create the branch, run the prompt as one turn, and snapshot the
    /// candidate. Returns once the turn ends; the branch's status says how.
    pub fn run(self) -> Result<Branch, Error> {
        let _ = (&self.yard, &self.prompt, &self.options);
        todo!("engine")
    }

    /// Run the same task on several harnesses in parallel, one branch each,
    /// named `<name>-<harness>`. A failure on one branch does not stop the
    /// others; it is recorded in that branch's status.
    pub fn run_on(self, harnesses: &[&str]) -> Result<Vec<Branch>, Error> {
        let _ = harnesses;
        todo!("engine")
    }
}

/// A branch: one harness session working in one worktree.
#[derive(Clone)]
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

    /// Continue this branch's harness session with another prompt, then
    /// snapshot a new candidate.
    pub fn send(&self, prompt: &str, options: TaskOptions) -> Result<Branch, Error> {
        let _ = (prompt, options);
        todo!("engine")
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
        let _ = (prompt, fresh_session, options);
        todo!("engine")
    }

    /// Unified diff of the candidate against the branch's base.
    pub fn diff(&self) -> Result<String, Error> {
        todo!("engine")
    }

    /// Recorded events, oldest first.
    pub fn events(&self) -> Result<Vec<RecordedEvent>, Error> {
        todo!("engine")
    }
}

/// A branch's durable record.
#[derive(Clone, Debug, PartialEq)]
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
    pub base: String,
    pub candidate: Option<CandidateInfo>,
    pub status: BranchStatus,
    pub turns: u32,
    /// The harness's own cumulative cost estimate, when it reports one.
    pub cost_usd: Option<f64>,
    /// Seconds since the Unix epoch.
    pub created_at: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CandidateInfo {
    pub commit: String,
    pub files_changed: u32,
    pub insertions: u32,
    pub deletions: u32,
}

#[derive(Clone, Debug, PartialEq)]
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
/// estimate and cannot apply to harnesses that report none.
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
        let matches = |rule: &Rule| match rule.tool.strip_suffix('*') {
            Some(prefix) => request.tool.starts_with(prefix),
            None => request.tool == rule.tool,
        };
        let deny = || PermissionDecision::Deny {
            message: "Denied by Branchyard policy.".into(),
        };
        match self.rules.iter().find(|rule| matches(rule)) {
            Some(rule) if rule.allow => PermissionDecision::Allow,
            Some(_) => deny(),
            None => match &self.fallback {
                Fallback::Allow => PermissionDecision::Allow,
                Fallback::Deny => deny(),
                Fallback::Ask(ask) => ask(branch, request),
            },
        }
    }
}

/// An event from a named branch.
#[derive(Clone, Debug, PartialEq)]
pub struct BranchEvent {
    pub branch: String,
    pub event: Event,
}

/// An event as recorded in `.branchyard/`, with its observation time.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedEvent {
    /// Milliseconds since the Unix epoch.
    pub at_ms: u64,
    pub event: Event,
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
    /// Live qualification summary, such as "9/9 on Claude Code 2.1.283".
    pub qualification: Option<String>,
}

#[derive(Debug)]
pub enum Error {
    /// Not inside a git work tree.
    NotARepository(PathBuf),
    UnknownBranch(String),
    BranchExists(String),
    UnknownHarness(String),
    /// The harness executable could not be started.
    HarnessUnavailable {
        harness: String,
        reason: String,
    },
    /// The requested operation needs a capability the harness lacks.
    Unsupported(String),
    /// The branch has no candidate to merge or fork.
    NoCandidate(String),
    TargetMoved {
        expected: String,
        actual: String,
    },
    Conflict {
        files: Vec<String>,
    },
    CheckFailed {
        output_tail: String,
    },
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
            Error::UnknownHarness(name) => write!(f, "no harness or profile named {name}"),
            Error::HarnessUnavailable { harness, reason } => {
                write!(f, "{harness} is unavailable: {reason}")
            }
            Error::Unsupported(what) => write!(f, "unsupported: {what}"),
            Error::NoCandidate(name) => write!(f, "branch {name} has no candidate"),
            Error::TargetMoved { expected, actual } => {
                write!(
                    f,
                    "target moved from {expected} to {actual}; re-run the check"
                )
            }
            Error::Conflict { files } => write!(f, "merge conflicts in {}", files.join(", ")),
            Error::CheckFailed { output_tail } => write!(f, "check failed:\n{output_tail}"),
            Error::Git(message) => write!(f, "git: {message}"),
            Error::Harness(message) => write!(f, "harness: {message}"),
            Error::Io(error) => write!(f, "{error}"),
            Error::State(message) => write!(f, "state: {message}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::Io(error)
    }
}
