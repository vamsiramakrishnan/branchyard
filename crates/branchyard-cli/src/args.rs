//! Command-line parsing for `by`, with clap's derive API.
//!
//! [`Cli`] is the whole command line: [`Globals`], which choose where
//! commands run and may come before or after the command, and one
//! [`Command`]. `by --help`, `by help <command>`, typo suggestions, shell
//! completions (`by completions`) and the man page (`by man`) are all
//! generated from these types.
//!
//! # Adding a command
//!
//! 1. Add a variant to [`Command`]. Its doc comment is its one-line summary
//!    in `by --help`; `#[command(display_order = N)]` files it under the
//!    group [`GROUPS`] names for `N / 100` (without one it is listed under
//!    "Other commands"). Its fields are its positionals and flags, as in any
//!    clap derive: a `json: bool` with `#[arg(long)]` for `--json`, the
//!    flag groups below ([`Limits`], [`Perms`], ...) flattened where they
//!    fit, and a nested `#[command(subcommand)]` enum for actions.
//! 2. Add one arm for it to `dispatch` in `main.rs` (or to `run` there, when
//!    it needs no repository or server).
//!
//! For example, `by config` in the "Shell and setup" group, with its
//! actions as a nested subcommand:
//!
//! ```ignore
//! /// Show, locate or validate branchyard.toml and the user configuration
//! #[command(display_order = 603, subcommand_required = true)]
//! Config {
//!     /// Print JSON
//!     #[arg(long, global = true)]
//!     json: bool,
//!     #[command(subcommand)]
//!     action: ConfigAction,
//! },
//! // and in main.rs `run`, since it needs no repository or server:
//! Command::Config { json, action } => return config_cmd::main(&action, json),
//! ```
//!
//! Conflicts between flags go in clap attributes where clap can say them
//! (`conflicts_with`, `requires`, an `ArgGroup`, as [`InitFlags`] does).
//! Validation clap cannot express goes in a [`Flags`] impl, flattened into
//! the variant as [`Checked<F>`]: it then fails as a usage error (exit 2)
//! that names the command, before anything runs.

use std::ffi::OsString;
use std::fmt;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::time::Duration;

use clap::builder::styling::Style;
use clap::builder::{PossibleValue, StringValueParser, TypedValueParser};
use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{
    ArgGroup, ArgMatches, Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum,
    ValueHint,
};

/// How tool permission requests are answered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Permissions {
    /// `--yes`: allow every request.
    Yes,
    /// `--ask`: prompt on the terminal.
    Ask,
    /// `--permissions PRESET`: a named preset's rules.
    Preset(branchyard::PolicyPreset),
    /// Neither flag: decided by whether a terminal is attached.
    #[default]
    Unset,
}

/// Options shared by `run`, `fan`, `send` and `fork`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TaskArgs {
    pub harness: Option<String>,
    pub name: Option<String>,
    pub base: Option<String>,
    /// Check argument vector, split from one quoted string.
    pub check: Option<Vec<String>>,
    pub budget_usd: Option<f64>,
    pub max_turns: Option<u32>,
    /// From `--max-minutes`.
    pub max_duration: Option<Duration>,
    /// From `--stall-after`, in minutes.
    pub stall_after: Option<Duration>,
    /// From `--stall-action`; ignored without `--stall-after`.
    pub stall_action: branchyard::StallAction,
    pub permissions: Permissions,
    pub isolated: bool,
    /// `--isolated=false`: no private home for this run, whatever
    /// `[defaults] isolated` says.
    pub isolation_off: bool,
    /// Executable and fixed arguments replacing the profile's.
    pub command: Option<Vec<String>>,
    /// `--provider microsandbox` and its options; `None` keeps the default.
    pub sandbox: Option<SandboxArgs>,
    /// `--provider substrate` and its options.
    pub substrate: Option<SubstrateArgs>,
    /// `--provider recipe:NAME` and its options; the recipe is resolved
    /// and its trust checked when the command runs
    /// (`crate::recipe_cmd::provider`).
    pub recipe: Option<RecipeArgs>,
    /// `--provider local`.
    pub local: bool,
    /// From `--delegate[=DEPTH]`: levels of children the harness may create.
    pub delegate: Option<u32>,
    /// `--allow-delegation`: auto-allow the harness's own `by` delegation
    /// commands.
    pub allow_delegation: bool,
    /// `--no-wake`: the delegating branch is not woken when its children
    /// settle after its turn ended.
    pub no_wake: bool,
    /// `--allow-unapproved-tools`: run a profile whose tools Branchyard's
    /// policy never sees.
    pub unapproved_tools: bool,
    /// `by run --deny`: tools the harness is denied outright, stored with
    /// the branch.
    pub deny: Vec<String>,
    /// `--secret`, `--auth`, `--mcp`, `--model`, `--effort` and
    /// `--telemetry`; `None` when none was given.
    pub provision: Option<branchyard::Provisioning>,
    /// `--instructions FILE`, read when the command runs.
    pub instructions: Option<String>,
    /// `--issue URL|#N|N|linear:KEY|jira:KEY|gitlab:PATH#N`: the issue
    /// that is the task; see `crate::pr::issue_task`.
    pub issue: Option<String>,
    /// `--pr N`: start from GitHub pull request N's head; see
    /// `crate::pr::issue_task`.
    pub pr: Option<u64>,
    /// `--require-label`: worker labels the server's operation needs.
    pub require_labels: Vec<String>,
    /// `--priority`: the server's operation's priority, -10 to 10.
    pub priority: Option<i32>,
    /// `--auto`: route through the fleet table, failing over when a harness
    /// fails. See docs/fleet.md.
    pub auto: bool,
    /// Route because the configuration has a `[fleet]` and the command
    /// names no harness (filled by `crate::defaults`); failover is then the
    /// entry's.
    pub implied_auto: bool,
    /// `--kind`: the task's kind instead of the classifier's.
    pub kind: Option<branchyard::TaskKind>,
    /// `--seed`: a reproducible route.
    pub seed: Option<u64>,
    /// The configuration's `[fleet]`, when it has one (`crate::defaults`);
    /// else, for `--auto`, the inventory default (`crate::fleet_cmd`).
    pub fleet: Option<branchyard::Fleet>,
    /// Installed harnesses the inventory default left out, and why, when
    /// `fleet` is the inventory default.
    pub fleet_excluded: Vec<branchyard::Excluded>,
    /// `--plan`: plan first, read-only, and wait for `by plan approve`.
    pub plan: bool,
    /// `--goal`: a goal a judge verifies when the branch would be ready.
    pub goal: Option<String>,
    /// `--goal-rounds`: follow-up turns at most for an unmet goal.
    pub goal_rounds: Option<u32>,
    /// `--goal-judge`: the goal's judge harness.
    pub goal_judge: Option<String>,
    /// `--goal-judge-command`: launch the goal judge with this.
    pub goal_judge_command: Option<Vec<String>>,
    /// `--no-preflight`: do not check, before the branch is created, that
    /// the harness is installed and not logged out; see `crate::preflight`.
    pub no_preflight: bool,
}

const PLAN_EXAMPLES: &str = "\
Examples:
  by run \"migrate the config loader\" --plan
  by plan show migrate-the-config-loader
  by plan approve migrate-the-config-loader --edit
  by plan reject migrate-the-config-loader --reason \"keep the old flag\" --replan

See docs/plans-and-goals.md.";

const APPROVALS_EXAMPLES: &str = "\
Examples:
  by approvals
  by approvals allow 7Q2M9K4D
  by approvals deny 7Q2M9K4D --reason \"not before the board meets\"
  by approvals ls --all --json

See docs/effects.md.";

const EFFECTS_EXAMPLES: &str = "\
Examples:
  by effects
  by effects --branch board-update --json
  by effects show 5H3XK2PA
  by effects promote 5H3XK2PA
  by effects reconcile

See docs/effects.md.";

const UNDO_EXAMPLES: &str = "\
Examples:
  by undo board-update --to 3 --plan
  by undo board-update --to 3
  by undo board-update --to 3 --only 5H3XK2PA 9TQ0WZ1R
  by undo board-update --yes

See docs/effects.md.";

const KNOWLEDGE_EXAMPLES: &str = "\
Examples:
  by knowledge review
  by knowledge add \"Run cargo fmt before finishing\" --path \"crates/**\"
  by knowledge distill fix-parser
  by knowledge export --out AGENTS.md

See docs/knowledge.md.";

/// `by approvals`.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct ApprovalsArgs {
    /// Print JSON
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub action: Option<ApprovalsAction>,
}

/// `by effects`.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct EffectsArgs {
    /// Print JSON
    #[arg(long, global = true)]
    pub json: bool,
    /// Only this branch's
    #[arg(long, value_name = "BRANCH")]
    pub branch: Option<String>,
    #[command(subcommand)]
    pub action: Option<EffectsAction>,
}

/// `by undo`.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct UndoArgs {
    /// The branch to undo
    pub branch: String,
    /// Go back to this checkpoint (default: the branch's base, 0); effects of later turns are
    /// planned
    #[arg(long, value_name = "TURN")]
    pub to: Option<u32>,
    /// Print the plan and change nothing
    #[arg(long)]
    pub plan: bool,
    /// Undo only these upstream effects (ids or their ends)
    #[arg(long, value_name = "ID", num_args = 1..)]
    pub only: Vec<String>,
    /// Undo every reversible effect without a confirmation prompt
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Print JSON
    #[arg(long)]
    pub json: bool,
}

/// `by approvals`' actions.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum ApprovalsAction {
    /// List the approvals waiting (the default)
    Ls {
        /// Answered ones too
        #[arg(long)]
        all: bool,
    },
    /// Allow it: the waiting call or tool goes ahead, a staged effect is performed
    Allow {
        /// The approval's id, or the end of it
        #[arg(required_unless_present = "branch")]
        id: Option<String>,
        /// The oldest approval waiting on this branch instead
        #[arg(long, value_name = "BRANCH", conflicts_with = "id")]
        branch: Option<String>,
        /// Why, recorded with the answer
        #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
        reason: Option<String>,
    },
    /// Deny it: the call or tool is refused, a staged effect is discarded
    Deny {
        /// The approval's id, or the end of it
        #[arg(required_unless_present = "branch")]
        id: Option<String>,
        /// The oldest approval waiting on this branch instead
        #[arg(long, value_name = "BRANCH", conflicts_with = "id")]
        branch: Option<String>,
        /// Why, recorded with the answer
        #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
        reason: Option<String>,
    },
}

/// `by effects`' actions.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum EffectsAction {
    /// One entry and the events it is projected from
    Show {
        /// The entry's id, or the end of it
        id: String,
    },
    /// Perform a staged effect for real: promote its draft, or make the held call
    Promote {
        /// The entry's id, or the end of it
        id: String,
    },
    /// Settle entries whose outcome is unknown through their operation's lookup
    Reconcile,
}

/// `by plan`'s actions.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum PlanAction {
    /// Show a branch's plan, its task list and its phase
    Show {
        /// The branch whose plan to show
        branch: String,
    },
    /// Approve the plan and run it as the branch's next turn, with normal permissions
    Approve {
        /// The branch whose plan to approve
        branch: String,
        /// Edit the plan in your editor first; what you save is what is approved
        #[arg(long, conflicts_with = "file")]
        edit: bool,
        /// Approve this file's text instead of the proposed plan
        #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
        file: Option<String>,
        /// The editor for --edit: a known name or a command line that waits until the file is
        /// closed (default: $VISUAL, then $EDITOR)
        #[arg(long, value_name = "EDITOR", requires = "edit")]
        editor: Option<String>,
        #[command(flatten)]
        task: Checked<SendFlags>,
    },
    /// Reject the plan: the branch ends, or with --replan it plans again with your reason
    Reject {
        /// The branch whose plan to reject
        branch: String,
        /// Why; with --replan, the branch's next planning turn gets it
        #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
        reason: Option<String>,
        /// Plan again (read-only) instead of ending the branch
        #[arg(long)]
        replan: bool,
        #[command(flatten)]
        task: Checked<SendFlags>,
    },
}

/// `by knowledge`'s actions.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum KnowledgeAction {
    /// The repository's entries (default: proposed and adopted)
    List {
        /// Only entries with this status: proposed, adopted or rejected
        #[arg(long, value_name = "STATUS", value_parser = knowledge_status, conflicts_with = "all")]
        status: Option<branchyard::KnowledgeStatus>,
        /// Every entry, rejected ones too
        #[arg(long)]
        all: bool,
    },
    /// One entry
    Show {
        /// The entry's number
        id: u64,
    },
    /// Walk the proposed entries one at a time: adopt, reject, edit or skip each
    Review {
        /// The editor for edits (default: $VISUAL, then $EDITOR)
        #[arg(long, value_name = "EDITOR")]
        editor: Option<String>,
    },
    /// Adopt entries: from now on, matching branches are given them
    Adopt {
        #[arg(required = true, value_name = "ID")]
        /// Entry numbers, as `by knowledge review` lists them
        ids: Vec<u64>,
    },
    /// Reject entries: they are not used, and the same text is not proposed again
    Reject {
        #[arg(required = true, value_name = "ID")]
        /// Entry numbers, as `by knowledge review` lists them
        ids: Vec<u64>,
        /// Why
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
    },
    /// Change an entry's text (in your editor without --text) or scope
    Edit {
        /// The entry's number
        id: u64,
        /// The new text
        #[arg(long, value_name = "TEXT", value_parser = non_blank)]
        text: Option<String>,
        /// The path glob it applies to; \"\" for the whole repository
        #[arg(long, value_name = "GLOB")]
        path: Option<String>,
        /// The kind of task it applies to; \"\" for every kind
        #[arg(long, value_name = "KIND")]
        kind: Option<String>,
        /// The editor (default: $VISUAL, then $EDITOR)
        #[arg(long, value_name = "EDITOR", conflicts_with = "text")]
        editor: Option<String>,
    },
    /// Add an entry you wrote, adopted (or only proposed, with --propose)
    Add {
        #[arg(value_parser = non_blank)]
        /// The entry's text
        text: String,
        /// Only for tasks touching files matching this glob, such as crates/parser/**
        #[arg(long, value_name = "GLOB")]
        path: Option<String>,
        /// Only for tasks of this kind
        #[arg(long, value_name = "KIND", value_parser = task_kind)]
        kind: Option<branchyard::TaskKind>,
        /// Add it as proposed, for review, instead of adopted
        #[arg(long)]
        propose: bool,
    },
    /// Remove an entry
    Rm {
        /// The entry's number
        id: u64,
    },
    /// Propose entries from a branch now
    ///
    /// Propose entries from a branch now: the corrections sent into it and the review
    /// comments it addressed, or a distiller harness's proposals
    Distill {
        /// The branch to propose entries from
        branch: String,
        /// Ask this harness to distill, read-only on a scratch branch (default: [knowledge]
        /// distiller, else the deterministic extractor)
        #[arg(long, value_name = "ID", conflicts_with = "deterministic")]
        harness: Option<String>,
        /// Launch the distiller with this instead of its executable, for development and testing
        #[arg(long, value_name = "CMD", value_parser = command_argv, requires = "harness")]
        command: Option<Argv>,
        /// Use only the deterministic extractor, even when [knowledge] names a distiller
        #[arg(long)]
        deterministic: bool,
    },
    /// Adopted entries as an AGENTS.md-style Markdown file
    Export {
        /// Write it here instead of stdout
        #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
        out: Option<String>,
    },
}

fn knowledge_status(text: &str) -> Result<branchyard::KnowledgeStatus, String> {
    text.parse().map_err(|e: branchyard::Error| match e {
        branchyard::Error::Unsupported(why) => why,
        other => other.to_string(),
    })
}

/// `by fleet`'s actions.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum FleetAction {
    /// Recorded outcomes per kind and candidate: runs, merged, judged best, failed, cost, time
    Stats {
        /// Only this kind
        #[arg(long, value_name = "KIND", value_parser = task_kind)]
        kind: Option<branchyard::TaskKind>,
    },
    /// What the router would pick for a prompt, without running anything
    Route {
        /// The task
        prompt: String,
        /// The task's kind instead of the classifier's
        #[arg(long, value_name = "KIND", value_parser = task_kind)]
        kind: Option<branchyard::TaskKind>,
        /// Attempts, as for `by fan --auto`
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=16))]
        attempts: Option<u32>,
        /// Seed the router for a reproducible pick
        #[arg(long, value_name = "N")]
        seed: Option<u64>,
    },
    /// What to know before picking a harness for a task
    ///
    /// What to know before picking a harness for a task: the candidates with whether they
    /// can run here, their recorded outcomes for its kind and the router's own pick. Inside
    /// a harness, as its branch: its task's fleet entry, or the harnesses it may run, and
    /// its budget left
    Advise {
        /// The task
        prompt: String,
        /// The task's kind instead of the classifier's
        #[arg(long, value_name = "KIND", value_parser = task_kind)]
        kind: Option<branchyard::TaskKind>,
        /// Seed the router's pick for a reproducible advice
        #[arg(long, value_name = "N")]
        seed: Option<u64>,
    },
}

/// Options for `--provider microsandbox`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SandboxArgs {
    pub image: String,
    pub cpus: Option<u8>,
    pub memory_mib: Option<u32>,
    pub pass_env: Vec<String>,
    /// `--keep-sandbox`, `--sandbox-snapshots`, `--max-paused`,
    /// `--max-kept-minutes`.
    pub lifecycle: LifecycleArgs,
    /// `--live-branch`: declare the SDK's pause, live branch and full
    /// snapshots (unqualified).
    pub live_branch: bool,
}

/// What happens to a sandboxed branch's sandbox between turns; see
/// docs/sandbox-snapshots.md.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LifecycleArgs {
    pub keep: Option<branchyard::SandboxKeep>,
    pub snapshots: Option<u32>,
    pub max_paused: Option<u32>,
    pub max_kept_minutes: Option<u32>,
}

impl LifecycleArgs {
    /// `--max-kept-minutes` as the engine's seconds.
    pub fn max_kept_secs(&self) -> Option<u64> {
        self.max_kept_minutes.map(|m| u64::from(m) * 60)
    }
}

/// Options for `--provider substrate`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SubstrateArgs {
    pub endpoint: String,
    pub router: String,
    pub template: String,
    pub key: String,
    /// Control bearer token source, reread per RPC.
    pub api_token_file: Option<String>,
    /// TLS service name used to verify Control.
    pub server_name: Option<String>,
    /// Explicit actor network policy.
    pub egress: branchyard::config::SubstrateEgress,
    pub atespace: Option<String>,
    pub workdir: Option<String>,
    pub home: Option<String>,
    pub pass_env: Vec<String>,
    /// `--substrate-ca`: authorities for an `https://` Control API, and for
    /// the router unless `router_ca` is given.
    pub ca: Option<String>,
    /// `--substrate-client-cert` and `--substrate-client-key`: a client
    /// certificate for the Control API (mutual TLS).
    pub client_cert: Option<String>,
    pub client_key: Option<String>,
    /// `--substrate-router-ca`: authorities for an `https://` or `wss://`
    /// router.
    pub router_ca: Option<String>,
    /// `--substrate-insecure`: allow `http://` or `ws://` to hosts other
    /// than loopback.
    pub insecure: bool,
    /// `--keep-sandbox`, `--sandbox-snapshots`, `--max-paused`,
    /// `--max-kept-minutes`.
    pub lifecycle: LifecycleArgs,
}

/// Options for `--provider recipe:NAME`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RecipeArgs {
    /// The recipe's name, `[recipes.NAME]`.
    pub name: String,
    /// `--recipe-workdir`: where the worktree goes on the machine.
    pub workdir: Option<String>,
    /// `--recipe-home`: the harness's `HOME` on the machine.
    pub home: Option<String>,
    pub pass_env: Vec<String>,
    /// `--keep-sandbox` and `--max-paused`.
    pub lifecycle: LifecycleArgs,
}

/// Options of `by spawn`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpawnArgs {
    /// Harness, name, base, check, limits and permissions for the child.
    pub task: TaskArgs,
    /// The delegating branch, outside a harness.
    pub parent: Option<String>,
    pub wait: bool,
    pub max_depth: Option<u32>,
    /// `--max-children`: at most the parent's.
    pub max_children: Option<u32>,
    /// `--harnesses`: what the child may delegate to.
    pub harnesses: Option<Vec<String>>,
    pub deny: Vec<String>,
    /// `--seat`: the rig seat the child fills.
    pub seat: Option<String>,
    /// `--depends-on`: siblings the child waits for.
    pub depends_on: Vec<String>,
    /// `--after`: when each of them counts as done.
    pub after: branchyard::After,
    /// `--bind NAME:ACCESS`, repeatable.
    pub bindings: Vec<branchyard::Binding>,
    /// `--connector`, repeatable: the child's grant, narrowed to its
    /// parent's. Empty: its seat's or its parent's.
    pub connectors: Vec<branchyard::connectors::GrantEntry>,
    /// `--plan`: the child plans first, read-only.
    pub plan: bool,
    /// `--model`: the child's model. Unset: its seat's or its parent's.
    pub model: Option<String>,
    /// `--why`: the parent's reason for the harness and model, recorded on
    /// the child.
    pub why: Option<String>,
    /// `--result-schema`: the child's result contract's schema, as JSON
    /// text.
    pub result_schema: Option<String>,
    /// `--result-schema-file`: the same, in a file (`-` for standard
    /// input).
    pub result_schema_file: Option<String>,
    /// `--result-required`: the child must report a result.
    pub result_required: bool,
    /// `--owns`, repeatable: the paths the child owns. Empty: none of its
    /// own, bounded only by the scope its parent works under.
    pub owns: Vec<String>,
    /// `--land`: leave the child to the engine to land.
    pub land: bool,
    /// `--review` and its `--review-*` flags: the child's reviewer before
    /// it lands. Boxed, so `Command` stays small.
    pub review: Option<Box<branchyard::ReviewSpec>>,
    pub json: bool,
}

/// `by graph show|apply|resume ...`; see `docs/graph.md`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphArgs {
    pub action: String,
    /// `show`'s branch, or `apply`'s proposal file (`-` for stdin).
    pub arg: Option<String>,
    /// The branch whose graph `apply` changes, outside a harness.
    pub parent: Option<String>,
    /// `--edits JSON`: the edits inline, instead of a file.
    pub edits: Option<String>,
    /// `--expected-revision N`, overriding a file's.
    pub expected_revision: Option<u64>,
    /// Permissions for the children's turns, outside a harness.
    pub task: TaskArgs,
    pub json: bool,
}

/// `by rig check FILE` or `by rig run FILE PROMPT`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RigArgs {
    /// `run` with its prompt; `None` for `check`.
    pub prompt: Option<String>,
    pub file: String,
    /// The root branch's name, instead of the rig's.
    pub name: Option<String>,
    pub base: Option<String>,
    /// The root's executable, for development and testing.
    pub command: Option<Vec<String>>,
    pub unapproved_tools: bool,
    pub json: bool,
}

/// `by artifact publish|list|get|share|export|import ...`; see `docs/storage.md`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ArtifactArgs {
    pub action: String,
    /// `publish`'s file, `get`/`share`'s id, or `import`'s bundle file.
    pub arg: Option<String>,
    /// `export`'s artifact ids, one or more.
    pub ids: Vec<String>,
    pub name: Option<String>,
    pub media_type: Option<String>,
    pub labels: Vec<(String, String)>,
    pub out: Option<String>,
    pub to: Option<String>,
    pub branch: Option<String>,
    pub json: bool,
}

/// `by scratch create|list|lock|unlock|share ...`; see `docs/storage.md`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScratchArgs {
    pub action: String,
    pub name: Option<String>,
    pub to: Option<String>,
    pub branch: Option<String>,
    pub json: bool,
}

/// The variables behind [`Globals`], each also a `--flag`.
pub const GLOBAL_ENV: [&str; 4] = [
    "BRANCHYARD_REMOTE",
    "BRANCHYARD_TOKEN_FILE",
    "BRANCHYARD_REPO",
    "BRANCHYARD_CA_FILE",
];

/// Options choosing where commands run, before or after the command. A
/// flag wins over its variable; a blank variable counts as unset (see
/// [`unset_blank_env`]).
#[derive(Args, Clone, Debug, Default, PartialEq, Eq)]
#[command(next_help_heading = "Global options")]
pub struct Globals {
    /// Run commands on a Branchyard server at URL instead of locally
    #[arg(
        long,
        global = true,
        display_order = 1,
        value_name = "URL",
        env = "BRANCHYARD_REMOTE",
        hide_env_values = true,
        value_parser = non_blank
    )]
    pub remote: Option<String>,
    /// The server's bearer token, on the first line of FILE
    #[arg(
        long,
        global = true,
        display_order = 2,
        value_name = "FILE",
        env = "BRANCHYARD_TOKEN_FILE",
        hide_env_values = true,
        value_parser = non_blank
    )]
    pub token_file: Option<String>,
    /// Repository on the server, when it serves several
    #[arg(
        long,
        global = true,
        display_order = 3,
        value_name = "NAME",
        env = "BRANCHYARD_REPO",
        hide_env_values = true,
        value_parser = non_blank
    )]
    pub repo: Option<String>,
    /// Also trust this CA certificate for https
    #[arg(
        long,
        global = true,
        display_order = 4,
        value_name = "FILE",
        env = "BRANCHYARD_CA_FILE",
        hide_env_values = true,
        value_parser = non_blank
    )]
    pub ca_file: Option<String>,
    /// No bell or desktop notification when a branch needs you or ends
    #[arg(long, global = true, display_order = 5)]
    pub no_notify: bool,
    /// `[notify]` from the configuration files.
    #[arg(skip)]
    pub notify: branchyard_setup::config::Notify,
}

/// Unset each of [`GLOBAL_ENV`] that is set but blank, so that clap, which
/// reads them, treats it as unset rather than as an empty value. Call before
/// [`command`], while the process has one thread.
pub fn unset_blank_env() {
    for name in GLOBAL_ENV {
        if std::env::var_os(name).is_some_and(|v| v.to_string_lossy().trim().is_empty()) {
            std::env::remove_var(name);
        }
    }
}

/// The whole `by` command line.
#[derive(Parser, Debug, Clone, PartialEq)]
#[command(
    name = "by",
    version,
    about = "Delegate coding work to agent harnesses on git branches, and merge only validated \
             results.",
    after_help = GENERAL_AFTER_HELP,
    // Commands without a `display_order` go under "Other commands".
    next_display_order = None,
)]
pub struct Cli {
    #[command(flatten)]
    pub globals: Globals,
    /// `None` prints the status card (`crate::setup_cmd::card`); `--help` is the general help.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The groups of `by --help`'s command list, by `display_order / 100`.
pub const GROUPS: &[(usize, &str)] = &[
    (1, "Work on branches"),
    (2, "Inspect"),
    (3, "Delegate and coordinate (usually from inside a harness)"),
    (4, "Rigs and shared storage"),
    (5, "Servers"),
    (6, "Shell and setup"),
];

const GENERAL_AFTER_HELP: &str = "\
Examples:
  by run \"fix the flaky test\" --check \"cargo test\" --yes
  by fan \"speed up the parser\" --harness claude-code,codex
  by ls && by diff fix-the-flaky-test && by merge fix-the-flaky-test
  by --remote https://ci.example:8421 --token-file ~/.by-token ls

Run 'by help <command>' or 'by <command> --help' for its options.
Local mode: harnesses run as your operating-system user, with no other
isolation. State lives in .branchyard/ at the repository root. Remote mode:
harnesses run as the server's user, with no other isolation.
To isolate: --isolated (a private HOME, not a sandbox) or --provider
microsandbox (a microVM; needs a build with that feature). Where harnesses run
and where state lives, rung by rung: docs/modes.md.";

const RUN_EXAMPLES: &str = "\
Examples:
  by run \"fix the flaky test\" --check \"cargo test -p core\" --yes
  by run \"fix the flaky test\" --auto --kind bugfix     # routed by [fleet]
  by run \"add a --verbose flag\" -n verbose --harness codex --budget-usd 2
  by run \"port the build\" --provider microsandbox --image ghcr.io/me/claude:1 \\
      --pass-env ANTHROPIC_API_KEY
  by run -- \"--explain is a prompt here\"
  by run \"fix the flaky test\" --detach --yes     # in the background; prints the branch

--detach (local mode) starts the turn in its own process group with no
terminal and returns once the turn has started: follow it with by log -f
BRANCH or by watch, wait with by wait BRANCH, stop it with by cancel BRANCH.
Its output goes to a log under .branchyard/detached/. With no terminal, tool
permission requests are denied unless --yes, --permissions or branchyard.toml
allows them; --ask is refused. See docs/surfaces.md.";

const FAN_EXAMPLES: &str = "\
Examples:
  by fan \"speed up the parser\" --harness claude-code,codex,gemini-cli --check \"cargo test\"
  by fan \"fix the parser crash\" --auto --attempts 3 --judge
  by ls

--auto picks the harnesses from the [fleet] table in branchyard.toml (the
default when there is a [fleet] and no --harness); --judge then scores the
attempts and proposes one. See docs/fleet.md.";

const MAP_EXAMPLES: &str = "\
Examples:
  by map \"Find the license and stars of {{item.repo}}\" --items repos.csv \\
      --schema answer.schema.json --out results.csv --concurrency 8 --yes
  git ls-files '*.md' | by map \"Fix spelling in {{item}}\" --rm --yes
  by map \"Summarize issue {{item.number}}\" --from-command \"gh issue list --json number\" \\
      --input-format json --reduce \"Group these by theme\" --yes
  by map resume find-the-license-and-stars-of-item-repo
  by map show find-the-license-and-stars-of-item-repo

Each item runs on its own branch, <map>-<item id>. With --schema, each branch
must end its reply with JSON matching the schema; an invalid answer gets one
follow-up turn. Running the same command again (or by map resume) skips the
items done. See docs/map.md.";

const JUDGE_EXAMPLES: &str = "\
Examples:
  by judge speed-up-the-parser                  # a fan's branches
  by judge a b c --harness claude-code --json
  by judge speed-up-the-parser --pick --discard-others --yes

Runs each attempt's check on its exact candidate, scores it without a model
(check, diff size, cost, time), and, with a judge harness (--harness, or the
[fleet] entry's judge), asks it for a JSON verdict on a read-only scratch
branch. An answer that is not a strict verdict falls back to the
deterministic score. Local mode only. See docs/fleet.md.";

const FLEET_EXAMPLES: &str = "\
Examples:
  by fleet stats
  by fleet stats --kind bugfix --json
  by fleet route \"fix the flaky parser test\" --seed 7

The table lives in branchyard.toml as [fleet.<kind>] and [fleet.default];
see docs/fleet.md.";

const SEND_EXAMPLES: &str = "\
Examples:
  by send fix-the-flaky-test \"now add a regression test\"
  by send fix-the-flaky-test \"also cover Windows\" --steer
  by send fix-the-flaky-test --prompt-file review.md     # a long prompt, from a file (- for stdin)
  by send fix-the-flaky-test \"now the docs\" --detach --yes   # in the background, as by run --detach";

const STEER_EXAMPLES: &str = "\
Examples:
  by steer fix-the-flaky-test \"also cover Windows\"
  by steer fix-the-flaky-test --prompt-file note.md     # from a file (- for stdin)";

const REVIEW_EXAMPLES: &str = "\
Opens the branch's diff in your editor. Write a comment on its own line
starting with >> under the line it is about (under a file's header: the whole
file; under an @@ line: the hunk). On save, every comment goes to the branch
as one prompt, formatted File / Line / User comment, as by send would send it.
With no comments nothing is sent; an unsent review is kept and reopened.

Examples:
  by review fix-the-flaky-test
  by review fix-the-flaky-test --editor \"code --wait\"
  by review fix-the-flaky-test --print
  by review fix-the-flaky-test --file review.diff --yes";

const FORK_EXAMPLES: &str = "\
Examples:
  by fork fix-the-flaky-test \"try a lock instead\" -n with-lock
  by fork fix-the-flaky-test \"try a lock instead\" --detach --yes   # in the background, as by run --detach";

const TASK_EXAMPLES: &str = "\
Every run is a task: `by run` starts one with one attempt, `by fan` one with an
attempt per harness, `by map` one whose items are its attempts. Beside each
checkpoint, the task's record is committed: the files with .task/ (what was
asked, each turn's conversation), never merged or diffed. A folder task keeps its git
directory in ~/.branchyard/tasks/<id>/ and writes the folder only on accept,
refusing to overwrite a file changed outside the task.

Examples:
  by task new --folder ~/Documents/board \"Update the Q3 deck from the new numbers\"
  by task new --no-files \"Draft a reply to the vendor\"
  by task ls
  by task show 01JA2B3C
  by task rewind 01JA2B3C --to 1 --yes
  by task fork 01JA2B3C --at 1 \"Try a shorter version\"
  by task accept 01JA2B3C --attempt update-the-q3-deck-2";

const REWIND_EXAMPLES: &str = "\
Each turn ends with a checkpoint, refs/branchyard/<branch>/<incarnation>/turn-<N>;
`by show` lists them. Later checkpoints are kept until the branch is removed, so
a rewind is undone by rewinding forward again. The next turn resumes the
harness's session only if it ended at that checkpoint; otherwise it starts a
fresh session with a summary of the turns before, and says so.

Examples:
  by rewind fix-the-flaky-test --to 2
  by rewind fix-the-flaky-test --to 4 --yes      # forward again
  by fork fix-the-flaky-test --at 1 \"try a lock instead\"";

const TRY_EXAMPLES: &str = "\
Applies the branch's diff against its base to this checkout, only when it is
clean, and records what it changed under .branchyard/try/ so --off restores
it exactly. Local mode only.

Examples:
  by try fix-the-flaky-test
  by try --status
  by try other-attempt      # swaps: the first try is turned off
  by try --off";

const COMPARE_EXAMPLES: &str = "\
Examples:
  by compare --fan speed-up-the-parser
  by compare a b c --check --json
  by compare --fan speed-up-the-parser --diff speed-up-the-parser-codex speed-up-the-parser-claude-code
  by compare --fan speed-up-the-parser --pick speed-up-the-parser-codex --discard-others";

const SPAWN_EXAMPLES: &str = "\
Examples (inside a harness, the parent is the harness's own branch):
  by spawn \"write the tokenizer\" --harness codex --budget-usd 1 --wait
  by spawn \"write the parser\" --depends-on tokenizer --after integrated
  by spawn \"fix it\" --parent root --yes            # outside a harness
  by spawn --prompt-file task.md --name parser       # a long task, from a file (- for stdin)
  by spawn \"list the unsafe blocks\" --result-schema '{\"type\":\"object\",\"required\":[\"files\"],\"properties\":{\"files\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}}}}' --result-required

The child's check (--check) defaults to its parent's, and the spawn says which it
inherits. It runs on the merge when the child is integrated, so siblings that share
one test suite are integrated together: by integrate a b (checked once).";

const DELEGATE_EXAMPLES: &str = "\
Example, inside a harness (outside one, add --parent BRANCH):
  by delegate --plan plan.json        # then end your turn
  cat plan.json
  {\"brief\": \"Parse with the Token type in src/token.rs; add no crates.\",
   \"tasks\": [
     {\"name\": \"lexer\", \"prompt\": \"Write the lexer\", \"owns\": [\"src/lexer\"]},
     {\"name\": \"parser\", \"prompt\": \"Write the parser\", \"owns\": [\"src/parser\"],
      \"depends_on\": [\"lexer\"]}]}

The children are created all or none. Those that wait for nothing start at once, and
you are woken with a summary of each when they have all settled. by brief TEXT adds a
decision every child is given; by steer corrects one that is running.";

const DISCARD_EXAMPLES: &str = "\
Examples:
  by discard lru-linkedlist --reason \"the ordered-dict version won\"
  by discard flaky-fix --json

A running child is refused: by cancel it first. by cancel stops a turn; by
discard settles what a stopped or finished child is. Its worktree stays until
by rm.";

/// The edit format of `by graph apply`, from `branchyard::GraphEdit`.
pub(crate) const GRAPH_APPLY_HELP: &str = "\
A proposal is {\"expected_revision\": N, \"edits\": [EDIT, ...]}; --edits takes the
array alone. Each edit is an object tagged by \"kind\":

  {\"kind\": \"spawn\", \"prompt\": \"...\", ...}   a new child; it takes what by spawn
      does, by the MCP tool's names: name, harness, base, budget {max_usd,
      max_turns, max_minutes}, check [\"cmd\", \"arg\", ...] (a literal argv
      array, not a string to shell-split like by spawn --check), max_depth,
      max_children, harnesses [ids], deny [tools], seat, depends_on [names],
      after (settled or integrated), bindings [{scratch, access}], connectors
      [grants], plan, model
  {\"kind\": \"add_dependency\", \"dependent\": \"B\", \"prerequisite\": \"A\",
   \"after\": \"settled\"}   B waits for A; B must not have started
  {\"kind\": \"remove_dependency\", \"dependent\": \"B\", \"prerequisite\": \"A\"}

Examples:
  by graph show --json          # the revision to propose against
  by graph apply --expected-revision 3 --edits '[
    {\"kind\": \"spawn\", \"name\": \"schema\", \"prompt\": \"Add the migration\"},
    {\"kind\": \"spawn\", \"name\": \"api\", \"prompt\": \"Use the column\",
     \"depends_on\": [\"schema\"], \"after\": \"integrated\"}]'
  by graph apply proposal.json --parent root --yes      # outside a harness

All or nothing: a stale revision is the error stale_revision; run by graph
show and propose again. See docs/graph.md.";

const MERGE_EXAMPLES: &str = "\
Examples:
  by merge fix-the-flaky-test
  by merge fix-the-flaky-test --into release
  by merge fix-the-flaky-test --rm";

const WORKSPACE_EXAMPLES: &str = "\
Examples:
  by workspace show
  by workspace trust
  by workspace run fix-the-flaky-test dev
  by workspace show fix-the-flaky-test --json

[workspace] in branchyard.toml copies untracked files into each new worktree,
runs setup before its first turn and teardown when it is removed. Its scripts
never run until you trust them; see docs/workspace.md.";

const ENV_EXAMPLES: &str = "\
Examples:
  by env list
  by env show
  by env rebuild
  by env prune --keep 2 --older-than 7
  by env pool fill

With prepare = true in [workspace], setup runs once per environment key (the
setup commands, copy globs and lockfiles) and new branches start from what it
produced. A failed build never replaces the last good one. See
docs/environments.md. With [workspace.pool], ready worktrees wait for new
branches; by serve and by worker refill them. See docs/pools.md.";

const PR_EXAMPLES: &str = "\
Examples:
  by pr fix-the-flaky-test                       # push, then open or update the PR
  by pr fix-the-flaky-test --draft --base release
  by pr fix-the-flaky-test --watch --yes         # feed CI failures and reviews back
  by run --issue 42 --check \"cargo test\" && by pr issue-42-parser-crash

Needs the GitHub CLI, gh, logged in (gh auth login). The branch must be
ready and its check must pass on its candidate; --allow-not-ready and
--allow-failing-check override that. Local mode only. See docs/pull-requests.md.";

const HARNESSES_EXAMPLES: &str = "\
Examples:
  by harnesses                                  # installed here: version, login, quota
  by harnesses --on ssh://me@build.example      # on another machine
  by harnesses --remote https://by.internal     # on a server's live workers
  by harnesses install codex                    # the catalog's command, under [harnesses] install
  by harnesses update codex --version 0.200.0 --yes
  by harnesses login codex                      # its own login flow, once
  by harnesses login antigravity --yes          # with GEMINI_API_KEY set: select the gemini provider
  printf %s \"$KEY\" | by harnesses login claude-code --api-key
  by harnesses --profiles                       # the profiles Branchyard drives
  by harnesses --all --json | jq '.[] | select(.id == \"codex\") | .install'

Detection never reads a secret. LOGIN says what is known: logged in (the harness's
own status command said so), key set (VAR) or key stored (VAR) (a key, by name, not
checked), credentials found (a file exists, not read), logged out, login not
verified (nothing Branchyard looks for is there), unknown. Every installed harness
is asked its version at once, ten seconds at most (BRANCHYARD_HARNESS_TIMEOUT sets
the seconds); one that is slower is slow to start, not unknown. Installs run only
as [harnesses] install allows (ask on a terminal, never elsewhere, unless your user
file says otherwise) and are logged. --all reads catalog/harnesses.toml, generated
from emdash's and Orca's agent registries. See docs/harness-lifecycle.md.

login exits 0 when the login ran or a key was stored, 2 when it only printed
what you must run (no terminal here), and 1 when it failed; --json adds
\"status\": done, stored, manual or failed.";

const USAGE_EXAMPLES: &str = "\
Examples:
  by usage
  by usage --json | jq '.logins[] | {harness, five_hour: .five_hour.used_percent}'

Read from each login's own session files, never a credential: Codex records
its rate limits there; Claude Code records tokens only, so its percent needs
[usage] claude_five_hour_tokens. [usage] guard = \"refuse\" stops by run and
by fan near a limit; skip_over makes the router pass a candidate over. See
docs/usage.md.";

const ADOPT_EXAMPLES: &str = "\
Examples:
  by adopt                         # this repository's Claude Code and Codex sessions
  by adopt 3f2a9c --name parser    # make one a branch
  by send parser \"now add a test\"  # resumes the adopted session

The branch's worktree starts at the commit the session recorded (Codex), else
the HEAD of the directory it ran in, with that directory's uncommitted changes
to tracked files applied (--no-diff leaves them out). See docs/usage.md.";

const OPEN_EXAMPLES: &str = "\
Examples:
  by open fix-the-flaky-test
  by open fix-the-flaky-test --editor cursor
  cd \"$(by open fix-the-flaky-test --print)\"";

const WATCH_EXAMPLES: &str = "\
Examples:
  by watch
  by watch --interval 250ms
  by watch --once | less";

const SETUP_EXAMPLES: &str = "\
Steps, in order, each skipped when there is nothing to do:
  detect     the repository, the harnesses on PATH with their versions, whether each is
             logged in, and the check command the repository's files suggest
  config     branchyard.toml with the detected defaults, when there is none; on a terminal
             it asks only what cannot be detected (the default harness when several are
             logged in, a check when none is found, isolation)
  hooks      each harness on PATH that has an install (by init plugin): its hooks,
             commands and skills, with every file listed first; then a model-free check
             that its hook command answers
  logins     for each harness that is logged out or whose login cannot be told, the
             command to run; setup never starts a login flow
Prints a short card: what was detected, written and installed, \"Only you can do:\" lines,
and the next command. A second run writes nothing and says so. Exit 0 when done, even with
things left for you; 1 only on a real error. Without a terminal and without --yes it
behaves as --dry-run.

Examples:
  by setup                       # asks once before writing, on a terminal
  by setup --yes                 # take the defaults, ask nothing
  by setup --dry-run             # show everything, write nothing
  by setup --yes --harness claude-code,codex
  by setup --yes --no-hooks      # branchyard.toml only
  by setup --json --dry-run      # detected, written, installed, todo, next

See docs/setup.md#one-command.";

const INIT_EXAMPLES: &str = "\
Examples:
  by init                                            # the wizard, on a terminal
  by init server --defaults                          # the wizard, every default taken
  by init --json                                     # the topics
  by init project --json --next --answers answers.json
  by init project --answers answers.json --dry-run
  by init project --answers - --apply --json < answers.json
  by init server --defaults --apply --force
  by init plugin                                     # each harness Branchyard installs for, detected first
  by init plugin codex hermes                        # their hooks: every diff shown, then asks
  by init plugin --all-detected --dry-run            # the plan for every harness on PATH
  by init plugin cursor --apply                      # write without asking (--force replaces)

Answers name where a secret is (a variable, or @file), never its value.
Generated tokens are written 0600 and printed nowhere. See docs/setup.md.";

const CONFIG_EXAMPLES: &str = "\
Files: ~/.config/branchyard/config.toml (BRANCHYARD_USER_CONFIG overrides the
path), then branchyard.toml at or above the current directory, up to the
repository root. The project file overrides the user file key by key;
BRANCHYARD_REMOTE, BRANCHYARD_TOKEN_FILE, BRANCHYARD_REPO and
BRANCHYARD_CA_FILE override both; flags override everything. Neither file is
read inside a harness running on a branch (BRANCHYARD_BRANCH set).
Write one with `by init project`.

Examples:
  by config show
  by config show --json
  by config validate
  by config validate ~/.config/branchyard/config.toml
  by config schema > branchyard.config.json";

const UP_EXAMPLES: &str = "\
Steps, each ready, planned, needs_person or blocked, with a line on what to do:
  store            .branchyard/, made by opening the repository
  harness.<id>     each harness the configuration names ([defaults], [fleet], [knowledge];
                   claude-code when none): installed, or installed here when [harnesses]
                   in your user file allows it (ask on a terminal, --yes, or auto)
  login.<id>       logged in, or how to (the login command, the key variable)
  secret.<NAME>    each [secrets] entry and [models] key: its variable set or file present
  models           the model gateway [models] describes
  connectors       the connector gateway: listening, started here, or without Anvil
  provider         [defaults] provider: build and host access, Substrate connection,
                   or a recipe you trust
  workspace        [workspace] scripts trusted (by workspace trust)
Exit 0 when every step is ready, 1 otherwise. Nothing prints a secret's value.

Examples:
  by up                    # install what is allowed, start the gateway, list the rest
  by up --plan             # inspect readiness without installing or starting services
  by up --yes              # confirm the installs the policy asks about
  by up --json             # the steps as JSON, for a script or the Python SDK

See docs/up.md.
";

const HOOK_EXAMPLES: &str = "\
Reads Claude Code's hook JSON (session_id, cwd) on stdin and prints its hookSpecificOutput
with additionalContext, or nothing. With BRANCHYARD_BRANCH set: that branch's unread messages.
Otherwise: branches that finished, failed, became blocked or started asking since this
session's last hook call (remembered in .branchyard/hooks/<session_id>.json). Reads local
state only, gives up after two seconds, and always exits 0.

The plugin's hooks/hooks.json runs it; to run it from settings.json instead:
  \"hooks\": {\"UserPromptSubmit\": [{\"hooks\": [{\"type\": \"command\",
    \"command\": \"by hook user-prompt-submit\"}]}]}

Other harnesses: --harness ID reads that harness's hook JSON and answers in its shape, for
example `by hook --harness gemini-cli user-prompt-submit` (a BeforeAgent hook). An unknown
ID prints nothing and exits 0. --harness generic reads {\"cwd\", \"session_id\"} and prints
{\"context\": \"...\"}, for plugins written in code.

See docs/harness-integration.md and docs/harness-hooks.md.
";

const STATUSLINE_EXAMPLES: &str = "\
Prints, for example:  by: feat-x · 2 running · 1 asking · 3 ready
(the branch whose worktree Claude Code runs in, then the counts that are not zero).
Set it in ~/.claude/settings.json or .claude/settings.json; by never edits them:
  \"statusLine\": {\"type\": \"command\", \"command\": \"by statusline\"}

See docs/harness-integration.md.
";

const DOCTOR_EXAMPLES: &str = "\
Checks, each ok, todo, warn or fail with a line on what to do; a Mode: line above the
table says where branches run:
  repository       a repository; .branchyard/ and its store (none yet is a todo: by
                   setup); the store's schema
                   against this binary's; services whose lease ran out
  configuration    branchyard.toml and the user file load and validate (a refusal is
                   shown with its line); [workspace] scripts trusted, as they are now
  harnesses        what `by harnesses` detects, a row for each installed harness
                   (version, login, live run); [defaults] harness installed;
                   credential variables, by name only
  provider         the configured provider as the engine checks it; kept sandboxes
                   and snapshots against max_kept_minutes
  host             unprivileged user namespaces (egress confinement), a loopback
                   listener, /dev/kvm
  server           a configured [remote] answers GET /v1/repos with its token
  model gateway    [models] backends' key variables set; routed models priced

Exit status: 0 when nothing failed (a todo and a warning do not fail), 1 when something
did. Offline apart from a configured server, which gets 5 seconds. It opens an existing
.branchyard/ as every command does, creates none, and prints no secret.

Examples:
  by doctor
  by doctor --json
  by doctor --path ~/src/app
  by doctor --remote https://ci.example:8421 --token-file ~/.by-token

See docs/doctor.md.";

const MODELS_EXAMPLES: &str = "\
Examples:
  by models                        # routes, backends, budgets, and this month's usage
  by models --period day --json
  by models list                   # the models your keys can call, from the providers' own APIs
  by models list --refresh         # ask them again (otherwise the cache, with its age)
  by models list --harness codex   # the catalog's list beside what OpenAI lists now
  by run --model-gateway 'fix the bug'              # this branch's model calls go through the gateway
  by run --model-gateway='claude-sonnet-*' 'fix it' # and only to these models

[models] in branchyard.toml names the backends (their API, URL and the secret
holding each key), the routes by model, and daily and monthly budgets. A
branch on the gateway gets its own gateway each turn, on the turn's token; the
key never reaches the harness. See docs/model-gateway.md.";

const MODELS_LIST_EXAMPLES: &str = "\
Examples:
  by models list                   # every source whose key is set: Gemini, OpenAI, Anthropic, [models.backends]
  by models list --refresh         # list them again and cache the answer
  by models list --harness gemini-cli
  by models list --json

Each source whose key is in the environment (GEMINI_API_KEY, OPENAI_API_KEY or
CODEX_API_KEY, ANTHROPIC_API_KEY, or the key a [models.backends] entry names) is
asked for its models; one without a key is skipped, one that fails says its
status. A model neither catalog/pricing.toml nor [models.prices] can price is
marked: a dollar budget cannot count its calls. The answer is cached under
~/.cache/branchyard/catalog (or $BRANCHYARD_CATALOG_DIR) and shown, with its
age, until --refresh. HTTPS_PROXY, NO_PROXY and SSL_CERT_FILE are honored.
Keys are never printed. See docs/model-gateway.md.";

const SYNC_EXAMPLES: &str = "\
Examples:
  by sync                          # push and pull every branch that changed
  by sync fix-login                # one branch
  by sync status                   # the remote, each task's state and lag, the counters
  by sync pull 3f2a9c01be47.fix-login
  by sync gc --dry-run
  by sync scrub --sample 500
  by sync hold fix-login --reason \"audit 42\"

[sync] in your user configuration (~/.config/branchyard/config.toml) says where:
remote = \"gs://bucket/prefix\" (or s3://, az://, file:///, git+https://),
encrypt = \"passphrase\" or \"kms://...\", interval, bandwidth, retention. Objects are
written first and each task's manifest is swapped by compare-and-swap; two
machines that moved one branch apart keep both, the second as
refs/heads/conflict/<device>/<n>. See docs/sync.md.";

const SERVICES_EXAMPLES: &str = "\
Examples:
  by services                      # this repository's live services
  by services --kind connector_gateway --json
  by services --all                # with those that left or were reclaimed
  by services gc                   # reclaim what a stopped owner left
  by --remote https://by.example services   # a server's fleet

Services register themselves with what they can do and a lease their owner
renews; consumers find them by capability. A service whose owner stopped is
reclaimed: a leaked process stopped, a sandbox or recipe machine recovered
and destroyed, pool slots removed. Nothing Branchyard did not start is ever
stopped. See docs/registry.md.";

const CATALOG_EXAMPLES: &str = "\
Examples:
  by catalog refresh               # the MCP registry and npm, cached and verified
  by catalog refresh --only harnesses
  by catalog status --json

Nothing is fetched unless you ask: catalog/connectors.toml and
catalog/harnesses.toml stay the pinned baseline, and what a refresh adds is
cached with its ETag and checksum under ~/.cache/branchyard/catalog (or
$BRANCHYARD_CATALOG_DIR). A cache whose checksum does not verify is refused.
See docs/registry.md.";

const GATEWAY_EXAMPLES: &str = "\
Examples:
  by gateway start                 # in the background; its log in .branchyard/gateway/
  by gateway status --json
  by gateway start --foreground    # in this terminal, until interrupted
  by gateway rotate-key            # a new signing key; the previous one stays valid
  by gateway stop

[connectors] in branchyard.toml says where the gateway is (gateway), which
bundles it serves (bundles) and how to run Anvil (anvil). The gateway reads
this yard's public keys from .branchyard/gateway/jwks.json and writes its audit
log to .branchyard/gateway/audit.jsonl, which by log shows. See
docs/connectors.md.";

const CONNECT_EXAMPLES: &str = "\
Examples:
  by connect github
  by connect github --account work --open
  by connect github --api-key-stdin < ~/.config/github-token

Runs `anvil connect` against the gateway as you: it opens (or prints) the
connector's authorization URL, and the gateway keeps the upstream token. A
harness never sees it. See docs/connectors.md.";

const COMPLETIONS_EXAMPLES: &str = "\
Examples:
  by completions bash > ~/.local/share/bash-completion/completions/by
  by completions zsh > \"${fpath[1]}/_by\"
  by completions fish > ~/.config/fish/completions/by.fish
  by completions powershell >> $PROFILE";

#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum Command {
    /// Run a task on a new branch; waits for the turn (--detach returns at once)
    #[command(display_order = 100, after_help = RUN_EXAMPLES)]
    Run {
        /// The task for the harness; quote it (with --issue, added to the issue's text)
        #[arg(
            required_unless_present_any = ["issue", "pr"],
            default_value = "",
            hide_default_value = true
        )]
        prompt: String,
        #[command(flatten)]
        detached: Detached,
        #[command(flatten)]
        task: Checked<RunFlags>,
    },
    /// Run a task on several harnesses in parallel; waits, then compares
    #[command(display_order = 101, after_help = FAN_EXAMPLES)]
    Fan {
        /// The task for every harness; quote it (with --issue, added to the issue's text)
        #[arg(
            required_unless_present_any = ["issue", "pr"],
            default_value = "",
            hide_default_value = true
        )]
        prompt: String,
        /// Harnesses to run on, one branch each (default with a [fleet]: routed, as --auto)
        #[arg(long = "harness", value_name = "ID,ID,...", value_parser = harness_list)]
        harnesses: Option<List>,
        /// Branches to start when routed (default: the [fleet] entry's attempts)
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=16))]
        attempts: Option<u32>,
        /// Then judge the attempts and propose one, as `by judge` does
        #[arg(long)]
        judge: bool,
        /// Print JSON: the branches, as `by ls --json` prints them (activity goes to standard
        /// error), and with --judge the judgement
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        task: Checked<FanFlags>,
    },
    /// Run one prompt over a list of items, a branch for each
    ///
    /// Every item of the list gets its own branch, and the answers are collected.
    #[command(
        display_order = 101,
        after_help = MAP_EXAMPLES,
        args_conflicts_with_subcommands = true,
        subcommand_negates_reqs = true
    )]
    Map {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: Option<MapAction>,
        #[command(flatten)]
        map: MapArgs,
    },
    /// Send a branch another prompt; waits for the turn (--detach returns at once)
    #[command(display_order = 102, after_help = SEND_EXAMPLES)]
    Send {
        /// The branch to send to; `by ls` lists them
        branch: String,
        /// The next prompt; quote it
        #[arg(
            required_unless_present_any = ["retry", "prompt_file"],
            default_value = "",
            hide_default_value = true
        )]
        prompt: String,
        /// Read the prompt from this file instead, `-` for standard input: for a long prompt,
        /// or one a shell would mangle
        #[arg(long, value_name = "PATH", conflicts_with = "prompt")]
        prompt_file: Option<String>,
        /// Add the prompt to the branch's running turn without interrupting it, instead of
        /// starting a new turn; refused when no turn runs or the harness cannot take it
        #[arg(long)]
        steer: bool,
        /// Submit again the prompt of the branch's last turn that was cut off when the engine
        /// running it stopped (the recovery note names it), instead of a new prompt
        #[arg(long, conflicts_with_all = ["steer", "prompt", "prompt_file"])]
        retry: bool,
        /// Wait for the turn to end and show it (outside a harness, send always waits)
        #[arg(long)]
        wait: bool,
        /// Print JSON
        #[arg(long)]
        json: bool,
        /// Return as soon as the turn has started, leaving it running in the background; its
        /// output goes to a log under .branchyard/detached/ (local mode)
        #[arg(long, conflicts_with_all = ["ask", "steer", "wait"])]
        detach: bool,
        #[command(flatten)]
        task: Checked<SendFlags>,
    },
    /// Add to a branch's running turn without interrupting it
    ///
    /// The same as `by send --steer`.
    #[command(display_order = 102, after_help = STEER_EXAMPLES)]
    Steer {
        /// The branch whose running turn to add to
        branch: String,
        /// What to add; quote it
        #[arg(
            required_unless_present = "prompt_file",
            default_value = "",
            hide_default_value = true
        )]
        prompt: String,
        /// Read it from this file instead, `-` for standard input: for a long text, or one a
        /// shell would mangle
        #[arg(long, value_name = "PATH", conflicts_with = "prompt")]
        prompt_file: Option<String>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Comment on a branch's diff in your editor and send the comments
    ///
    /// Opens the diff in your editor, then sends every comment as one prompt.
    #[command(display_order = 111, after_help = REVIEW_EXAMPLES)]
    Review {
        /// The branch whose diff to review
        branch: String,
        /// Print the prompt the comments make instead of sending it
        #[arg(long)]
        print: bool,
        /// The editor: a known name or a command line that waits until the file is closed, such
        /// as "code --wait" (default: $VISUAL, then $EDITOR)
        #[arg(long, value_name = "EDITOR", conflicts_with = "file")]
        editor: Option<String>,
        /// Read the comments from this edited review file instead of opening an editor
        #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
        file: Option<String>,
        /// Return as soon as the send has started, leaving it running in the background; its
        /// output goes to a log beside the draft
        #[arg(long, conflicts_with = "print")]
        detach: bool,
        /// Print JSON: the prompt with --print, else what `by send --json` prints
        #[arg(long, conflicts_with = "detach")]
        json: bool,
        #[command(flatten)]
        task: Checked<SendFlags>,
    },
    /// Fork a branch's work and conversation; waits (--detach returns at once)
    #[command(display_order = 103, after_help = FORK_EXAMPLES)]
    Fork {
        /// The branch to fork
        branch: String,
        /// The new branch's prompt; quote it
        prompt: String,
        /// Start a new session if the harness cannot fork its conversation
        #[arg(long)]
        fresh_session: bool,
        /// Fork from the branch's checkpoint N (0 is its base) instead of its candidate; the
        /// session forks only if it ended there, else a fresh one starts with a summary
        #[arg(long, value_name = "N", conflicts_with = "fresh_session")]
        at: Option<u32>,
        #[command(flatten)]
        detached: Detached,
        #[command(flatten)]
        task: Checked<ForkFlags>,
    },
    /// Fork a branch's candidate into a fresh session with a generated handoff brief
    #[command(display_order = 104)]
    Reincarnate {
        /// The branch to start over from
        branch: String,
        #[command(flatten)]
        task: Checked<ReincarnateFlags>,
    },
    /// Merge a branch's candidate after its check passes
    #[command(display_order = 105, after_help = MERGE_EXAMPLES)]
    Merge {
        /// The branch to merge
        branch: String,
        /// Local branch to merge into (default: the current branch)
        #[arg(long, value_name = "TARGET")]
        into: Option<String>,
        /// Then remove the branch, running its workspace teardown, as `by rm` does
        #[arg(long)]
        rm: bool,
        /// Also perform the branch's staged effects (drafts and held calls), as approving
        /// each would
        #[arg(long)]
        promote_effects: bool,
        /// Print JSON: the branch, what it was merged into, the merge commit and the check's
        /// result
        #[arg(long)]
        json: bool,
    },
    /// A branch's workspace: trust its scripts, show it, or run a named script in it
    #[command(display_order = 110, subcommand_required = true, after_help = WORKSPACE_EXAMPLES)]
    Workspace {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: WorkspaceAction,
    },
    /// Prepared environments: list, show, rebuild or prune them; the warm pool
    #[command(display_order = 111, subcommand_required = true, after_help = ENV_EXAMPLES)]
    Env {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: EnvAction,
    },
    /// Environment recipes: repository scripts that create a VM
    ///
    /// List, check and trust them (see docs/recipes.md).
    #[command(
        display_order = 112,
        subcommand_required = true,
        after_help = crate::recipe_cmd::RECIPE_EXAMPLES
    )]
    Recipe {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: crate::recipe_cmd::RecipeAction,
    },
    /// Remove a branch and free its name
    ///
    /// Removes the branch's worktree and record. A removed child leaves its parent's
    /// children, and what its subtree spent still counts in the parent's budget.
    #[command(display_order = 106)]
    Rm {
        /// The branch to remove
        branch: String,
        /// Keep the credential files provisioning wrote in a home a fork still uses
        #[arg(long)]
        keep_credentials: bool,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Reset a branch to one of its per-turn checkpoints
    #[command(display_order = 107, after_help = REWIND_EXAMPLES)]
    Rewind {
        /// The branch to reset
        branch: String,
        /// The checkpoint to go to: a turn number, or 0 for the branch's base
        #[arg(long, value_name = "N")]
        to: u32,
        /// Confirm the reset without a prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Tasks: what was asked, and its attempts
    ///
    /// In this repository, a folder you grant, or none.
    #[command(display_order = 107, subcommand_required = true, after_help = TASK_EXAMPLES)]
    Task(Box<TaskCommand>),
    /// Apply a branch's changes to this checkout to try them; --off restores it
    #[command(display_order = 108, after_help = TRY_EXAMPLES)]
    Try {
        /// The branch to try (another branch's try is turned off first)
        #[arg(required_unless_present_any = ["off", "status"], conflicts_with_all = ["off", "status"])]
        branch: Option<String>,
        /// Restore the checkout to what it held before the try
        #[arg(long, conflicts_with = "status")]
        off: bool,
        /// Show what is being tried
        #[arg(long)]
        status: bool,
        /// With --off: restore even files changed since the try, discarding those changes
        #[arg(long, conflicts_with_all = ["branch", "status"])]
        force: bool,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Compare attempts side by side, and pick one to merge
    #[command(display_order = 109, after_help = COMPARE_EXAMPLES)]
    Compare {
        /// Branches to compare
        #[arg(required_unless_present_any = ["fan", "diff"], conflicts_with = "fan")]
        branches: Vec<String>,
        /// Compare the branches one `by fan` started, named NAME-<harness>
        #[arg(long, value_name = "NAME")]
        fan: Option<String>,
        /// Run each branch's check on its exact candidate, in a private worktree
        #[arg(long)]
        check: bool,
        /// Show the diff from attempt A's candidate to B's
        #[arg(long, num_args = 2, value_names = ["A", "B"], conflicts_with = "pick")]
        diff: Option<Vec<String>>,
        /// Merge this attempt (its check must pass, as with by merge)
        #[arg(long, value_name = "BRANCH")]
        pick: Option<String>,
        /// Local branch to merge the pick into (default: the current branch)
        #[arg(long, value_name = "TARGET", requires = "pick")]
        into: Option<String>,
        /// After the pick merges, remove the other attempts
        #[arg(long, requires = "pick")]
        discard_others: bool,
        /// Confirm removing the other attempts without a prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Score attempts at one task, and propose or merge one
    ///
    /// Optionally with a judge harness.
    #[command(display_order = 109, after_help = JUDGE_EXAMPLES)]
    Judge {
        /// A `by fan`'s name, or the branches to judge
        #[arg(required = true, value_name = "FAN|BRANCH")]
        targets: Vec<String>,
        /// Ask this harness for a verdict (default: the [fleet] entry's judge, if any)
        #[arg(long, value_name = "ID", conflicts_with = "deterministic")]
        harness: Option<String>,
        /// Score without a judge harness, even when the [fleet] names one
        #[arg(long)]
        deterministic: bool,
        /// Launch the judge harness with this instead of its executable, for development and
        /// testing
        #[arg(long, value_name = "CMD", value_parser = command_argv, requires = "harness")]
        command: Option<Argv>,
        /// More rubric for the judge harness, after the default
        #[arg(long, value_name = "TEXT")]
        rubric: Option<String>,
        /// Merge the proposed pick, as `by compare --pick` does
        #[arg(long)]
        pick: bool,
        /// Local branch to merge the pick into (default: the current branch)
        #[arg(long, value_name = "TARGET", requires = "pick")]
        into: Option<String>,
        /// After the pick merges, remove the other attempts
        #[arg(long, requires = "pick")]
        discard_others: bool,
        /// Confirm removing the other attempts without a prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Approvals waiting for an answer: list them, allow or deny one
    #[command(display_order = 114, after_help = APPROVALS_EXAMPLES)]
    Approvals(ApprovalsArgs),
    /// The effect ledger: what branches did outside the machine
    ///
    /// Show, promote or reconcile an entry.
    #[command(display_order = 115, after_help = EFFECTS_EXAMPLES)]
    Effects(EffectsArgs),
    /// Rewind a branch and undo what it did upstream
    ///
    /// Rewinds its files and conversation, and undoes what it did upstream where the
    /// upstream allows.
    #[command(display_order = 116, after_help = UNDO_EXAMPLES)]
    Undo(UndoArgs),
    /// A branch's plan: show it, approve it (as proposed or edited) or reject it
    #[command(display_order = 112, subcommand_required = true, after_help = PLAN_EXAMPLES)]
    Plan {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: PlanAction,
    },
    /// Repository knowledge: what agents are told
    ///
    /// Review, adopt, reject, edit, add, remove or export it.
    #[command(display_order = 113, subcommand_required = true, after_help = KNOWLEDGE_EXAMPLES)]
    Knowledge {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: KnowledgeAction,
    },
    /// The fleet table's routing: statistics and where a prompt goes
    ///
    /// Outcome statistics, or what a prompt would be routed to.
    #[command(display_order = 207, subcommand_required = true, after_help = FLEET_EXAMPLES)]
    Fleet {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: FleetAction,
    },
    /// Open or update a ready branch's GitHub pull request
    ///
    /// Pushes the branch first; --watch feeds CI and reviews back.
    #[command(display_order = 110, after_help = PR_EXAMPLES)]
    Pr {
        /// The branch to open the pull request for
        branch: String,
        #[command(flatten)]
        pr: Checked<PrFlags>,
    },
    /// List branches
    #[command(display_order = 200)]
    Ls {
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Summarize branches, turns and cost (and, with --remote, the server's queue)
    #[command(display_order = 209)]
    Stats {
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Show one branch
    #[command(display_order = 201)]
    Show {
        /// The branch to show; `by ls` lists them
        branch: String,
        /// Print JSON
        #[arg(long)]
        json: bool,
        /// Ask GitHub (through gh) for the branch's pull request state first, instead of
        /// showing the last one recorded (local mode)
        #[arg(long)]
        refresh: bool,
    },
    /// Show a branch's candidate diff against its base
    #[command(display_order = 202)]
    Diff {
        /// The branch whose changes to show
        branch: String,
    },
    /// Show a branch's recorded events
    #[command(display_order = 203)]
    Log {
        /// The branch whose events to show
        branch: String,
        /// Print JSON
        #[arg(long)]
        json: bool,
        /// Keep printing events as they are recorded, until interrupted; with --json, one
        /// object per line
        #[arg(short, long)]
        follow: bool,
    },
    /// Watch every branch live: status, activity, cost
    #[command(display_order = 204, after_help = WATCH_EXAMPLES)]
    Watch {
        /// Time between refreshes: seconds, or with a unit such as 250ms or 2s
        #[arg(long, value_name = "SECONDS", default_value = "1", value_parser = watch_interval)]
        interval: Duration,
        /// Print the tree once and exit
        #[arg(long)]
        once: bool,
    },
    /// Harnesses installed here or elsewhere; install, update, log in
    ///
    /// Shows each harness with its version, login and quota; installs, updates or logs in
    /// to one.
    #[command(display_order = 205, after_help = HARNESSES_EXAMPLES)]
    Harnesses {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        /// Every harness CLI in catalog/harnesses.toml, with install and login commands, API-key
        /// variables and models where known, marking the ones Branchyard can drive
        #[arg(long, conflicts_with_all = ["profiles", "on"])]
        all: bool,
        /// The harness profiles Branchyard drives, and whether each is on PATH
        #[arg(long, conflicts_with = "on")]
        profiles: bool,
        /// Detect again rather than use what was detected in the last minute
        #[arg(long)]
        refresh: bool,
        /// Another machine: ssh://[user@]host[:port], or recipe:NAME (a fresh machine from the
        /// repository's recipe)
        #[arg(long, value_name = "TARGET", global = true)]
        on: Option<String>,
        #[command(subcommand)]
        action: Option<HarnessesAction>,
    },
    /// Each Claude Code and Codex login's usage and when it resets
    ///
    /// The 5-hour and weekly usage of each local login.
    #[command(display_order = 208, after_help = USAGE_EXAMPLES)]
    Usage {
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Make a session already on this machine a branch
    ///
    /// A Claude Code or Codex session; --list shows them.
    #[command(display_order = 112, after_help = ADOPT_EXAMPLES)]
    Adopt {
        /// The session's ID, or a unique start of it (default: list them)
        session: Option<String>,
        /// List this repository's sessions (the default without SESSION)
        #[arg(long)]
        list: bool,
        /// The branch's name (default: a slug of the session's first prompt)
        #[arg(short, long, conflicts_with = "list")]
        name: Option<String>,
        /// Do not carry the session directory's uncommitted changes into the branch
        #[arg(long, conflicts_with = "list")]
        no_diff: bool,
        /// The profile its turns run with, one of the session's harness's (default: the
        /// harness's default profile), such as claude-code-acp
        #[arg(long, value_name = "ID", conflicts_with = "list")]
        harness: Option<String>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// The catalog of connectors the connector gateway (Anvil) can adopt
    ///
    /// See docs/connectors.md.
    #[command(display_order = 207, subcommand_required = true)]
    Connectors {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: ConnectorsAction,
    },
    /// Open a branch's worktree in your editor
    #[command(display_order = 206, after_help = OPEN_EXAMPLES)]
    Open {
        /// The branch whose worktree to open
        branch: String,
        /// The editor: code, cursor, zed, windsurf, subl, idea, nvim, ... or a command line
        /// (default: $VISUAL, then $EDITOR)
        #[arg(long, value_name = "EDITOR")]
        editor: Option<String>,
        /// Print the worktree's path instead of opening it
        #[arg(long, conflicts_with = "editor")]
        print: bool,
    },
    /// Delegate to a new child branch of this branch
    #[command(hide = true, display_order = 300, after_help = SPAWN_EXAMPLES)]
    Spawn {
        /// The child's task; quote it (with --issue, added to the issue's text)
        #[arg(
            required_unless_present_any = ["issue", "prompt_file"],
            default_value = "",
            hide_default_value = true
        )]
        prompt: String,
        /// Read the child's task from this file instead, `-` for standard input: for a long
        /// prompt, or one a shell would mangle
        #[arg(long, value_name = "PATH", conflicts_with = "prompt")]
        prompt_file: Option<String>,
        #[command(flatten)]
        spawn: Checked<SpawnFlags>,
    },
    /// Hand the engine a plan: several children at once
    ///
    /// With the brief they share; then end your turn, and you are woken with a summary when
    /// they have settled.
    #[command(hide = true, display_order = 300, after_help = DELEGATE_EXAMPLES)]
    Delegate {
        /// The plan as JSON, `-` for standard input: {"brief": TEXT, "tasks": [TASK, ...]},
        /// each task as the spawn tool takes a child ({"prompt", "name", "owns", "depends_on",
        /// "budget_usd", "check", "harness", ...}); depends_on may name another task
        #[arg(long, value_name = "PATH")]
        plan: String,
        /// The brief the children share, in place of the plan's: the goal, the conventions,
        /// the interfaces between their parts
        #[arg(long, value_name = "TEXT", value_parser = non_blank)]
        brief: Option<String>,
        /// Leave every task to the engine to land, as `spawn --land` does: each is integrated
        /// into your branch when it is ready, and your next turn says what landed
        #[arg(long)]
        land: bool,
        /// The delegating branch (outside a harness; inside one, it is the harness's own)
        #[arg(long, value_name = "BRANCH")]
        parent: Option<String>,
        /// Print JSON
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        perms: Perms,
    },
    /// Show or add to this branch's brief for its children
    ///
    /// The brief every child it delegates to is given each turn; with TEXT, add an entry
    /// first, sent to the children running now.
    #[command(hide = true, display_order = 300)]
    Brief {
        /// A new entry: a decision every child must follow, such as an interface you settled
        #[arg(conflicts_with = "file")]
        text: Option<String>,
        /// Read the new entry from this file instead, `-` for standard input
        #[arg(long, value_name = "PATH")]
        file: Option<String>,
        /// The delegating branch (outside a harness; inside one, it is the harness's own)
        #[arg(long, value_name = "BRANCH")]
        parent: Option<String>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Show a branch's status, candidate, cost, budget and last message
    #[command(hide = true, display_order = 301)]
    Inspect {
        /// Default: this harness's own branch
        branch: Option<String>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Show a branch's recorded events from a cursor
    #[command(hide = true, display_order = 302)]
    Events {
        /// Default: this harness's own branch
        branch: Option<String>,
        /// Start at event N (default: the most recent)
        #[arg(long, value_name = "N")]
        cursor: Option<usize>,
        /// At most N events (default 50, at most 200)
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Merge delegated children into this branch after their check
    ///
    /// Merges delegated children into their parent's branch after their check passes;
    /// several are merged together, all or none, and checked once on the result.
    #[command(hide = true, display_order = 303)]
    Integrate {
        #[arg(required = true, value_name = "BRANCH")]
        /// The children to merge, all of one parent
        branches: Vec<String>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Run a branch's check on its current work, without merging
    ///
    /// Runs the check on the branch's work merged into its parent's branch, as integrate
    /// would, without integrating it; run it on yourself before you finish.
    #[command(display_order = 303)]
    Check {
        /// Default: this harness's own branch
        branch: Option<String>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Wait for branches to settle
    ///
    /// Waits for delegated branches to settle: all of them, or the first with --any. Each
    /// is printed as it settles (with --json, as a JSON line on stderr), then the result.
    #[command(display_order = 303)]
    Wait {
        /// Default, inside a harness: its children still running
        #[arg(value_name = "BRANCH")]
        branches: Vec<String>,
        /// Return when the first of them settles
        #[arg(long, conflicts_with = "all")]
        any: bool,
        /// Return when all of them have settled (the default)
        #[arg(long)]
        all: bool,
        /// Give up after this many seconds; the result says timed_out, and by exits 1. Keep it
        /// under any limit of your own (a shell `timeout`, a tool call's time limit): a wait
        /// stopped from outside prints no result, only the branches that settled before
        #[arg(long, value_name = "SECONDS", value_parser = seconds)]
        timeout: Option<f64>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Stop a branch's running turn and the turns below it
    ///
    /// Stops the turn and every turn delegated below it (a settled child: by discard).
    #[command(display_order = 304)]
    Cancel {
        /// The branch whose turn to stop
        branch: String,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Set a settled child aside; it is never integrated
    ///
    /// It ends discarded with your reason, keeps its record and cost, and frees its slot in
    /// its parent's max_children.
    #[command(hide = true, display_order = 304, after_help = DISCARD_EXAMPLES)]
    Discard {
        /// The settled child to set aside
        branch: String,
        /// Why, recorded with it (default: who discarded it)
        #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
        reason: Option<String>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// List the branches a branch delegated to
    #[command(hide = true, display_order = 305)]
    Children {
        /// Default: this harness's own branch
        branch: Option<String>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Show a branch's children and dependencies, or apply a proposal
    ///
    /// See docs/graph.md.
    #[command(hide = true, display_order = 306, subcommand_required = true)]
    Graph {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: GraphAction,
    },
    /// Ask this branch's parent a question
    #[command(hide = true, display_order = 307)]
    Ask {
        /// Act as this branch (outside a harness; inside one, it is the harness's own)
        #[arg(long = "as", value_name = "BRANCH")]
        as_branch: Option<String>,
        /// The question
        text: String,
        /// Block up to this many seconds for an answer (default: return once the question is sent)
        #[arg(long = "wait", value_name = "SECONDS", value_parser = seconds, allow_negative_numbers = true)]
        wait_seconds: Option<f64>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Report to this branch's parent, optionally with a JSON result
    ///
    /// A result is checked against the contract the parent gave at spawn (`by inspect` shows it
    /// as result_contract); a mismatch is refused naming the path and the rule, and nothing is
    /// recorded. The parent reads the latest result in `by inspect` and `by wait`.
    #[command(hide = true, display_order = 308)]
    Report {
        /// Act as this branch (outside a harness; inside one, it is the harness's own)
        #[arg(long = "as", value_name = "BRANCH")]
        as_branch: Option<String>,
        /// What to report
        text: String,
        /// The branch's result, as JSON text
        #[arg(long, value_name = "JSON", value_parser = non_blank, conflicts_with = "result_file")]
        result: Option<String>,
        /// Read the result from this file instead, `-` for standard input
        #[arg(long, value_name = "PATH", value_parser = non_blank)]
        result_file: Option<String>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Escalate to this branch's parent, or further up if its rig seat allows
    #[command(hide = true, display_order = 309)]
    Escalate {
        /// Act as this branch (outside a harness; inside one, it is the harness's own)
        #[arg(long = "as", value_name = "BRANCH")]
        as_branch: Option<String>,
        /// What to escalate
        text: String,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// State this branch's own status in one line, beside the one Branchyard derives
    ///
    /// A claim the harness makes about itself, shown in `by log`, `by watch`, `by show` and
    /// `by inspect`; it changes nothing about the branch. `blocked` and `failure` are also
    /// escalated to the parent's inbox.
    #[command(hide = true, display_order = 312)]
    ReportStatus {
        /// Act as this branch (outside a harness; inside one, it is the harness's own)
        #[arg(long = "as", value_name = "BRANCH")]
        as_branch: Option<String>,
        /// What the harness is doing
        #[arg(value_parser = ["working", "idle", "blocked", "complete", "failure"])]
        state: String,
        /// One line, at most 280 characters
        summary: String,
        /// An http(s) URL with more, such as a pull request or an issue
        #[arg(long, value_name = "URL")]
        link: Option<String>,
        /// How long the claim holds (whole seconds, 1s to 7d, such as 10m or 2h); after it, the
        /// report is shown as stale
        #[arg(long, value_name = "DURATION", value_parser = report_ttl)]
        ttl: Option<Duration>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Answer a message (usually a question) from a descendant
    #[command(hide = true, display_order = 310)]
    Answer {
        /// Act as this branch (outside a harness; inside one, it is the harness's own)
        #[arg(long = "as", value_name = "BRANCH")]
        as_branch: Option<String>,
        /// The message's number, as `by inbox` lists it
        message_id: u64,
        /// The answer
        text: String,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// List messages addressed to this branch
    #[command(hide = true, display_order = 311)]
    Inbox {
        /// Act as this branch (outside a harness; inside one, it is the harness's own)
        #[arg(long = "as", value_name = "BRANCH")]
        as_branch: Option<String>,
        /// Only messages not yet delivered to a turn
        #[arg(long)]
        unread: bool,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Check a rig spec and print its plan, or run its root seat with a prompt
    #[command(display_order = 400, subcommand_required = true)]
    Rig {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: RigAction,
    },
    /// Publish, list, read, share, export or import an artifact (see docs/storage.md)
    #[command(display_order = 401, subcommand_required = true)]
    Artifact {
        /// Act as this branch (outside a harness; inside one, it is the harness's own)
        #[arg(long, global = true, value_name = "NAME")]
        branch: Option<String>,
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: ArtifactAction,
    },
    /// Create, list, lock, unlock or share a scratch area (see docs/storage.md)
    #[command(display_order = 402, subcommand_required = true)]
    Scratch {
        /// Act as this branch (outside a harness; inside one, it is the harness's own)
        #[arg(long, global = true, value_name = "NAME")]
        branch: Option<String>,
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: ScratchAction,
    },
    /// Serve repositories over an authenticated HTTP API (see 'by serve --help')
    // Its options are the server's own: `main` hands everything after `serve`
    // to `branchyard_server::cli` (see `server_call`) before this parser runs.
    #[command(hide = true, display_order = 500, disable_help_flag = true)]
    Serve {
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "SERVER_OPTIONS"
        )]
        /// The server's own options (see `by serve --help`)
        args: Vec<String>,
    },
    /// Run the operations queued in a PostgreSQL database (see 'by worker --help')
    // `by serve --worker`; like `serve`, handed to the server's own parser.
    #[command(hide = true, display_order = 501, disable_help_flag = true)]
    Worker {
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "SERVER_OPTIONS"
        )]
        /// The server's own options (see `by worker --help`)
        args: Vec<String>,
    },
    /// Serve a branch's delegation tools over MCP on stdio (started by the engine)
    #[command(hide = true, display_order = 502)]
    Mcp {
        /// Repository root
        #[arg(long, value_name = "DIR", required_unless_present = "list_tools")]
        root: Option<String>,
        /// The branch whose turn this server serves
        #[arg(long, value_name = "NAME", required_unless_present = "list_tools")]
        branch: Option<String>,
        /// Print the tools it serves, with their input schemas, as JSON, and exit; needs no
        /// repository, branch or token
        #[arg(long, conflicts_with_all = ["root", "branch"])]
        list_tools: bool,
    },
    /// Run tasks on a schedule or from webhooks
    ///
    /// Add, list, show, test, enable, disable and rm triggers, and see their runs (see
    /// docs/triggers.md).
    #[command(
        display_order = 503,
        subcommand_required = true,
        after_help = crate::trigger_cmd::TRIGGER_EXAMPLES
    )]
    Trigger {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: crate::trigger_cmd::TriggerAction,
    },
    /// Servers that --remote ssh:// started on other hosts
    ///
    /// Their status, or stop one (see docs/remote-ssh.md).
    #[command(
        display_order = 504,
        subcommand_required = true,
        after_help = crate::ssh_remote::REMOTE_EXAMPLES
    )]
    Remote {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: crate::ssh_remote::RemoteAction,
    },
    /// Print a shell completion script for by
    #[command(hide = true, display_order = 600, after_help = COMPLETIONS_EXAMPLES)]
    Completions {
        #[arg(value_enum)]
        /// The shell to print the script for
        shell: clap_complete::Shell,
    },
    /// Print by's man page (roff), for `man -l -` or a man directory
    #[command(hide = true, display_order = 601)]
    Man,
    /// Set this repository up in one command
    ///
    /// Project defaults, hooks for the harnesses on PATH, and what only you can do.
    #[command(display_order = 602, after_help = SETUP_EXAMPLES)]
    Setup {
        #[command(flatten)]
        setup: SetupFlags,
    },
    /// Set up Branchyard by interview
    ///
    /// Project defaults, a server, a rig, a deployment, skills.
    #[command(display_order = 602, after_help = INIT_EXAMPLES)]
    Init {
        #[command(flatten)]
        init: Checked<InitFlags>,
    },
    /// Show, locate or validate branchyard.toml and the user configuration
    #[command(display_order = 603, subcommand_required = true, after_help = CONFIG_EXAMPLES)]
    Config {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Check that this machine and repository are ready, and say what to fix
    #[command(display_order = 604, after_help = DOCTOR_EXAMPLES)]
    Doctor {
        /// Print JSON
        #[arg(long)]
        json: bool,
        /// Check the repository at PATH instead of the one around the current directory
        #[arg(long, value_name = "PATH", value_hint = ValueHint::DirPath)]
        path: Option<String>,
    },
    /// Bring the configured backends up
    ///
    /// Installs harnesses, starts the gateway, and says what only you can do.
    #[command(display_order = 605, after_help = UP_EXAMPLES)]
    Up {
        /// Print JSON
        #[arg(long)]
        json: bool,
        /// Inspect readiness without installing harnesses or starting services
        #[arg(long)]
        plan: bool,
        /// Make the installs the [harnesses] policy would ask about, without asking
        #[arg(long)]
        yes: bool,
    },
    /// Prepare or check the saved Agent Substrate connection
    #[command(display_order = 605)]
    Substrate {
        #[command(subcommand)]
        action: crate::substrate_cmd::Action,
    },
    /// Tell a harness session what changed in this repository
    ///
    /// Tells a Claude Code session (or, with --harness, another harness's) what changed in
    /// this repository's branches. Run by the plugin's hooks with the hook's JSON on stdin;
    /// prints nothing and exits 0 on any problem.
    #[command(hide = true, display_order = 606, after_help = HOOK_EXAMPLES)]
    Hook {
        /// Read and answer in this harness's hook dialect: claude-code (the default), codex,
        /// gemini-cli, qwen-code, github-copilot, droid, deepseek-harness, cursor, antigravity,
        /// kiro, hermes, or generic
        #[arg(long, value_name = "ID")]
        harness: Option<String>,
        /// session-start or user-prompt-submit
        event: String,
    },
    /// Print one line about the branches for a status line
    ///
    /// For Claude Code's (or, with --harness, another harness's) status line. Reads its
    /// JSON on stdin; prints nothing outside a repository Branchyard uses.
    #[command(hide = true, display_order = 607, after_help = STATUSLINE_EXAMPLES)]
    Statusline {
        /// Read the directory from this harness's status-line JSON (as for `by hook --harness`)
        #[arg(long, value_name = "ID")]
        harness: Option<String>,
    },
    /// Run, stop or inspect the connector gateway (Anvil), and its keys
    ///
    /// The connector gateway (Anvil) serves this repository's connectors to branches.
    #[command(display_order = 403, subcommand_required = true, after_help = GATEWAY_EXAMPLES)]
    Gateway {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: GatewayAction,
    },
    /// The model gateway: routes, backends, budgets and cost
    ///
    /// What its calls cost; `list` asks the providers which models your keys can call.
    #[command(
        display_order = 405,
        after_help = MODELS_EXAMPLES,
        args_conflicts_with_subcommands = true
    )]
    Models {
        /// Usage over this period (UTC): day, month, or all
        #[arg(long, value_name = "PERIOD", default_value = "month", value_parser = ["day", "month", "all"])]
        period: String,
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: Option<ModelsAction>,
    },
    /// Connect your account for a connector through the connector gateway (Anvil)
    #[command(display_order = 404, after_help = CONNECT_EXAMPLES)]
    Connect {
        /// The connector, as the gateway serves it, such as github
        connector: String,
        /// Which of your accounts to connect (default: your default account)
        #[arg(long, value_name = "NAME")]
        account: Option<String>,
        /// For a key-based connector: read the API key or personal token from stdin
        #[arg(long)]
        api_key_stdin: bool,
        /// Also open the authorization URL in your browser
        #[arg(long)]
        open: bool,
    },
    /// What Branchyard started or found, with health and owner
    ///
    /// Gateways, proxies, servers, workers, sandboxes and pool keepers, with health, lease
    /// and owner; gc reclaims what expired.
    #[command(display_order = 405, after_help = SERVICES_EXAMPLES)]
    Services {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        /// Only services of this kind, such as connector_gateway or server
        #[arg(long, value_name = "KIND")]
        kind: Option<String>,
        /// Also those that left or were reclaimed (kept for an hour)
        #[arg(long)]
        all: bool,
        #[command(subcommand)]
        action: Option<ServicesAction>,
    },
    /// Sync tasks to durable storage: push and pull
    ///
    /// A branch, or every branch that changed.
    #[command(
        display_order = 407,
        after_help = SYNC_EXAMPLES,
        args_conflicts_with_subcommands = true,
        subcommand_negates_reqs = true
    )]
    Sync(SyncArgs),
    /// Refresh the connector and harness catalogs from live registries
    ///
    /// Or show what is cached.
    #[command(display_order = 406, subcommand_required = true, after_help = CATALOG_EXAMPLES)]
    Catalog {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: CatalogAction,
    },
}

/// `by sync`'s arguments, parsed in a function of their own (it keeps
/// the stack frame of `Command`'s parser small).
#[derive(Args, Clone, Debug, PartialEq, Eq)]
pub struct SyncArgs {
    /// Print JSON
    #[arg(long, global = true)]
    pub json: bool,
    /// A branch, or a task ID (`<repository key>.<branch>`); every branch when omitted
    pub task: Option<String>,
    #[command(subcommand)]
    pub action: Option<SyncAction>,
}

/// `by sync ...`; see docs/sync.md.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum SyncAction {
    /// The remote, this machine's device name, each task's state and lag, what is queued, and
    /// the counters
    Status,
    /// Bring a task from the remote: create or fast-forward its refs here
    Pull {
        /// A branch, or a task ID from `by sync ls`
        task: String,
    },
    /// The tasks in the remote
    Ls,
    /// Collect garbage: unreferenced objects and tasks past retention
    ///
    /// Collect garbage: objects no manifest references, after their grace period; tasks
    /// past their retention unless held
    Gc {
        /// Say what would be deleted, and delete nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Read a sample of objects back and check each against its name; repair chunks from here
    Scrub {
        /// Objects to read (all of them when larger than their number)
        #[arg(long, default_value_t = 100)]
        sample: usize,
        /// The sample's seed, to repeat one
        #[arg(long)]
        seed: Option<u64>,
    },
    /// Put a task on legal hold (never deleted or collected), or release it
    Hold {
        /// A branch, or a task ID
        task: String,
        /// Why, recorded with the hold
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
        /// Release the hold instead
        #[arg(long)]
        release: bool,
    },
    /// Delete a task from the remote (refused under a legal hold); `gc` reclaims its objects
    Rm {
        /// A branch, or a task ID
        task: String,
    },
    /// Rotate the tenant key
    ///
    /// Rotate the tenant key: a new key version, every object's data key rewrapped, the old
    /// versions retired
    RotateKey {
        /// Wrap the keys from now on with this instead: `passphrase` (the new one in
        /// BRANCHYARD_SYNC_NEW_PASSPHRASE) or a kms:// URL
        #[arg(long, value_name = "WRAPPER")]
        to: Option<String>,
    },
}

/// `by services ...`; see docs/registry.md.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum ServicesAction {
    /// Reclaim the services whose lease ran out or whose owner is gone
    ///
    /// Expire services whose lease ran out or whose owner is gone, and reclaim what
    /// Branchyard started for them
    Gc,
}

/// `by models ...`; see docs/model-gateway.md.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum ModelsAction {
    /// Ask each provider which models your keys can call
    ///
    /// Ask each provider whose key you have which models it serves, cache the answer, and
    /// mark the models no price covers
    #[command(after_help = MODELS_LIST_EXAMPLES)]
    List {
        /// Ask the providers again instead of showing the cache
        #[arg(long)]
        refresh: bool,
        /// A harness in the catalog: its static list of models beside what its key's provider lists
        #[arg(long, value_name = "ID")]
        harness: Option<String>,
        /// Gemini's API base URL
        #[arg(
            long,
            value_name = "URL",
            env = "BRANCHYARD_GEMINI_URL",
            default_value = "https://generativelanguage.googleapis.com"
        )]
        gemini_url: String,
        /// OpenAI's API base URL
        #[arg(
            long,
            value_name = "URL",
            env = "BRANCHYARD_OPENAI_URL",
            default_value = "https://api.openai.com"
        )]
        openai_url: String,
        /// Anthropic's API base URL
        #[arg(
            long,
            value_name = "URL",
            env = "BRANCHYARD_ANTHROPIC_URL",
            default_value = "https://api.anthropic.com"
        )]
        anthropic_url: String,
    },
}

/// `by catalog ...`; see docs/registry.md.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum CatalogAction {
    /// Refresh the connector and harness catalogs from live registries
    ///
    /// Fetch the official MCP registry's servers and the npm registry's latest harness
    /// versions, verify them and cache them with their checksums
    Refresh {
        /// The MCP registry's base URL
        #[arg(
            long,
            value_name = "URL",
            env = "BRANCHYARD_MCP_REGISTRY",
            default_value = "https://registry.modelcontextprotocol.io"
        )]
        mcp_registry: String,
        /// The npm registry's base URL
        #[arg(
            long,
            value_name = "URL",
            env = "BRANCHYARD_NPM_REGISTRY",
            default_value = "https://registry.npmjs.org"
        )]
        npm_registry: String,
        /// The most pages of the MCP registry to read (100 servers each)
        #[arg(long, value_name = "N", default_value = "20")]
        max_pages: usize,
        /// Only the connectors, or only the harnesses
        #[arg(long, value_name = "WHICH", value_parser = ["connectors", "harnesses"])]
        only: Option<String>,
    },
    /// What the cache holds, where it came from, and whether its checksums verify
    Status,
}

/// `by gateway ...`; see docs/connectors.md.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum GatewayAction {
    /// Start the gateway, supervised, in the background (or in this terminal)
    Start {
        /// Run in this terminal until interrupted, restarting the gateway if it exits
        #[arg(long)]
        foreground: bool,
    },
    /// Stop the gateway started in the background
    Stop,
    /// Whether the gateway runs and listens, what it serves, and the signing keys
    Status,
    /// Put a new signing key first; tokens the previous one signed stay valid until they expire
    RotateKey {
        /// Older keys to keep publishing
        #[arg(long, value_name = "N", default_value = "1")]
        keep: usize,
    },
    /// Print the public keys the gateway verifies tokens against (JWKS)
    Jwks,
}

/// `by harnesses ACTION`.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum HarnessesAction {
    /// Install a harness with the catalog's command, then check it
    ///
    /// Install a harness with the catalog's command, when [harnesses] install allows, then
    /// check it is there
    Install(HarnessChange),
    /// Install a harness again at a newer or pinned version, then check it
    Update(HarnessChange),
    /// Log in to a harness once, through its own flow, or store its API key
    Login {
        /// The harness's ID, as `by harnesses --all` lists it
        id: String,
        /// Store an API key, read from standard input, for the harness's key variable instead
        /// (in a 0600 file beside your user configuration, named in its [secrets])
        #[arg(long)]
        api_key: bool,
        /// Answer yes to writing a setting the login needs (Antigravity's modelProvider, when
        /// GEMINI_API_KEY is set)
        #[arg(long)]
        yes: bool,
    },
    /// Every install, update and login recorded on this machine
    Log,
}

/// What `by harnesses install|update` takes.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct HarnessChange {
    /// The harness's ID, as `by harnesses --all` lists it
    pub id: String,
    /// The version to install (default: the version its profile was checked against, where
    /// the install command can take one)
    #[arg(long, value_name = "VERSION")]
    pub version: Option<String>,
    /// Install without asking, where [harnesses] install = "ask"
    #[arg(long)]
    pub yes: bool,
}

/// `by connectors ...`.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum ConnectorsAction {
    /// Every connector in catalog/connectors.toml: kind, authentication, credential names
    Catalog,
}

/// `by workspace ...`; see docs/workspace.md.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceAction {
    /// The effective [workspace], where it came from, whether it is trusted, and ports
    Show {
        /// One branch's workspace: what it was created with, its setup, its port
        branch: Option<String>,
    },
    /// Trust this repository's [workspace] scripts as they are now
    Trust,
    /// Forget the trust decision for this repository
    Untrust,
    /// Run a named [workspace.run] script in a branch's worktree; with --detach, several at once
    Run {
        /// The branch (default: $BRANCHYARD_BRANCH inside a harness), then the
        /// script's name (default: the one marked default, or the only one); with --detach,
        /// several names start each its own script, each with its own port
        #[arg(value_name = "BRANCH [NAME...]")]
        args: Vec<String>,
        /// Return as soon as the script has started, leaving it running in the background; its
        /// output goes to a log file
        #[arg(long)]
        detach: bool,
    },
    /// The TCP ports a branch's processes listen on (default: every branch's)
    Ports {
        /// One branch (default: every branch with a worktree here)
        branch: Option<String>,
    },
    /// Open a port a branch listens on in a browser ($BROWSER, xdg-open or open)
    Browse {
        /// The branch (default: $BRANCHYARD_BRANCH inside a harness)
        branch: Option<String>,
        /// The port (default: the branch's only one, or its reserved BRANCHYARD_PORT)
        #[arg(long)]
        port: Option<u16>,
        /// Print the URL instead of opening it
        #[arg(long)]
        print: bool,
    },
    /// Stop the processes listening on a branch's ports (SIGTERM)
    Kill {
        /// The branch (default: $BRANCHYARD_BRANCH inside a harness)
        branch: Option<String>,
        /// Only the process listening on this port
        #[arg(long)]
        port: Option<u16>,
        /// Stop the process without a confirmation prompt
        #[arg(long)]
        yes: bool,
    },
}

/// `by env ...`; see docs/environments.md.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum EnvAction {
    /// Every prepared environment and recorded failure, newest first
    List,
    /// One environment: its inputs, setup, what it holds (default: the current key)
    Show {
        /// A key, or the start of one
        key: Option<String>,
    },
    /// Build the current key's environment now from HEAD; a failure keeps the last good one
    Rebuild,
    /// Remove old environments; the newest of each recipe and linked ones stay
    Prune {
        /// Only these keys (or starts of keys), even the newest
        #[arg(value_name = "KEY")]
        keys: Vec<String>,
        /// Good environments kept per recipe (default 3)
        #[arg(long, value_name = "N")]
        keep: Option<usize>,
        /// Remove those unused for this many days (default 14)
        #[arg(long, value_name = "DAYS")]
        older_than: Option<u64>,
    },
    /// The warm pool of ready worktrees (`[workspace.pool]`); see docs/pools.md
    #[command(subcommand)]
    Pool(PoolAction),
}

/// `by env pool ...`; see docs/pools.md.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum PoolAction {
    /// The pool's slots: ready, being made, being claimed
    Status,
    /// Make the pool full again, replacing stale slots
    ///
    /// Discard stale slots and make new ones until the pool is full (runs setup when the
    /// environment is not built)
    Fill,
    /// Remove every ready slot on this host
    Drain,
}

/// `by config ...`.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum ConfigAction {
    /// Every effective value and where it came from
    Show,
    /// The user and project files, and whether they exist
    Path,
    /// Load FILE, or every file by reads, strictly
    Validate {
        /// One file to check (default: the user and project files, and their merge)
        #[arg(value_hint = ValueHint::FilePath)]
        file: Option<String>,
    },
    /// The JSON Schema of the file (schema/branchyard.config.json)
    Schema,
}

/// Which step of the JSON protocol `by init` takes; none runs the wizard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitStep {
    /// `--next`: the next batch of questions, or the plan.
    Next,
    /// `--dry-run`: the plan, writing nothing.
    DryRun,
    /// `--apply`: write the plan.
    Apply,
}

/// `by init`, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitArgs {
    pub topic: Option<branchyard_setup::Topic>,
    pub step: Option<InitStep>,
    pub json: bool,
    /// `--answers FILE`, or `-` for stdin.
    pub answers: Option<String>,
    pub defaults: bool,
    pub force: bool,
    /// `by init plugin ID...`: the harnesses to install for.
    pub harnesses: Vec<String>,
    /// `by init plugin --all-detected`.
    pub all_detected: bool,
    /// `by init plugin --status`: what is installed, and what is left to do.
    pub status: bool,
}

/// `by init`'s options. At most one step; a step needs a topic; `--answers`
/// needs a step and `--force` needs `--apply`: all clap's to enforce.
#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(group(ArgGroup::new("step").args(["next", "dry_run", "apply"]).requires("topic")))]
pub struct InitFlags {
    /// What to set up (default: the wizard asks)
    #[arg(value_name = "TOPIC", value_parser = TopicParser)]
    topic: Option<branchyard_setup::Topic>,
    /// With plugin: the harnesses to install hooks, commands and skills for (by init plugin lists
    /// them)
    #[arg(value_name = "HARNESS", requires = "topic")]
    harnesses: Vec<String>,
    /// With plugin: every harness found on PATH
    #[arg(long, conflicts_with = "harnesses")]
    all_detected: bool,
    /// With plugin: per harness, whether its install is current, stale or missing, whether `by`
    /// is on PATH for its hooks, and the one-time steps still left to you; writes nothing
    #[arg(long, conflicts_with_all = ["all_detected", "next", "dry_run", "apply"])]
    status: bool,
    /// Print JSON (schema/setup.protocol.json): the topics, or the step's result
    #[arg(long)]
    json: bool,
    /// Print the next batch of at most four questions given the answers so far, or the plan
    /// when none remain
    #[arg(long, help_heading = "Protocol steps (for harnesses and scripts)")]
    next: bool,
    /// Print the plan: files, diffs and validation; write nothing
    #[arg(long, help_heading = "Protocol steps (for harnesses and scripts)")]
    dry_run: bool,
    /// Write the plan; refuses to replace a file that differs
    #[arg(long, help_heading = "Protocol steps (for harnesses and scripts)")]
    apply: bool,
    /// With --apply: replace files that differ (shown as diffs)
    #[arg(
        long,
        requires = "apply",
        // A requirement that conflicts with a given argument is not
        // enforced, so the other steps are refused by name.
        conflicts_with_all = ["next", "dry_run"],
        help_heading = "Protocol steps (for harnesses and scripts)"
    )]
    force: bool,
    /// A JSON object of answers by question id; - reads stdin
    #[arg(
        long,
        value_name = "FILE|-",
        requires = "step",
        value_hint = ValueHint::FilePath,
        help_heading = "Protocol steps (for harnesses and scripts)"
    )]
    answers: Option<String>,
    /// Take the default for every unanswered question
    #[arg(long)]
    defaults: bool,
}

impl Flags for InitFlags {
    type Output = InitArgs;
    fn check(self) -> Result<InitArgs, String> {
        let step = match (self.next, self.dry_run, self.apply) {
            (true, _, _) => Some(InitStep::Next),
            (_, true, _) => Some(InitStep::DryRun),
            (_, _, true) => Some(InitStep::Apply),
            _ => None,
        };
        let plugin = self.topic == Some(branchyard_setup::Topic::Plugin);
        if (!self.harnesses.is_empty() || self.all_detected || self.status) && !plugin {
            return Err("harnesses, --all-detected and --status go with by init plugin".into());
        }
        if self.json && self.topic.is_some() && step.is_none() && !plugin {
            return Err("with --json and a topic, give --next, --dry-run or --apply".into());
        }
        Ok(InitArgs {
            topic: self.topic,
            step,
            json: self.json,
            answers: self.answers,
            defaults: self.defaults,
            force: self.force,
            harnesses: self.harnesses,
            all_detected: self.all_detected,
            status: self.status,
        })
    }
}

/// `by setup`'s options.
#[derive(Args, Clone, Debug, Default, PartialEq, Eq)]
pub struct SetupFlags {
    /// Guide repository selection, compute setup and readiness checks
    #[arg(long)]
    pub guided: bool,
    /// Bring configured prerequisites up and include their readiness in the result
    #[arg(long)]
    pub up: bool,
    /// Set up this repository instead of the current directory
    #[arg(long, value_name = "PATH")]
    pub directory: Option<PathBuf>,
    /// Take every default and ask nothing (the only way to write without a terminal)
    #[arg(long)]
    pub yes: bool,
    /// Show what would be detected, written and installed; write nothing
    #[arg(long)]
    pub dry_run: bool,
    /// Print one JSON object: detected, written, installed, todo, next
    #[arg(long)]
    pub json: bool,
    /// Install hooks only for these harnesses, named even if not on PATH (default: every harness
    /// on PATH that has an install; by init plugin lists them)
    #[arg(
        long = "harness",
        value_name = "ID",
        num_args = 1..,
        value_delimiter = ',',
        conflicts_with = "no_hooks"
    )]
    pub harnesses: Vec<String>,
    /// Do not install hooks, commands or skills into any harness
    #[arg(long)]
    pub no_hooks: bool,
    /// Do not write branchyard.toml
    #[arg(long)]
    pub no_config: bool,
}

/// `by init`'s topics, from [`branchyard_setup::Topic::ALL`]: completed by
/// every shell, listed in the man page, and refused with the list.
#[derive(Clone, Copy, Debug)]
struct TopicParser;

impl TypedValueParser for TopicParser {
    type Value = branchyard_setup::Topic;

    fn parse_ref(
        &self,
        cmd: &clap::Command,
        arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<Self::Value, clap::Error> {
        let known = |text: String| {
            branchyard_setup::Topic::parse(&text).ok_or_else(|| {
                let ids: Vec<&str> = branchyard_setup::Topic::ALL
                    .iter()
                    .map(|t| t.id())
                    .collect();
                format!("unknown topic '{text}'; use {}", ids.join(", "))
            })
        };
        StringValueParser::new()
            .try_map(known)
            .parse_ref(cmd, arg, value)
    }

    fn possible_values(&self) -> Option<Box<dyn Iterator<Item = PossibleValue> + '_>> {
        Some(Box::new(
            branchyard_setup::Topic::ALL
                .into_iter()
                .map(|t| PossibleValue::new(t.id()).help(t.summary())),
        ))
    }
}

/// `by graph ...`.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum GraphAction {
    /// Show a branch's children, their dependencies and the graph's revision
    Show {
        /// Default: this harness's own branch
        branch: Option<String>,
    },
    /// Apply a graph proposal to a branch's children, atomically
    #[command(
        group(clap::ArgGroup::new("proposal").required(true).args(["file", "edits"])),
        after_help = GRAPH_APPLY_HELP
    )]
    Apply {
        /// The proposal, {"expected_revision", "edits"}; - for stdin
        file: Option<String>,
        /// The branch whose graph changes (outside a harness; inside one, it is the
        /// harness's own)
        #[arg(long, value_name = "BRANCH")]
        parent: Option<String>,
        /// The proposal's edits inline, as a JSON array, instead of a file
        #[arg(long, value_name = "JSON", requires = "expected_revision")]
        edits: Option<String>,
        /// The graph revision the proposal was made against (by graph show); overrides the
        /// file's
        #[arg(long, value_name = "N")]
        expected_revision: Option<u64>,
        #[command(flatten)]
        perms: Perms,
    },
    /// Start dependents whose prerequisites settled while no engine was running
    Resume {
        #[command(flatten)]
        perms: Perms,
    },
}

impl GraphAction {
    pub fn into_args(self, json: bool) -> GraphArgs {
        let mut args = GraphArgs {
            json,
            ..GraphArgs::default()
        };
        let perms = match self {
            GraphAction::Show { branch } => {
                args.action = "show".into();
                args.arg = branch;
                return args;
            }
            GraphAction::Apply {
                file,
                parent,
                edits,
                expected_revision,
                perms,
            } => {
                args.action = "apply".into();
                args.arg = file;
                args.parent = parent;
                args.edits = edits;
                args.expected_revision = expected_revision;
                perms
            }
            GraphAction::Resume { perms } => {
                args.action = "resume".into();
                perms
            }
        };
        perms.apply(&mut args.task);
        args
    }
}

/// `by rig ...`.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum RigAction {
    /// Check a rig spec and print its plan
    Check {
        /// The rig spec, TOML (see docs/rigs.md)
        file: String,
    },
    /// Run a rig's root seat with a prompt
    Run {
        /// The rig spec, TOML (see docs/rigs.md)
        file: String,
        /// The root seat's task; quote it
        #[arg(value_parser = prompt_text)]
        prompt: String,
        /// The root branch's name (default: the rig's)
        #[arg(short, long)]
        name: Option<String>,
        /// Base revision (default: HEAD)
        #[arg(short, long, value_name = "REV")]
        base: Option<String>,
        /// Launch this instead of the root's executable, for development and testing
        #[arg(long, value_name = "CMD", value_parser = command_argv)]
        command: Option<Argv>,
        /// Run a profile that does not route tool permission requests to Branchyard
        #[arg(long)]
        allow_unapproved_tools: bool,
    },
}

impl RigAction {
    pub fn into_args(self, json: bool) -> RigArgs {
        match self {
            RigAction::Check { file } => RigArgs {
                file,
                json,
                ..RigArgs::default()
            },
            RigAction::Run {
                file,
                prompt,
                name,
                base,
                command,
                allow_unapproved_tools,
            } => RigArgs {
                prompt: Some(prompt),
                file,
                name,
                base,
                command: command.map(|argv| argv.0),
                unapproved_tools: allow_unapproved_tools,
                json,
            },
        }
    }
}

/// `by artifact ...`.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum ArtifactAction {
    /// Publish a file as a new artifact of the acting branch
    Publish {
        /// The file to publish
        file: String,
        /// The artifact's name (default: the file's)
        #[arg(short, long)]
        name: Option<String>,
        /// The artifact's media type (default: application/octet-stream)
        #[arg(long, value_name = "TYPE")]
        media_type: Option<String>,
        /// A label on the artifact; repeatable
        #[arg(long = "label", value_name = "KEY=VALUE", value_parser = label)]
        labels: Vec<(String, String)>,
    },
    /// List the artifacts the acting branch may read
    List,
    /// Write an artifact's bytes to a file
    Get {
        /// The artifact's ID, as `by artifact list` shows it
        id: String,
        /// Where to write the artifact's bytes
        #[arg(short, long, value_name = "PATH")]
        out: String,
    },
    /// Let another branch read an artifact
    Share {
        /// The artifact's ID, as `by artifact list` shows it
        id: String,
        /// The branch to share with
        #[arg(long, value_name = "BRANCH")]
        to: String,
    },
    /// Write artifacts into one verified, deterministic tar bundle (local only)
    Export {
        #[arg(required = true)]
        /// The artifacts' IDs, as `by artifact list` shows them
        ids: Vec<String>,
        /// Where to write the bundle's tar
        #[arg(short, long, value_name = "PATH")]
        out: String,
    },
    /// Publish every artifact of a bundle as new artifacts (local only)
    Import {
        /// A bundle from 'by artifact export'
        file: String,
    },
}

impl ArtifactAction {
    pub fn into_args(self, branch: Option<String>, json: bool) -> ArtifactArgs {
        let mut args = ArtifactArgs {
            branch,
            json,
            ..ArtifactArgs::default()
        };
        args.action = match self {
            ArtifactAction::Publish {
                file,
                name,
                media_type,
                labels,
            } => {
                args.arg = Some(file);
                args.name = name;
                args.media_type = media_type;
                args.labels = labels;
                "publish"
            }
            ArtifactAction::List => "list",
            ArtifactAction::Get { id, out } => {
                args.arg = Some(id);
                args.out = Some(out);
                "get"
            }
            ArtifactAction::Share { id, to } => {
                args.arg = Some(id);
                args.to = Some(to);
                "share"
            }
            ArtifactAction::Export { ids, out } => {
                args.arg = ids.first().cloned();
                args.ids = ids;
                args.out = Some(out);
                "export"
            }
            ArtifactAction::Import { file } => {
                args.arg = Some(file);
                "import"
            }
        }
        .into();
        args
    }
}

/// `by scratch ...`.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum ScratchAction {
    /// Create a scratch area owned by the acting branch
    Create {
        /// The scratch area's name
        name: String,
    },
    /// List the scratch areas the acting branch may reach
    List,
    /// Take a scratch area's writer lock
    Lock {
        /// The scratch area's name
        name: String,
    },
    /// Release a scratch area's writer lock
    Unlock {
        /// The scratch area's name
        name: String,
    },
    /// Let another branch reach a scratch area
    Share {
        /// The scratch area to share
        name: String,
        /// The branch to share with
        #[arg(long, value_name = "BRANCH")]
        to: String,
    },
}

impl ScratchAction {
    pub fn into_args(self, branch: Option<String>, json: bool) -> ScratchArgs {
        let (action, name, to) = match self {
            ScratchAction::Create { name } => ("create", Some(name), None),
            ScratchAction::List => ("list", None, None),
            ScratchAction::Lock { name } => ("lock", Some(name), None),
            ScratchAction::Unlock { name } => ("unlock", Some(name), None),
            ScratchAction::Share { name, to } => ("share", Some(name), Some(to)),
        };
        ScratchArgs {
            action: action.into(),
            name,
            to,
            branch,
            json,
        }
    }
}

impl Command {
    /// The recipe `--provider recipe:NAME` names, for commands that take
    /// it: a server never runs one, so remote mode refuses it before
    /// connecting.
    pub fn recipe(&self) -> Option<&str> {
        let task: &TaskArgs = match self {
            Command::Run { task, .. } => task,
            Command::Fan { task, .. } => task,
            Command::Fork { task, .. } => task,
            Command::Reincarnate { task, .. } => task,
            Command::Send { task, .. } => task,
            Command::Review { task, .. } => task,
            Command::Spawn { spawn, .. } => &spawn.task,
            Command::Task(task) => match &task.action {
                TaskAction::New(new) => &new.task,
                TaskAction::Fork(fork) => &fork.flags,
                _ => return None,
            },
            _ => return None,
        };
        task.recipe.as_ref().map(|r| r.name.as_str())
    }
}

/// Flags whose combination is checked after clap parses them, producing
/// [`Flags::Output`]; flattened into a command as [`Checked<Self>`].
pub trait Flags: Args + FromArgMatches {
    type Output;
    /// The checked value, or the usage error's message.
    fn check(self) -> Result<Self::Output, String>;
}

/// `F`, checked while parsing: a failed [`Flags::check`] is a usage error
/// like any other. Dereferences to the checked value.
pub struct Checked<F: Flags>(F::Output, PhantomData<fn() -> F>);

impl<F: Flags> Checked<F> {
    pub fn new(output: F::Output) -> Checked<F> {
        Checked(output, PhantomData)
    }

    #[cfg(test)]
    pub fn into_inner(self) -> F::Output {
        self.0
    }
}

impl<F: Flags> Deref for Checked<F> {
    type Target = F::Output;
    fn deref(&self) -> &F::Output {
        &self.0
    }
}

/// `branchyard.toml` defaults fill what the flags left unset
/// (`crate::defaults`).
impl<F: Flags> DerefMut for Checked<F> {
    fn deref_mut(&mut self) -> &mut F::Output {
        &mut self.0
    }
}

impl<F: Flags> Clone for Checked<F>
where
    F::Output: Clone,
{
    fn clone(&self) -> Self {
        Checked::new(self.0.clone())
    }
}

impl<F: Flags> fmt::Debug for Checked<F>
where
    F::Output: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<F: Flags> PartialEq for Checked<F>
where
    F::Output: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<F: Flags> FromArgMatches for Checked<F> {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, clap::Error> {
        let flags = F::from_arg_matches(matches)?;
        flags
            .check()
            .map(Checked::new)
            .map_err(|message| clap::Error::raw(ErrorKind::ArgumentConflict, message))
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), clap::Error> {
        *self = Self::from_arg_matches(matches)?;
        Ok(())
    }
}

impl<F: Flags> Args for Checked<F> {
    fn group_id() -> Option<clap::Id> {
        F::group_id()
    }

    fn augment_args(cmd: clap::Command) -> clap::Command {
        F::augment_args(cmd)
    }

    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        F::augment_args_for_update(cmd)
    }
}

/// An argument vector split from one string like a shell (`--check`,
/// `--command`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Argv(pub Vec<String>);

/// A comma-separated list, trimmed, without empty or repeated entries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct List(pub Vec<String>);

impl Deref for List {
    type Target = Vec<String>;
    fn deref(&self) -> &Vec<String> {
        &self.0
    }
}

/// `--stall-action`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum StallActionArg {
    /// Record the stall and keep running (default)
    Notify,
    /// Record the stall, then interrupt the turn
    Interrupt,
}

impl From<StallActionArg> for branchyard::StallAction {
    fn from(action: StallActionArg) -> Self {
        match action {
            StallActionArg::Notify => branchyard::StallAction::Notify,
            StallActionArg::Interrupt => branchyard::StallAction::Interrupt,
        }
    }
}

/// `--after`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum AfterArg {
    /// Its last turn ended ready or no_changes, or it was merged (default)
    Settled,
    /// It was integrated into the parent
    Integrated,
}

impl From<AfterArg> for branchyard::After {
    fn from(after: AfterArg) -> Self {
        match after {
            AfterArg::Settled => branchyard::After::Settled,
            AfterArg::Integrated => branchyard::After::Integrated,
        }
    }
}

/// `--provider`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderArg {
    /// A process on this host (the default for a new branch)
    Local,
    /// A Microsandbox microVM; needs --image
    Microsandbox,
    /// An Agent Substrate actor; needs the --substrate-* endpoints and key
    Substrate,
    /// The machine an environment recipe makes (`recipe:NAME`)
    Recipe(String),
}

/// `local`, `microsandbox`, `substrate` or `recipe:NAME`.
fn provider_arg(value: &str) -> Result<ProviderArg, String> {
    match value {
        "local" => Ok(ProviderArg::Local),
        "microsandbox" => Ok(ProviderArg::Microsandbox),
        "substrate" => Ok(ProviderArg::Substrate),
        other => match other.strip_prefix("recipe:") {
            Some(name) if branchyard_setup::config::valid_recipe_name(name) => {
                Ok(ProviderArg::Recipe(name.to_owned()))
            }
            Some(name) => Err(format!(
                "{name:?} is not a recipe name (1 to 64 of a-z, 0-9, '.', '_' and '-', \
                 starting with a letter or digit)"
            )),
            None => Err(format!(
                "{other:?} is not a provider [possible values: local, microsandbox, substrate, \
                 recipe:NAME]"
            )),
        },
    }
}

/// `--check`, the budget, turn and time limits, and stall detection.
#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(next_help_heading = "Checks and limits")]
pub struct Limits {
    /// Command that must pass before the branch merges, such as "cargo test": split into words
    /// like a shell does, then run directly, not through a shell (use `sh -c "..."` for pipes)
    #[arg(long, value_name = "CMD", value_parser = check_argv)]
    check: Option<Argv>,
    /// Stop once the harness's own cost estimate, or on the model gateway its metered cost,
    /// exceeds this many dollars
    #[arg(long, value_name = "DOLLARS", value_parser = usd, allow_negative_numbers = true)]
    budget_usd: Option<f64>,
    /// Stop after N turns
    #[arg(long, value_name = "N", value_parser = positive_turns, allow_negative_numbers = true)]
    max_turns: Option<u32>,
    /// Interrupt the turn after this many minutes (or a duration such as 90s or 2h)
    #[arg(long, value_name = "MINUTES", value_parser = max_minutes, allow_negative_numbers = true)]
    max_minutes: Option<Duration>,
    /// Mark the branch stalled after this many minutes with no harness activity (or 90s, 2h, ...)
    #[arg(long, value_name = "MINUTES", value_parser = stall_after, allow_negative_numbers = true)]
    stall_after: Option<Duration>,
    /// What a stall does
    #[arg(long, value_name = "ACTION", requires = "stall_after")]
    stall_action: Option<StallActionArg>,
}

impl Limits {
    fn apply(self, task: &mut TaskArgs) {
        task.check = self.check.map(|argv| argv.0);
        task.budget_usd = self.budget_usd;
        task.max_turns = self.max_turns;
        task.max_duration = self.max_minutes;
        task.stall_after = self.stall_after;
        task.stall_action = self.stall_action.map(Into::into).unwrap_or_default();
    }
}

/// How tool permission requests are answered.
#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(next_help_heading = "Permissions")]
pub struct Perms {
    /// Grant every tool permission request without asking; the harness may write files and run
    /// commands
    #[arg(short, long, conflicts_with = "ask")]
    yes: bool,
    /// Ask on the terminal for each tool permission request
    #[arg(long)]
    ask: bool,
    /// Answer tool permission requests by a named preset: read-only, edit-worktree, or full
    /// (as --yes); see docs/egress.md#permission-presets
    #[arg(
        long,
        value_name = "PRESET",
        value_parser = branchyard::PolicyPreset::parse,
        conflicts_with_all = ["yes", "ask"]
    )]
    permissions: Option<branchyard::PolicyPreset>,
    /// Run a profile that does not route tool permission requests to Branchyard (Antigravity,
    /// Pi, Amp); its tools run under the harness's own configuration
    #[arg(long)]
    allow_unapproved_tools: bool,
}

impl Perms {
    pub(crate) fn apply(self, task: &mut TaskArgs) {
        task.permissions = match (self.yes, self.ask, self.permissions) {
            (true, _, _) => Permissions::Yes,
            (false, true, _) => Permissions::Ask,
            (false, false, Some(preset)) => Permissions::Preset(preset),
            (false, false, None) => Permissions::Unset,
        };
        task.unapproved_tools = self.allow_unapproved_tools;
    }
}

/// `--no-preflight`, for the commands that start a harness turn in this
/// repository (`run`, `fan`, `send`).
#[derive(Args, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[command(next_help_heading = "Launch")]
pub struct Preflight {
    /// Skip the check, before any branch is created, that the harness is installed and not
    /// verified logged out (it is not made for --command or a routed run)
    #[arg(long)]
    no_preflight: bool,
}

impl Preflight {
    fn apply(self, task: &mut TaskArgs) {
        task.no_preflight = self.no_preflight;
    }
}

/// Where and how the harness is launched.
#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(next_help_heading = "Launch")]
pub struct Launch {
    /// Private HOME, not a sandbox; the harness is then usually not logged in
    ///
    /// A private HOME at .branchyard/homes/BRANCH, without the ANTHROPIC*, CLAUDE*, OPENAI*
    /// and CODEX* variables. It is not a sandbox: the harness still runs as your user and can
    /// read your files and reach the network (--provider chooses a sandbox; see
    /// docs/modes.md). --isolated=false turns off [defaults] isolated = true for this run.
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    isolated: Option<bool>,
    /// Launch this instead of the profile's executable, for development and testing
    #[arg(long, value_name = "CMD", value_parser = command_argv)]
    command: Option<Argv>,
    /// Where the harness runs: local, microsandbox, substrate or recipe:NAME (default: local,
    /// or the branch's own)
    #[arg(long, value_name = "PROVIDER", value_parser = provider_arg)]
    provider: Option<ProviderArg>,
    /// Variables to copy into the sandbox, such as API keys; nothing else is
    #[arg(long, value_name = "NAME,NAME,...", value_parser = variable_names)]
    pass_env: Option<List>,
    #[command(flatten)]
    microsandbox: MicrosandboxFlags,
    #[command(flatten)]
    substrate: SubstrateFlags,
    #[command(flatten)]
    recipe: RecipeFlags,
    #[command(flatten)]
    lifecycle: LifecycleFlags,
    /// With --remote: only a worker carrying this label runs it (repeatable)
    #[arg(long = "require-label", value_name = "LABEL")]
    require_label: Vec<String>,
    /// With --remote: the operation's priority, -10 to 10 (default 0, or a spawned child's
    /// parent's); higher runs first
    #[arg(
        long,
        value_name = "N",
        allow_negative_numbers = true,
        value_parser = clap::value_parser!(i32).range(-10..=10)
    )]
    priority: Option<i32>,
}

#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(next_help_heading = "Recipe provider (--provider recipe:NAME)")]
pub struct RecipeFlags {
    /// Where the worktree is copied on the machine (default:
    /// /tmp/branchyard/<branch>-<hash>/workspace)
    #[arg(long, value_name = "PATH")]
    recipe_workdir: Option<String>,
    /// The harness's HOME on the machine (default: beside the workdir)
    #[arg(long, value_name = "PATH")]
    recipe_home: Option<String>,
}

#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(
    next_help_heading = "Sandbox lifecycle (--provider microsandbox, substrate or recipe:NAME)"
)]
pub struct LifecycleFlags {
    /// Between turns: destroy the sandbox (default) or pause it for the next turn
    #[arg(long, value_name = "WHAT", value_parser = keep_sandbox)]
    keep_sandbox: Option<branchyard::SandboxKeep>,
    /// Checkpoints that also keep a provider snapshot, with a kept sandbox (default 3)
    #[arg(long, value_name = "N")]
    sandbox_snapshots: Option<u32>,
    /// Kept sandboxes of this provider in the repository before the least recently used
    /// is destroyed (default 4)
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    max_paused: Option<u32>,
    /// Minutes a kept sandbox may go unused before it is destroyed instead of resumed
    /// (default: never)
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    max_kept_minutes: Option<u32>,
}

impl LifecycleFlags {
    fn first_given(&self) -> Option<&'static str> {
        [
            ("keep-sandbox", self.keep_sandbox.is_some()),
            ("sandbox-snapshots", self.sandbox_snapshots.is_some()),
            ("max-paused", self.max_paused.is_some()),
            ("max-kept-minutes", self.max_kept_minutes.is_some()),
        ]
        .into_iter()
        .find_map(|(flag, given)| given.then_some(flag))
    }

    fn args(&self) -> LifecycleArgs {
        LifecycleArgs {
            keep: self.keep_sandbox,
            snapshots: self.sandbox_snapshots,
            max_paused: self.max_paused,
            max_kept_minutes: self.max_kept_minutes,
        }
    }
}

fn keep_sandbox(value: &str) -> Result<branchyard::SandboxKeep, String> {
    match value {
        "pause" => Ok(branchyard::SandboxKeep::Pause),
        "destroy" => Ok(branchyard::SandboxKeep::Destroy),
        other => Err(format!("{other:?} is not pause or destroy")),
    }
}

#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(next_help_heading = "Microsandbox provider (--provider microsandbox)")]
pub struct MicrosandboxFlags {
    /// OCI image with the harness installed
    #[arg(long, value_name = "REF")]
    image: Option<String>,
    /// Virtual CPUs for the sandbox
    #[arg(long, value_name = "N", value_parser = cpus)]
    cpus: Option<u8>,
    /// Sandbox memory in MiB
    #[arg(long, value_name = "MIB", value_parser = memory)]
    memory: Option<u32>,
    /// Use the SDK's pause, live branching and full snapshots (unqualified; needs KVM)
    #[arg(long)]
    live_branch: bool,
}

#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(next_help_heading = "Substrate provider (--provider substrate)")]
pub struct SubstrateFlags {
    /// Agent Substrate Control API, https://HOST:PORT
    #[arg(long, value_name = "URL")]
    substrate_endpoint: Option<String>,
    /// Router base URL; requests identify the actor with its routing header
    #[arg(long, value_name = "URL")]
    substrate_router: Option<String>,
    /// Actor template that runs branchyard-bridge and the harness
    #[arg(long, value_name = "NAME")]
    substrate_template: Option<String>,
    /// Bridge signing key from `branchyard-bridge keygen`
    #[arg(long, value_name = "FILE")]
    substrate_key: Option<String>,
    /// File holding the Control bearer token, reread for every request
    #[arg(long, value_name = "FILE")]
    substrate_api_token_file: Option<String>,
    /// Verified TLS service name for Control, including port-forwards
    #[arg(long, value_name = "NAME")]
    substrate_server_name: Option<String>,
    /// Actor egress policy (default: deny); use --substrate-allow-host for an allowlist
    #[arg(long, value_parser = ["deny", "unrestricted"], conflicts_with = "substrate_allow_host")]
    substrate_egress: Option<String>,
    /// DNS host the actor may reach; repeat for more hosts, *. matches one label
    #[arg(long, value_name = "HOST")]
    substrate_allow_host: Vec<String>,
    /// Atespace for the actors (default: default)
    #[arg(long, value_name = "NAME")]
    substrate_atespace: Option<String>,
    /// Where the worktree is copied in the actor (default: /workspace)
    #[arg(long, value_name = "PATH")]
    substrate_workdir: Option<String>,
    /// The harness's HOME in the actor (default: /branchyard/home)
    #[arg(long, value_name = "PATH")]
    substrate_home: Option<String>,
    /// PEM authorities for the TLS Control API, and the router
    #[arg(long, value_name = "FILE")]
    substrate_ca: Option<String>,
    /// PEM client certificate for the Control API, mutual TLS
    #[arg(long, value_name = "FILE")]
    substrate_client_cert: Option<String>,
    /// PEM key of --substrate-client-cert
    #[arg(long, value_name = "FILE")]
    substrate_client_key: Option<String>,
    /// PEM authorities for the TLS router, if not --substrate-ca
    #[arg(long, value_name = "FILE")]
    substrate_router_ca: Option<String>,
    /// Allow http:// or ws:// to hosts other than loopback
    #[arg(long)]
    substrate_insecure: bool,
}

impl SubstrateFlags {
    /// The first substrate flag given, for "needs --provider substrate".
    fn first_given(&self) -> Option<&'static str> {
        [
            ("substrate-endpoint", self.substrate_endpoint.is_some()),
            ("substrate-router", self.substrate_router.is_some()),
            ("substrate-template", self.substrate_template.is_some()),
            ("substrate-key", self.substrate_key.is_some()),
            (
                "substrate-api-token-file",
                self.substrate_api_token_file.is_some(),
            ),
            (
                "substrate-server-name",
                self.substrate_server_name.is_some(),
            ),
            ("substrate-egress", self.substrate_egress.is_some()),
            (
                "substrate-allow-host",
                !self.substrate_allow_host.is_empty(),
            ),
            ("substrate-atespace", self.substrate_atespace.is_some()),
            ("substrate-workdir", self.substrate_workdir.is_some()),
            ("substrate-home", self.substrate_home.is_some()),
            ("substrate-ca", self.substrate_ca.is_some()),
            (
                "substrate-client-cert",
                self.substrate_client_cert.is_some(),
            ),
            ("substrate-client-key", self.substrate_client_key.is_some()),
            ("substrate-router-ca", self.substrate_router_ca.is_some()),
            ("substrate-insecure", self.substrate_insecure),
        ]
        .into_iter()
        .find_map(|(flag, given)| given.then_some(flag))
    }
}

impl Launch {
    fn apply(self, task: &mut TaskArgs) -> Result<(), String> {
        task.isolated = self.isolated == Some(true);
        task.isolation_off = self.isolated == Some(false);
        task.require_labels = self.require_label.clone();
        task.priority = self.priority;
        task.command = self.command.map(|argv| argv.0);
        let chosen = self.provider;
        let micro = &self.microsandbox;
        let recipe = matches!(chosen, Some(ProviderArg::Recipe(_)));
        if !recipe {
            let given = [
                ("recipe-workdir", self.recipe.recipe_workdir.is_some()),
                ("recipe-home", self.recipe.recipe_home.is_some()),
            ]
            .into_iter()
            .find_map(|(flag, given)| given.then_some(flag));
            if let Some(flag) = given {
                return Err(format!("--{flag} needs --provider recipe:NAME"));
            }
        } else if self.lifecycle.sandbox_snapshots.is_some() {
            return Err(
                "--sandbox-snapshots does not apply to a recipe's machine, which has no \
                 snapshots"
                    .into(),
            );
        }
        if chosen != Some(ProviderArg::Microsandbox) {
            let given = [
                ("image", micro.image.is_some()),
                ("cpus", micro.cpus.is_some()),
                ("memory", micro.memory.is_some()),
                ("live-branch", micro.live_branch),
            ]
            .into_iter()
            .find_map(|(flag, given)| given.then_some(flag));
            if let Some(flag) = given {
                return Err(format!("--{flag} needs --provider microsandbox"));
            }
        }
        if chosen != Some(ProviderArg::Substrate) {
            if let Some(flag) = self.substrate.first_given() {
                return Err(format!("--{flag} needs --provider substrate"));
            }
        }
        if matches!(chosen, None | Some(ProviderArg::Local)) && self.pass_env.is_some() {
            return Err(
                "--pass-env needs --provider microsandbox, substrate or recipe:NAME".into(),
            );
        }
        if matches!(chosen, None | Some(ProviderArg::Local)) {
            if let Some(flag) = self.lifecycle.first_given() {
                return Err(format!(
                    "--{flag} needs --provider microsandbox, substrate or recipe:NAME"
                ));
            }
        }
        let lifecycle = self.lifecycle.args();
        let pass_env = self.pass_env.map(|list| list.0).unwrap_or_default();
        match chosen {
            None => {}
            Some(ProviderArg::Local) => task.local = true,
            Some(ProviderArg::Recipe(name)) => {
                let path = |value: Option<String>, flag: &str| match value {
                    Some(path) if !path.starts_with('/') => Err(format!(
                        "--{flag} is a path on the recipe's machine and must be absolute, not \
                         {path:?}"
                    )),
                    other => Ok(other),
                };
                task.recipe = Some(RecipeArgs {
                    name,
                    workdir: path(self.recipe.recipe_workdir, "recipe-workdir")?,
                    home: path(self.recipe.recipe_home, "recipe-home")?,
                    pass_env,
                    lifecycle,
                });
            }
            Some(ProviderArg::Microsandbox) => {
                let image = self
                    .microsandbox
                    .image
                    .filter(|image| !image.trim().is_empty())
                    .ok_or("--provider microsandbox needs --image")?;
                task.sandbox = Some(SandboxArgs {
                    image,
                    cpus: self.microsandbox.cpus,
                    memory_mib: self.microsandbox.memory,
                    pass_env,
                    lifecycle,
                    live_branch: self.microsandbox.live_branch,
                });
            }
            Some(ProviderArg::Substrate) => {
                let s = self.substrate;
                let required = |value: Option<String>, flag: &str| {
                    value
                        .filter(|value| !value.trim().is_empty())
                        .ok_or_else(|| format!("--provider substrate needs --{flag}"))
                };
                task.substrate = Some(SubstrateArgs {
                    endpoint: required(s.substrate_endpoint, "substrate-endpoint")?,
                    router: required(s.substrate_router, "substrate-router")?,
                    template: required(s.substrate_template, "substrate-template")?,
                    key: required(s.substrate_key, "substrate-key")?,
                    api_token_file: s.substrate_api_token_file,
                    server_name: s.substrate_server_name,
                    egress: if !s.substrate_allow_host.is_empty() {
                        branchyard::config::SubstrateEgress::AllowHosts(s.substrate_allow_host)
                    } else if s.substrate_egress.as_deref() == Some("unrestricted") {
                        branchyard::config::SubstrateEgress::Unrestricted
                    } else {
                        branchyard::config::SubstrateEgress::Deny
                    },
                    atespace: s.substrate_atespace,
                    workdir: s.substrate_workdir,
                    home: s.substrate_home,
                    pass_env,
                    ca: s.substrate_ca,
                    client_cert: s.substrate_client_cert,
                    client_key: s.substrate_client_key,
                    router_ca: s.substrate_router_ca,
                    insecure: s.substrate_insecure,
                    lifecycle,
                });
            }
        }
        Ok(())
    }
}

/// Letting the harness delegate.
#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(next_help_heading = "Delegation")]
pub struct Delegation {
    /// Let the harness create child branches through Branchyard's MCP tools, DEPTH levels
    /// deep (default 1)
    #[arg(
        long,
        value_name = "DEPTH",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "1",
        value_parser = delegate_depth
    )]
    delegate: Option<u32>,
    /// Allow the harness's own by commands that act as its branch (spawn, inspect, send,
    /// wait, discard, artifact, ask, ...; see docs/delegation.md) without asking; nothing else
    #[arg(long)]
    allow_delegation: bool,
    /// Do not start the harness's next turn on its own when its turn ends while its children
    /// run and they then settle
    #[arg(long)]
    no_wake: bool,
}

impl Delegation {
    fn apply(self, task: &mut TaskArgs) {
        task.delegate = self.delegate;
        task.allow_delegation = self.allow_delegation;
        task.no_wake = self.no_wake;
    }
}

/// Provisioning the harness's private home.
#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(next_help_heading = "Provisioning")]
pub struct Provision {
    /// A credential for the harness, such as ANTHROPIC_API_KEY, from the variable of that name,
    /// VAR, or FILE; written only into its private home (--isolated or a sandbox). Repeatable
    #[arg(long = "secret", value_name = "NAME[=VAR|=@FILE]", value_parser = branchyard::SecretSource::parse)]
    secrets: Vec<branchyard::SecretSource>,
    /// The authentication method when the secrets allow several: api-key, oauth-token,
    /// auth-file, vertex-ai
    #[arg(long, value_name = "METHOD")]
    auth: Option<String>,
    /// An MCP server for the harness: NAME=COMMAND starts a stdio server, COMMAND an absolute
    /// path with its arguments; NAME=https://URL connects to a streamable HTTP server where the
    /// harness can (Claude Code, ACP agents that advertise it). Repeatable
    #[arg(long = "mcp", value_name = "NAME=COMMAND|NAME=URL", value_parser = mcp_server)]
    mcp_servers: Vec<McpArg>,
    /// A header for an HTTP --mcp server, its value read each turn from the variable VAR or the
    /// file @FILE and never stored, as a --secret is (so it needs --isolated or a sandbox), such
    /// as 'search:Authorization=@/run/search-auth'. Repeatable
    #[arg(long = "mcp-header", value_name = "NAME:HEADER=VAR|@FILE", value_parser = mcp_header)]
    mcp_headers: Vec<McpHeader>,
    /// Standing instructions for the harness, read from FILE
    #[arg(long, value_name = "FILE")]
    instructions: Option<String>,
    /// The model, or a size alias (small, medium, large, extra-large) where the harness
    /// defines one
    #[arg(long, value_name = "NAME", value_parser = non_blank)]
    model: Option<String>,
    /// Reasoning effort: low, medium, high, xhigh, or 0-100
    #[arg(long, value_name = "LEVEL", value_parser = branchyard::Effort::parse)]
    effort: Option<branchyard::Effort>,
    /// Send the harness's OpenTelemetry to this OTLP/gRPC collector, or turn it off
    #[arg(long, value_name = "URL|off", value_parser = branchyard::Telemetry::parse)]
    telemetry: Option<branchyard::Telemetry>,
    /// A connector the harness may call through the gateway (docs/connectors.md):
    /// CONNECTOR[@ACCOUNT][:read|write|write+confirm[:OP,OP...]], such as github:read or
    /// 'github:write:issues.*'; only into its private home (--isolated or a sandbox). Repeatable
    #[arg(long = "connector", value_name = "GRANT", value_parser = branchyard::connectors::GrantEntry::parse)]
    connectors: Vec<branchyard::connectors::GrantEntry>,
    /// The hosts the harness may reach (docs/egress.md): open, none, or rules
    /// '[METHOD|METHOD ]HOST[:PORT][/PATH]' separated by commas, such as
    /// 'github.com,*.npmjs.org:443' or 'GET|HEAD api.github.com/repos/*' (a rule naming a
    /// method or path is held to inside the connection)
    #[arg(long, value_name = "open|none|HOSTS", value_parser = branchyard::Network::parse_flag)]
    network: Option<branchyard::Network>,
    /// Refuse to run where the network policy cannot be enforced (required), or run with the
    /// proxy's variables only and say so (best-effort, the default)
    #[arg(long, value_name = "MODE", value_parser = branchyard::NetworkEnforce::parse, requires = "network")]
    network_enforce: Option<branchyard::NetworkEnforce>,
    /// Call the harness's models through Branchyard's model gateway (docs/model-gateway.md),
    /// which holds the provider's key, meters every call and holds the budgets; =MODELS limits
    /// it to these models, globs separated by commas, such as 'claude-sonnet-*'
    #[arg(
        long,
        value_name = "MODELS",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "*",
        value_parser = branchyard::models::ModelAccess::parse_flag
    )]
    model_gateway: Option<branchyard::models::ModelAccess>,
}

impl Provision {
    fn apply(self, task: &mut TaskArgs) -> Result<(), String> {
        let mut secrets = self.secrets;
        let mut mcp_servers = Vec::new();
        let mut remote_mcp_servers = Vec::new();
        for server in self.mcp_servers {
            match server {
                McpArg::Stdio(spec) => mcp_servers.push(spec),
                McpArg::Http(spec) => remote_mcp_servers.push(spec),
            }
        }
        for header in self.mcp_headers {
            let server = remote_mcp_servers
                .iter_mut()
                .find(|s: &&mut branchyard::RemoteMcpSpec| s.name == header.server)
                .ok_or_else(|| {
                    format!(
                        "--mcp-header names {}, which no --mcp {}=https://... gives",
                        header.server, header.server
                    )
                })?;
            server
                .headers
                .insert(header.header.clone(), header.secret.name.clone());
            if !secrets.iter().any(|s| s.name == header.secret.name) {
                secrets.push(header.secret);
            }
        }
        let spec = branchyard::Provisioning {
            secrets,
            mcp_servers,
            remote_mcp_servers,
            auth: self.auth,
            model: self.model,
            effort: self.effort,
            telemetry: self.telemetry,
            connectors: self.connectors,
            network: self
                .network
                .map(|n| n.with_enforce(self.network_enforce.unwrap_or_default())),
            models: self.model_gateway,
            ..branchyard::Provisioning::default()
        };
        task.provision = (!spec.is_empty() || self.instructions.is_some()).then_some(spec);
        task.instructions = self.instructions;
        Ok(())
    }
}

/// One `--mcp`: a stdio server the harness starts, or an HTTP one it
/// connects to.
#[derive(Clone, Debug, PartialEq)]
pub enum McpArg {
    Stdio(branchyard::McpServerSpec),
    Http(branchyard::RemoteMcpSpec),
}

/// `--mcp NAME=COMMAND` or `--mcp NAME=https://URL`.
fn mcp_server(text: &str) -> Result<McpArg, String> {
    let (name, rest) = text
        .split_once('=')
        .ok_or_else(|| format!("an MCP server is NAME=COMMAND or NAME=URL, not {text:?}"))?;
    let url = rest.trim();
    if url.starts_with("https://") || url.starts_with("http://") {
        let spec = branchyard::RemoteMcpSpec {
            name: name.to_owned(),
            transport: branchyard::RemoteMcpTransport::Http,
            url: url.to_owned(),
            headers: Default::default(),
        };
        spec.check()?;
        return Ok(McpArg::Http(spec));
    }
    branchyard::McpServerSpec::parse(text).map(McpArg::Stdio)
}

/// One `--mcp-header NAME:HEADER=VAR|@FILE`: the header and the secret
/// that holds its value.
#[derive(Clone, Debug, PartialEq)]
pub struct McpHeader {
    server: String,
    header: String,
    secret: branchyard::SecretSource,
}

fn mcp_header(text: &str) -> Result<McpHeader, String> {
    let shape = || format!("an MCP header is NAME:HEADER=VAR or NAME:HEADER=@FILE, not {text:?}");
    let (server, rest) = text.split_once(':').ok_or_else(shape)?;
    let (header, source) = rest.split_once('=').ok_or_else(shape)?;
    if server.is_empty() || header.is_empty() || source.is_empty() {
        return Err(shape());
    }
    // A file's value gets a secret named for the server and header; a
    // variable's is the secret of that name, as `--secret VAR` is.
    let secret = match source.strip_prefix('@') {
        Some(_) => {
            let name: String = format!("MCP_{server}_{header}")
                .chars()
                .map(|c| match c.is_ascii_alphanumeric() {
                    true => c.to_ascii_uppercase(),
                    false => '_',
                })
                .collect();
            branchyard::SecretSource::parse(&format!("{name}={source}"))?
        }
        None => branchyard::SecretSource::parse(source)?,
    };
    Ok(McpHeader {
        server: server.to_owned(),
        header: header.to_owned(),
        secret,
    })
}

/// `--detach` and its `--json`, for `by run` and `by fork` (`by send` has
/// its own `--json`); see `crate::detach`.
#[derive(Args, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Detached {
    /// Return as soon as the turn has started, leaving it running in the background; its
    /// output goes to a log under .branchyard/detached/ (local mode)
    #[arg(long, conflicts_with = "ask")]
    pub detach: bool,
    /// Print JSON: the branch as `by show --json` prints it once the turn has ended (activity
    /// goes to standard error); with --detach, the branch and the log's path
    #[arg(long)]
    pub json: bool,
}

/// `by run`'s options.
#[derive(Args, Clone, Debug, Default, PartialEq)]
pub struct RunFlags {
    /// Harness or profile ID (default: claude-code, or routed when there is a [fleet])
    #[arg(long, value_name = "ID")]
    harness: Option<String>,
    #[command(flatten)]
    route: RouteFlags,
    /// Take the task from this issue: GitHub's (URL, #N or N, through gh), linear:KEY, jira:KEY,
    /// gitlab:GROUP/PROJECT#N, or a Linear, Jira or GitLab URL; a prompt, if given, is added
    #[arg(long, value_name = "REF", value_parser = non_blank)]
    issue: Option<String>,
    /// Start from GitHub pull request N's head (fetched with gh and git); by pr then updates it
    #[arg(long, value_name = "N", conflicts_with_all = ["issue", "base"])]
    pr: Option<u64>,
    /// Branch name (default: a slug of the prompt)
    #[arg(short, long)]
    name: Option<String>,
    /// Base revision (default: HEAD)
    #[arg(short, long, value_name = "REV")]
    base: Option<String>,
    #[command(flatten)]
    limits: Limits,
    #[command(flatten)]
    perms: Perms,
    /// Tools the harness is denied outright, before any permission answer, --yes included; a
    /// trailing * matches a prefix. Stored with the branch: later sends and every child it
    /// delegates to keep them, as with by spawn --deny. Repeatable
    #[arg(
        long,
        value_name = "TOOL,TOOL,...",
        value_parser = harness_list,
        help_heading = "Permissions"
    )]
    deny: Vec<List>,
    #[command(flatten)]
    launch: Launch,
    #[command(flatten)]
    preflight: Preflight,
    #[command(flatten)]
    delegation: Delegation,
    #[command(flatten)]
    provision: Provision,
    #[command(flatten)]
    plan_goal: PlanGoal,
}

impl Flags for RunFlags {
    type Output = TaskArgs;
    fn check(self) -> Result<TaskArgs, String> {
        if self.route.auto && self.harness.is_some() {
            return Err("--auto routes through the [fleet] table; it takes no --harness".into());
        }
        let mut task = TaskArgs {
            harness: self.harness,
            name: self.name,
            base: self.base,
            issue: self.issue,
            pr: self.pr,
            auto: self.route.auto,
            kind: self.route.kind,
            seed: self.route.seed,
            ..TaskArgs::default()
        };
        self.limits.apply(&mut task);
        self.perms.apply(&mut task);
        task.deny = self.deny.into_iter().flat_map(|list| list.0).collect();
        self.launch.apply(&mut task)?;
        self.preflight.apply(&mut task);
        self.delegation.apply(&mut task);
        self.provision.apply(&mut task)?;
        self.plan_goal.apply(&mut task);
        Ok(task)
    }
}

/// Plan approval and goals, for `run` and `fan`; see docs/plans-and-goals.md.
#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(next_help_heading = "Plan and goal")]
pub struct PlanGoal {
    /// Plan first: the first turn runs read-only and proposes a plan, and the branch waits for
    /// `by plan approve` (or reject) before anything changes
    #[arg(long)]
    plan: bool,
    /// A goal a judge must find evidence of before the branch is done; unmet, it gets follow-up
    /// turns with what is missing
    #[arg(long, value_name = "TEXT", value_parser = non_blank)]
    goal: Option<String>,
    /// Follow-up turns at most for an unmet goal (default 2), within the budget
    #[arg(long, value_name = "N", requires = "goal", value_parser = clap::value_parser!(u32).range(0..=20))]
    goal_rounds: Option<u32>,
    /// The goal's judge harness, read-only on a scratch branch (default: the [fleet] entry's
    /// goal_judge; without one, the branch's check and a non-empty diff decide)
    #[arg(long, value_name = "ID", requires = "goal")]
    goal_judge: Option<String>,
    /// Launch the goal judge with this instead of its executable, for development and testing
    #[arg(long, value_name = "CMD", value_parser = command_argv, requires = "goal_judge")]
    goal_judge_command: Option<Argv>,
}

impl PlanGoal {
    fn apply(self, task: &mut TaskArgs) {
        task.plan = self.plan;
        task.goal = self.goal;
        task.goal_rounds = self.goal_rounds;
        task.goal_judge = self.goal_judge;
        task.goal_judge_command = self.goal_judge_command.map(|a| a.0);
    }
}

/// Routing through the fleet table, for `run` and `fan`; see docs/fleet.md.
#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(next_help_heading = "Routing")]
pub struct RouteFlags {
    /// Pick the harness from the [fleet] table in branchyard.toml, learning from outcomes, and
    /// fail over to the next candidate when a harness fails
    #[arg(long)]
    auto: bool,
    /// The task's kind (default: inferred from the prompt): bugfix, feature, refactor, review,
    /// research, docs, migration, tests, other
    #[arg(long, value_name = "KIND", value_parser = task_kind)]
    kind: Option<branchyard::TaskKind>,
    /// Seed the router, for a reproducible pick
    #[arg(long, value_name = "N")]
    seed: Option<u64>,
}

fn task_kind(text: &str) -> Result<branchyard::TaskKind, String> {
    text.parse().map_err(|e: branchyard::Error| match e {
        branchyard::Error::Unsupported(why) => why,
        other => other.to_string(),
    })
}

/// `by fan`'s options: `run`'s, with `--harness` a list, taken by the
/// command itself.
#[derive(Args, Clone, Debug, Default, PartialEq)]
pub struct FanFlags {
    /// Branch name prefix (default: a slug of the prompt)
    #[arg(short, long)]
    name: Option<String>,
    #[command(flatten)]
    route: RouteFlags,
    /// Take the task from this issue: GitHub's (URL, #N or N, through gh), linear:KEY, jira:KEY,
    /// gitlab:GROUP/PROJECT#N, or a Linear, Jira or GitLab URL; a prompt, if given, is added
    #[arg(long, value_name = "REF", value_parser = non_blank)]
    issue: Option<String>,
    /// Start from GitHub pull request N's head (fetched with gh and git); by pr then updates it
    #[arg(long, value_name = "N", conflicts_with_all = ["issue", "base"])]
    pr: Option<u64>,
    /// Base revision (default: HEAD)
    #[arg(short, long, value_name = "REV")]
    base: Option<String>,
    #[command(flatten)]
    limits: Limits,
    #[command(flatten)]
    perms: Perms,
    #[command(flatten)]
    launch: Launch,
    #[command(flatten)]
    preflight: Preflight,
    #[command(flatten)]
    delegation: Delegation,
    #[command(flatten)]
    provision: Provision,
    #[command(flatten)]
    plan_goal: PlanGoal,
}

impl Flags for FanFlags {
    type Output = TaskArgs;
    fn check(self) -> Result<TaskArgs, String> {
        RunFlags {
            harness: None,
            route: self.route,
            issue: self.issue,
            pr: self.pr,
            name: self.name,
            base: self.base,
            limits: self.limits,
            perms: self.perms,
            launch: self.launch,
            preflight: self.preflight,
            delegation: self.delegation,
            provision: self.provision,
            plan_goal: self.plan_goal,
            deny: Vec::new(),
        }
        .check()
    }
}

/// `by task`, as its own struct and boxed: [`Command`]'s parser builds every
/// command's arguments in one function, whose debug-build stack frame is
/// near a test thread's limit, so this one adds only a call.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct TaskCommand {
    /// Print JSON
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub action: TaskAction,
}

/// `by task`'s actions (docs/task-repos.md), each with its own flags.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum TaskAction {
    /// Start a task and run its first attempt
    ///
    /// Start a task and run its first attempt: in this repository, in a folder you grant
    /// (--folder), or with no files (--no-files)
    New(Box<TaskNew>),
    /// List tasks: this repository's, then those with a repository of their own
    Ls,
    /// Show a task: what was asked, its attempts and their conversations
    Show(TaskShow),
    /// Open an attempt's worktree in your editor (or print its path)
    Open(TaskOpen),
    /// Reset an attempt, its files and its conversation, to one of its checkpoints
    Rewind(TaskRewind),
    /// Start another attempt from an attempt's candidate, or from its checkpoint N
    Fork(Box<TaskFork>),
    /// Accept an attempt: merge it here, or apply it to the task's folder
    Accept(TaskAccept),
    /// Remove a task and its attempts (never the folder you granted)
    Rm(TaskRm),
}

/// `by task new`.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct TaskNew {
    /// What to do; quote it
    pub prompt: String,
    /// A folder to work on (not a git repository): attempts run in their own worktrees and
    /// the folder changes only when you accept one
    #[arg(long, value_name = "PATH", value_hint = ValueHint::DirPath, conflicts_with = "no_files")]
    pub folder: Option<std::path::PathBuf>,
    /// A task with no files: its conversation and results are its repository
    #[arg(long)]
    pub no_files: bool,
    /// With --folder or --no-files: files of at least this many bytes are stored as chunks
    /// (default 1048576)
    #[arg(long, value_name = "BYTES")]
    pub large_threshold: Option<u64>,
    #[command(flatten)]
    pub task: Checked<RunFlags>,
}

/// `by task show`.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct TaskShow {
    /// The task's ID (or a prefix of at least 4 characters) or one of its attempts
    pub task: String,
}

/// `by task open`.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct TaskOpen {
    /// The task, by ID or by one of its branches
    pub task: String,
    /// The attempt (default: the only one)
    #[arg(long, value_name = "BRANCH")]
    pub attempt: Option<String>,
    /// The editor: code, cursor, zed, nvim, ... or a command line (default: $VISUAL, then
    /// $EDITOR)
    #[arg(long, value_name = "EDITOR", conflicts_with = "print")]
    pub editor: Option<String>,
    /// Print the worktree's path instead of opening it
    #[arg(long)]
    pub print: bool,
}

/// `by task rewind`.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct TaskRewind {
    /// The task whose attempt to reset, by ID or by one of its branches
    pub task: String,
    /// The attempt (default: the only one)
    #[arg(long, value_name = "BRANCH")]
    pub attempt: Option<String>,
    /// The checkpoint: a turn number, or 0 for the attempt's base
    #[arg(long, value_name = "N")]
    pub to: u32,
    /// Confirm the reset without a prompt
    #[arg(long, short = 'y')]
    pub yes: bool,
}

/// `by task fork`.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct TaskFork {
    /// The task to fork, by ID or by one of its branches
    pub task: String,
    /// The new attempt's prompt; quote it
    pub prompt: String,
    /// The attempt to fork (default: the only one)
    #[arg(long, value_name = "BRANCH")]
    pub attempt: Option<String>,
    /// Fork from checkpoint N (0 is its base) instead of its candidate
    #[arg(long, value_name = "N")]
    pub at: Option<u32>,
    #[command(flatten)]
    pub flags: Checked<ForkFlags>,
}

/// `by task accept`.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct TaskAccept {
    /// The task whose attempt to accept, by ID or by one of its branches
    pub task: String,
    /// The attempt (default: the only one)
    #[arg(long, value_name = "BRANCH")]
    pub attempt: Option<String>,
    /// In a repository: the branch to merge into (default: the current branch)
    #[arg(long, value_name = "TARGET")]
    pub into: Option<String>,
}

/// `by task rm`.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct TaskRm {
    /// A branch, or a task ID
    pub task: String,
    /// Confirm the removal without a prompt
    #[arg(long, short = 'y')]
    pub yes: bool,
}

/// `by map`'s actions besides running one.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum MapAction {
    /// Run a recorded map again with its command line and items, skipping the items done
    Resume {
        /// The map to run again
        name: String,
        /// Run the items that failed again too
        #[arg(long)]
        retry_failed: bool,
    },
    /// The recorded maps and their progress
    Ls,
    /// A map's progress, its items' results and its reduce
    Show {
        /// The map's name
        name: String,
    },
    /// Forget a map's record (its branches stay; remove them with by rm)
    Rm {
        /// The map to forget
        name: String,
    },
}

/// `by map`'s run: the prompt template, where the items come from, the
/// answer's schema, the results, and the branches' options.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct MapArgs {
    /// The prompt template: {{item}}, {{item.FIELD}} (a path such as {{item.a.0.b}}), {{id}},
    /// {{index}} and {{map}} are replaced for each item
    #[arg(required = true)]
    pub prompt: Option<String>,
    /// Read the items from this file (- for standard input): JSON lines, a JSON array, CSV with a
    /// header, or one item per line, by its extension (default: standard input)
    #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub items: Option<String>,
    /// Read the items from this shell command's output, run in the repository as you
    #[arg(long, value_name = "CMD", conflicts_with = "items", value_parser = non_blank)]
    pub from_command: Option<String>,
    /// How the items are written: jsonl, json, csv or lines (default: the file's extension, else
    /// guessed from the first character: [ for json, { for jsonl, else lines)
    #[arg(long, value_name = "FORMAT", value_parser = item_format)]
    pub input_format: Option<branchyard::ItemFormat>,
    /// Each branch must answer with JSON matching this JSON Schema (a subset; see docs/map.md);
    /// without one, an item's result is its last reply's text
    #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub schema: Option<String>,
    /// Write the results here as each item ends: CSV for a .csv file, else JSON lines
    #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub out: Option<String>,
    /// Branches running at once (default 4)
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=64))]
    pub concurrency: Option<u32>,
    /// New branches an item gets after its first fails (default 1)
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(0..=10))]
    pub retries: Option<u32>,
    /// Stop starting items once the map has cost X dollars, across runs (per branch, use
    /// --budget-usd)
    #[arg(long, value_name = "DOLLARS", value_parser = usd)]
    pub total_usd: Option<f64>,
    /// Then one more branch, given every result, answers this prompt: the map's summary
    #[arg(long, value_name = "PROMPT", value_parser = non_blank)]
    pub reduce: Option<String>,
    /// Write the reduce's answer to this file too
    #[arg(long, value_name = "FILE", requires = "reduce", value_hint = ValueHint::FilePath)]
    pub reduce_out: Option<String>,
    /// Remove an item's branches once its answer is recorded (failed items' branches are kept)
    #[arg(long)]
    pub rm: bool,
    /// Run the items that failed in an earlier run again
    #[arg(long)]
    pub retry_failed: bool,
    #[command(flatten)]
    pub task: Checked<MapFlags>,
}

fn item_format(text: &str) -> Result<branchyard::ItemFormat, String> {
    text.parse().map_err(|e: branchyard::Error| match e {
        branchyard::Error::Unsupported(why) => why,
        other => other.to_string(),
    })
}

/// `by map`'s branch options: `run`'s, without an issue, a pull request,
/// delegation, a plan or a goal.
#[derive(Args, Clone, Debug, Default, PartialEq)]
pub struct MapFlags {
    /// Harness or profile ID (default: claude-code, or routed when there is a [fleet])
    #[arg(long, value_name = "ID")]
    harness: Option<String>,
    /// The map's name, which its branches' names start with (default: a slug of the prompt)
    #[arg(short, long)]
    name: Option<String>,
    /// Base revision of every branch (default: HEAD)
    #[arg(short, long, value_name = "REV")]
    base: Option<String>,
    #[command(flatten)]
    route: RouteFlags,
    #[command(flatten)]
    limits: Limits,
    #[command(flatten)]
    perms: Perms,
    #[command(flatten)]
    launch: Launch,
    #[command(flatten)]
    provision: Provision,
}

impl Flags for MapFlags {
    type Output = TaskArgs;
    fn check(self) -> Result<TaskArgs, String> {
        RunFlags {
            harness: self.harness,
            route: self.route,
            issue: None,
            pr: None,
            name: self.name,
            base: self.base,
            limits: self.limits,
            perms: self.perms,
            launch: self.launch,
            preflight: Preflight::default(),
            delegation: Delegation::default(),
            provision: self.provision,
            plan_goal: PlanGoal::default(),
            deny: Vec::new(),
        }
        .check()
    }
}

/// `by send`'s task options: the branch keeps its harness, workspace and
/// provider.
#[derive(Args, Clone, Debug, Default, PartialEq)]
pub struct SendFlags {
    #[command(flatten)]
    limits: Limits,
    #[command(flatten)]
    perms: Perms,
    /// Launch this instead of the profile's executable, for development and testing
    #[arg(long, value_name = "CMD", value_parser = command_argv, help_heading = "Launch")]
    command: Option<Argv>,
    #[command(flatten)]
    preflight: Preflight,
    #[command(flatten)]
    delegation: Delegation,
    #[command(flatten)]
    provision: Provision,
    /// With --remote: only a worker carrying this label runs it (repeatable)
    #[arg(long = "require-label", value_name = "LABEL", help_heading = "Launch")]
    require_label: Vec<String>,
    /// With --remote: the operation's priority, -10 to 10 (default 0); higher runs first
    #[arg(
        long,
        value_name = "N",
        allow_negative_numbers = true,
        value_parser = clap::value_parser!(i32).range(-10..=10),
        help_heading = "Launch"
    )]
    priority: Option<i32>,
}

impl Flags for SendFlags {
    type Output = TaskArgs;
    fn check(self) -> Result<TaskArgs, String> {
        let mut task = TaskArgs {
            command: self.command.map(|argv| argv.0),
            require_labels: self.require_label,
            priority: self.priority,
            ..TaskArgs::default()
        };
        self.limits.apply(&mut task);
        self.perms.apply(&mut task);
        self.preflight.apply(&mut task);
        self.delegation.apply(&mut task);
        self.provision.apply(&mut task)?;
        Ok(task)
    }
}

/// `by fork`'s task options.
#[derive(Args, Clone, Debug, Default, PartialEq)]
pub struct ForkFlags {
    /// The new branch's name (default: derived from the branch's)
    #[arg(short, long)]
    name: Option<String>,
    #[command(flatten)]
    limits: Limits,
    #[command(flatten)]
    perms: Perms,
    #[command(flatten)]
    launch: Launch,
    #[command(flatten)]
    delegation: Delegation,
    #[command(flatten)]
    provision: Provision,
}

impl Flags for ForkFlags {
    type Output = TaskArgs;
    fn check(self) -> Result<TaskArgs, String> {
        RunFlags {
            harness: None,
            route: RouteFlags::default(),
            issue: None,
            pr: None,
            name: self.name,
            base: None,
            limits: self.limits,
            perms: self.perms,
            launch: self.launch,
            preflight: Preflight::default(),
            delegation: self.delegation,
            provision: self.provision,
            plan_goal: PlanGoal::default(),
            deny: Vec::new(),
        }
        .check()
    }
}

/// `by reincarnate`'s task options.
#[derive(Args, Clone, Debug, Default, PartialEq)]
pub struct ReincarnateFlags {
    /// The new branch's name (default: derived from the branch's)
    #[arg(short, long)]
    name: Option<String>,
    /// Harness or profile ID (default: the branch's)
    #[arg(long, value_name = "ID")]
    harness: Option<String>,
    #[command(flatten)]
    limits: Limits,
    #[command(flatten)]
    perms: Perms,
    #[command(flatten)]
    launch: Launch,
    #[command(flatten)]
    delegation: Delegation,
    #[command(flatten)]
    provision: Provision,
}

impl Flags for ReincarnateFlags {
    type Output = TaskArgs;
    fn check(self) -> Result<TaskArgs, String> {
        RunFlags {
            harness: self.harness,
            route: RouteFlags::default(),
            issue: None,
            pr: None,
            name: self.name,
            base: None,
            limits: self.limits,
            perms: self.perms,
            launch: self.launch,
            preflight: Preflight::default(),
            delegation: self.delegation,
            provision: self.provision,
            plan_goal: PlanGoal::default(),
            deny: Vec::new(),
        }
        .check()
    }
}

/// `by spawn`'s options.
#[derive(Args, Clone, Debug, Default, PartialEq)]
pub struct SpawnFlags {
    /// The delegating branch (outside a harness; inside one, it is the harness's own)
    #[arg(long, value_name = "BRANCH")]
    parent: Option<String>,
    /// In a rig, the seat the child fills; it sets the child's harness, limits, check and
    /// instructions
    #[arg(long, value_name = "NAME")]
    seat: Option<String>,
    /// Harness or profile ID, such as claude-code or claude-code-acp (default: the parent's own
    /// profile); shown as both, `claude-code (claude-code-stream-json)`
    #[arg(long, value_name = "ID")]
    harness: Option<String>,
    /// The child's model, or a size alias (small, medium, large, extra-large) where its harness
    /// defines one; a cheaper one suits mechanical work (default: its seat's, else the parent's).
    /// Refused for a harness whose driver cannot choose one
    #[arg(long, value_name = "NAME", value_parser = non_blank)]
    model: Option<String>,
    /// Take the task from this issue: GitHub's (URL, #N or N, through gh), linear:KEY, jira:KEY,
    /// gitlab:GROUP/PROJECT#N, or a Linear, Jira or GitLab URL; a prompt, if given, is added
    #[arg(long, value_name = "REF", value_parser = non_blank)]
    issue: Option<String>,
    /// Branch name (default: a slug of the prompt)
    #[arg(short, long)]
    name: Option<String>,
    /// Base revision (default: the parent's candidate)
    #[arg(short, long, value_name = "REV")]
    base: Option<String>,
    /// Wait for the turn to end and show it (outside a harness, spawn always waits)
    #[arg(long)]
    wait: bool,
    /// Print JSON
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    limits: Limits,
    #[command(flatten)]
    graph: SpawnGraph,
    #[command(flatten)]
    perms: Perms,
}

/// Where a spawned child sits among its siblings.
#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(next_help_heading = "Delegation")]
pub struct SpawnGraph {
    /// Levels the child may delegate below itself (default: one fewer than the parent)
    #[arg(long, value_name = "N")]
    max_depth: Option<u32>,
    /// Children the child may have at once (default and most: the parent's)
    #[arg(long, value_name = "N")]
    max_children: Option<u32>,
    /// Harness or profile IDs the child may delegate to, each allowed to the parent (default:
    /// the parent's)
    #[arg(long, value_name = "ID,ID,...", value_parser = harness_list)]
    harnesses: Option<List>,
    /// Tools the child is denied outright; a trailing * matches a prefix. Repeatable
    #[arg(long, value_name = "TOOL,TOOL,...", value_parser = harness_list)]
    deny: Vec<List>,
    /// Siblings the child waits for: it is created waiting and starts once they have settled
    #[arg(long, value_name = "BRANCH,BRANCH,...", value_parser = harness_list)]
    depends_on: Option<List>,
    /// When each --depends-on branch counts as done
    #[arg(long, value_name = "WHEN")]
    after: Option<AfterArg>,
    /// Bind the child to a scratch area, ACCESS read_only or exclusive_write (which holds its
    /// writer lock for each turn); repeatable
    #[arg(long = "bind", value_name = "SCRATCH:ACCESS", value_parser = branchyard::Binding::parse)]
    bindings: Vec<branchyard::Binding>,
    /// A connector the child may use, within its parent's grant (default: its seat's or its
    /// parent's): CONNECTOR[@ACCOUNT][:read|write|write+confirm[:OP,OP...]]. Repeatable
    #[arg(long = "connector", value_name = "GRANT", value_parser = branchyard::connectors::GrantEntry::parse)]
    connectors: Vec<branchyard::connectors::GrantEntry>,
    /// Plan first: the child's first turn is read-only and its plan is escalated to the
    /// parent's inbox; it changes nothing until `by plan approve`
    #[arg(long)]
    plan: bool,
    /// Why this harness and model for this subtask, in a few words: recorded on the child, shown
    /// by `by log`, and counted by `by fleet stats` beside how the child turned out
    #[arg(long, value_name = "TEXT", value_parser = non_blank)]
    why: Option<String>,
    /// The shape of the answer the child must report with `by report --result`, as a JSON Schema
    /// using only type, properties, required, additionalProperties, items, enum, const, minimum,
    /// maximum, exclusiveMinimum, exclusiveMaximum, minLength, maxLength, pattern, minItems,
    /// maxItems, uniqueItems, anyOf, oneOf and annotations such as title and description;
    /// anything else is refused. You read the result in `by inspect` and `by wait`
    #[arg(long, value_name = "JSON", value_parser = non_blank, conflicts_with = "result_schema_file")]
    result_schema: Option<String>,
    /// Read the result schema from this file instead, `-` for standard input
    #[arg(long, value_name = "PATH", value_parser = non_blank)]
    result_schema_file: Option<String>,
    /// The child must report a result: its `by report` without --result is refused, and its
    /// ending a turn without one is noted in your log
    #[arg(long)]
    result_required: bool,
    /// A path the child owns: a file, a directory (and everything below it) or a glob such as
    /// tests/parser_*.rs, relative to the repository. Its changes anywhere else are refused when
    /// it is integrated, and it is told them every turn. Repeatable, or comma-separated
    #[arg(long, value_name = "PATH,PATH,...", value_parser = harness_list)]
    owns: Vec<List>,
    /// Leave the child to the engine to land: once its turn ends ready while you wait on your
    /// children, it is integrated into your branch, its check on the merge included, and a child
    /// that cannot land is sent a turn to fix its work; your next turn says what landed
    #[arg(long)]
    land: bool,
    /// Have a reviewer on this harness, ideally another vendor's than the child's, read its work
    /// each time it is ready to land, before it lands; findings are sent to the child as a turn
    /// to fix its work. Needs --land, and a harness you may delegate to
    #[arg(long, value_name = "HARNESS", requires = "land", value_parser = non_blank)]
    review: Option<String>,
    /// The model the reviewer runs
    #[arg(long, value_name = "MODEL", requires = "review", value_parser = non_blank)]
    review_model: Option<String>,
    /// What the reviewer should look for, added to its prompt
    #[arg(long, value_name = "TEXT", requires = "review", value_parser = non_blank)]
    review_rubric: Option<String>,
    /// Each review's cost limit, from your budget; required when you have a cost limit
    #[arg(long, value_name = "DOLLARS", requires = "review", value_parser = usd)]
    review_budget_usd: Option<f64>,
}

impl Flags for SpawnFlags {
    type Output = SpawnArgs;
    fn check(self) -> Result<SpawnArgs, String> {
        if self.graph.result_required
            && self.graph.result_schema.is_none()
            && self.graph.result_schema_file.is_none()
        {
            return Err(
                "--result-required needs the schema it applies to: --result-schema or \
                 --result-schema-file"
                    .into(),
            );
        }
        let mut task = TaskArgs {
            harness: self.harness,
            name: self.name,
            base: self.base,
            issue: self.issue,
            ..TaskArgs::default()
        };
        self.limits.apply(&mut task);
        self.perms.apply(&mut task);
        Ok(SpawnArgs {
            task,
            parent: self.parent,
            wait: self.wait,
            max_depth: self.graph.max_depth,
            max_children: self.graph.max_children,
            harnesses: self.graph.harnesses.map(|list| list.0),
            deny: self
                .graph
                .deny
                .into_iter()
                .flat_map(|list| list.0)
                .collect(),
            seat: self.seat,
            depends_on: self.graph.depends_on.map(|list| list.0).unwrap_or_default(),
            after: self.graph.after.map(Into::into).unwrap_or_default(),
            bindings: self.graph.bindings,
            connectors: self.graph.connectors,
            plan: self.graph.plan,
            why: self.graph.why,
            model: self.model,
            result_schema: self.graph.result_schema,
            result_schema_file: self.graph.result_schema_file,
            result_required: self.graph.result_required,
            owns: self
                .graph
                .owns
                .into_iter()
                .flat_map(|list| list.0)
                .collect(),
            land: self.graph.land,
            review: self.graph.review.map(|harness| {
                Box::new(branchyard::ReviewSpec {
                    harness,
                    model: self.graph.review_model,
                    rubric: self.graph.review_rubric,
                    budget_usd: self.graph.review_budget_usd,
                })
            }),
            json: self.json,
        })
    }
}

/// `by pr`, checked.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PrArgs {
    /// The git remote to push to.
    pub git_remote: String,
    /// The branch on the remote (default: the branch's git branch).
    pub head: Option<String>,
    pub base: Option<String>,
    pub title: Option<String>,
    pub draft: bool,
    /// `-R OWNER/REPO` for gh, when it cannot tell from the remotes.
    pub gh_repo: Option<String>,
    pub force: bool,
    pub no_check: bool,
    pub allow_failing_check: bool,
    pub allow_not_ready: bool,
    pub watch: bool,
    /// First poll interval of `--watch`; it backs off to ten times this.
    pub interval: Duration,
    /// `--max-rounds`: stop after delivering feedback this many times.
    pub max_rounds: Option<u32>,
    /// `--no-resolve`: leave review threads the watch fed back unresolved
    /// after a push addresses them.
    pub no_resolve: bool,
    /// Limits and permissions for the turns `--watch` starts.
    pub task: TaskArgs,
    pub json: bool,
}

/// `by pr`'s options.
#[derive(Args, Clone, Debug, Default, PartialEq)]
pub struct PrFlags {
    /// The git remote to push to (not --remote, which names a Branchyard server)
    #[arg(long, value_name = "NAME", default_value = "origin", value_parser = non_blank)]
    git_remote: String,
    /// The branch to push to on the remote (default: the branch's git branch, by/<name>)
    #[arg(long, value_name = "BRANCH", value_parser = non_blank)]
    head: Option<String>,
    /// The pull request's base branch (default: the repository's default branch)
    #[arg(long, value_name = "BRANCH", value_parser = non_blank)]
    base: Option<String>,
    /// The pull request's title (default: the issue's title, or the prompt's first line); on
    /// an update, only when given
    #[arg(long, value_parser = non_blank)]
    title: Option<String>,
    /// Open the pull request as a draft (when creating it)
    #[arg(long)]
    draft: bool,
    /// The GitHub repository, when gh cannot tell it from the git remotes
    #[arg(long, value_name = "OWNER/REPO", value_parser = non_blank)]
    gh_repo: Option<String>,
    /// Replace the remote branch even if the candidate does not descend from it
    #[arg(long)]
    force: bool,
    /// Push without running the branch's check on its candidate
    #[arg(long, help_heading = "Readiness")]
    no_check: bool,
    /// Push even though the branch's check fails on its candidate
    #[arg(long, help_heading = "Readiness", conflicts_with = "no_check")]
    allow_failing_check: bool,
    /// Push a branch that is not ready (failed, interrupted, at a limit) if it has a candidate
    #[arg(long, help_heading = "Readiness")]
    allow_not_ready: bool,
    /// Then follow the pull request: send failed CI checks and new review comments into the
    /// branch, and push again after each turn, until it is merged or closed
    #[arg(long, help_heading = "Watching")]
    watch: bool,
    /// With --watch: the first time between polls, backing off to ten times it
    #[arg(
        long,
        value_name = "SECONDS",
        default_value = "30",
        value_parser = watch_interval,
        requires = "watch",
        help_heading = "Watching"
    )]
    interval: Duration,
    /// With --watch: stop after delivering feedback N times
    #[arg(
        long,
        value_name = "N",
        value_parser = positive_turns,
        requires = "watch",
        help_heading = "Watching"
    )]
    max_rounds: Option<u32>,
    /// With --watch: do not reply "Addressed in <commit>" to, and resolve, the review threads it
    /// fed back once a pushed commit changes their files
    #[arg(long, requires = "watch", help_heading = "Watching")]
    no_resolve: bool,
    /// Print JSON
    #[arg(long, conflicts_with = "watch")]
    json: bool,
    #[command(flatten)]
    perms: Perms,
    /// Launch this instead of the profile's executable for --watch's turns, for development and
    /// testing
    #[arg(long, value_name = "CMD", value_parser = command_argv, help_heading = "Launch")]
    command: Option<Argv>,
}

impl Flags for PrFlags {
    type Output = PrArgs;
    fn check(self) -> Result<PrArgs, String> {
        let mut task = TaskArgs {
            command: self.command.map(|argv| argv.0),
            ..TaskArgs::default()
        };
        if !self.watch && (self.perms != Perms::default() || task.command.is_some()) {
            return Err(
                "permissions and --command apply to the turns --watch starts; add --watch".into(),
            );
        }
        self.perms.apply(&mut task);
        Ok(PrArgs {
            git_remote: self.git_remote,
            head: self.head,
            base: self.base,
            title: self.title,
            draft: self.draft,
            gh_repo: self.gh_repo,
            force: self.force,
            no_check: self.no_check,
            allow_failing_check: self.allow_failing_check,
            allow_not_ready: self.allow_not_ready,
            watch: self.watch,
            interval: self.interval,
            max_rounds: self.max_rounds,
            no_resolve: self.no_resolve,
            task,
            json: self.json,
        })
    }
}

/// `by`'s command, with the first-run command list in its help. Built on a
/// thread with [`PARSE_STACK`], like parsing, so help, completions and the
/// man page never depend on the caller's stack either.
pub fn command() -> clap::Command {
    on_parse_stack(build_command)
}

/// [`command`] with the commands that are hidden from `by --help` shown, as
/// the man page lists them.
pub fn command_with_hidden() -> clap::Command {
    on_parse_stack(|| {
        let mut cmd = build_command();
        let names: Vec<String> = cmd
            .get_subcommands()
            .filter(|c| c.is_hide_set())
            .map(|c| c.get_name().to_owned())
            .collect();
        for name in names {
            cmd = cmd.mut_subcommand(name, |c| c.hide(false));
        }
        cmd
    })
}

fn build_command() -> clap::Command {
    let cmd = crate::operations::annotate(Cli::command());
    let header = *cmd.get_styles().get_header();
    let listing = command_listing(&cmd, Listing::Front, true);
    cmd.help_template(format!(
        "{{about-with-newline}}\n{{usage-heading}} {{usage}}\n\n{listing}\
         {header}Options:{header:#}\n{{options}}{{after-help}}"
    ))
}

/// What `by --help` and `by help all|agents` list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Listing {
    /// The first-run commands, [`FRONT`], and where to find the rest.
    Front,
    /// Every command, hidden or not, under [`GROUPS`]: `by help all`.
    All,
    /// What a harness runs for itself: `by help agents`.
    Agents,
}

/// The commands of `by --help`, in the order a first hour uses them. A name
/// no command has (a command another change adds) is left out.
pub const FRONT: &[(&str, &[&str])] = &[
    ("Start", &["setup", "init", "doctor"]),
    (
        "Work",
        &[
            "run", "fan", "send", "steer", "wait", "cancel", "merge", "pr",
        ],
    ),
    ("Look", &["ls", "show", "diff", "log", "watch"]),
    ("Clean up", &["rm", "rewind"]),
];

/// The commands of `by help agents` besides the group of delegation: what
/// the engine and the plugins run.
const AGENT_PLUMBING: &[&str] = &["mcp", "hook", "statusline"];

/// The listing `by help NAME` prints when NAME is a topic and not a
/// command: `all` and `agents`. `args` are `by`'s, after the program name;
/// global options may come first.
pub fn help_listing(args: &[OsString]) -> Option<Listing> {
    let mut at = 0;
    while let Some(arg) = args.get(at).and_then(|a| a.to_str()) {
        let Some(long) = arg.strip_prefix("--") else {
            break;
        };
        let name = long.split_once('=').map_or(long, |(name, _)| name);
        if name == "no-notify" {
            at += 1;
        } else if ["remote", "token-file", "repo", "ca-file"].contains(&name) {
            at += if long.contains('=') { 1 } else { 2 };
        } else {
            return None;
        }
    }
    let rest: Vec<&str> = args
        .get(at..)?
        .iter()
        .map(|a| a.to_str())
        .collect::<Option<_>>()?;
    match rest[..] {
        ["help", "all"] => Some(Listing::All),
        ["help", "agents"] => Some(Listing::Agents),
        _ => None,
    }
}

/// The text of `by help all` or `by help agents`; `color` for a terminal.
pub fn listing_text(listing: Listing, color: bool) -> String {
    on_parse_stack(move || {
        let cmd = build_command();
        let mut text = command_listing(&cmd, listing, color);
        text.push_str("Run 'by help <command>' or 'by <command> --help' for its options.\n");
        text
    })
}

/// The command list of a help screen: `listing`'s, with ANSI styles when
/// `color` (clap strips them again when the help goes to a pipe).
fn command_listing(cmd: &clap::Command, listing: Listing, color: bool) -> String {
    let mut built = cmd.clone();
    built.build();
    let styles = built.get_styles();
    let (header, literal): (Style, Style) = match color {
        true => (*styles.get_header(), *styles.get_literal()),
        false => (Style::new(), Style::new()),
    };
    let find = |name: &str| built.get_subcommands().find(|c| c.get_name() == name);
    let titled = |title: &str, rows: &[&clap::Command]| -> String {
        let width = rows
            .iter()
            .map(|c| c.get_name().len())
            .max()
            .unwrap_or(0)
            .max(10);
        let mut text = format!("{header}{title}:{header:#}\n");
        for c in rows {
            let about = c.get_about().map(|a| a.to_string()).unwrap_or_default();
            let name = c.get_name();
            let pad = " ".repeat(width - name.len());
            text.push_str(&format!("  {literal}{name}{literal:#}{pad}  {about}\n"));
        }
        text.push('\n');
        text
    };
    let mut text = String::new();
    match listing {
        Listing::Front => {
            for (title, names) in FRONT {
                let rows: Vec<&clap::Command> = names.iter().filter_map(|n| find(n)).collect();
                if !rows.is_empty() {
                    text.push_str(&titled(title, &rows));
                }
            }
            let shown: Vec<&str> = FRONT
                .iter()
                .flat_map(|(_, names)| names.iter().copied())
                .collect();
            let others: Vec<&str> = built
                .get_subcommands()
                .filter(|c| !c.is_hide_set() && c.get_name() != "help")
                .map(|c| c.get_name())
                .filter(|name| !shown.contains(name))
                .collect();
            text.push_str(&format!("{header}More:{header:#}\n"));
            for (what, says) in [
                ("by help all", "Every command, with what agents use"),
                ("by help agents", "What a harness runs for itself"),
                ("by <command> --help", "Options for one command"),
            ] {
                let pad = " ".repeat(22 - what.len());
                text.push_str(&format!("  {literal}{what}{literal:#}{pad}{says}\n"));
            }
            text.push_str(&wrapped("Also", &others));
            text.push('\n');
        }
        Listing::All => {
            let group = |c: &clap::Command| match c.get_name() {
                "help" => GROUPS.last().map_or(9, |(group, _)| *group),
                _ => c.get_display_order() / 100,
            };
            let mut groups: Vec<(usize, &str)> = GROUPS.to_vec();
            groups.push((usize::MAX, "Other commands"));
            for (number, title) in groups {
                let mut members: Vec<&clap::Command> = built
                    .get_subcommands()
                    .filter(|c| {
                        let g = group(c);
                        g == number
                            || (number == usize::MAX && !GROUPS.iter().any(|(n, _)| *n == g))
                    })
                    .collect();
                if members.is_empty() {
                    continue;
                }
                members.sort_by_key(|c| (c.get_display_order(), c.get_name()));
                text.push_str(&titled(title, &members));
            }
        }
        Listing::Agents => {
            let mut members: Vec<&clap::Command> = built
                .get_subcommands()
                .filter(|c| {
                    c.get_display_order() / 100 == 3 || AGENT_PLUMBING.contains(&c.get_name())
                })
                .collect();
            members.sort_by_key(|c| (c.get_display_order(), c.get_name()));
            text.push_str(&titled(
                "Commands a harness runs for itself (usually from inside its branch)",
                &members,
            ));
        }
    }
    text
}

/// `label: a, b, c` wrapped to 78 columns under the two-space indent.
fn wrapped(label: &str, names: &[&str]) -> String {
    let mut out = String::new();
    let mut line = format!("  {label}: ");
    let indent = " ".repeat(line.len());
    for (n, name) in names.iter().enumerate() {
        let piece = match n + 1 == names.len() {
            true => (*name).to_owned(),
            false => format!("{name}, "),
        };
        if line.trim_end().len() + piece.len() > 78 && line.trim().len() > label.len() + 1 {
            out.push_str(line.trim_end());
            out.push('\n');
            line = indent.clone();
        }
        line.push_str(&piece);
    }
    out.push_str(line.trim_end());
    out.push('\n');
    out
}

/// The stack [`parse_from`] parses on: clap's derived parser for every
/// command is one function whose unoptimized frame outgrew the 2 MiB a
/// spawned thread (a test, a worker) gets, so it never depends on the
/// caller's stack.
const PARSE_STACK: usize = 8 << 20;

/// Parse `by`'s command line, program name first.
pub fn parse_from<I, T>(argv: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let argv: Vec<OsString> = argv.into_iter().map(Into::into).collect();
    on_parse_stack(move || parse_argv(argv))
}

/// Run `f` on a thread with [`PARSE_STACK`], passing on its panic.
#[allow(clippy::expect_used)] // ratchet: branchyard-cli
fn on_parse_stack<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    std::thread::Builder::new()
        .name("by-args".into())
        .stack_size(PARSE_STACK)
        .spawn(f)
        .expect("could not start the argument parser's thread")
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

#[allow(clippy::expect_used)] // ratchet: branchyard-cli
fn parse_argv(argv: Vec<OsString>) -> Result<Cli, clap::Error> {
    let mut cmd = build_command();
    let matches = cmd
        .try_get_matches_from_mut(argv.iter().cloned())
        .map_err(|error| with_step_tip(with_prompt_tip(error, &cmd, &argv)))?;
    Cli::from_arg_matches(&matches).map_err(|error| {
        // Checked flags fail here, after clap's own checks; format the
        // error with the command whose usage explains it.
        let mut sub = &mut cmd;
        let mut m = &matches;
        while let Some((name, next)) = m.subcommand() {
            sub = sub
                .find_subcommand_mut(name)
                .expect("matched subcommands exist");
            m = next;
        }
        error.format(sub)
    })
}

/// A stray word after a prompt is most likely an unquoted prompt.
fn with_prompt_tip(mut error: clap::Error, cmd: &clap::Command, argv: &[OsString]) -> clap::Error {
    if error.kind() != ErrorKind::UnknownArgument {
        return error;
    }
    match error.get(ContextKind::InvalidArg) {
        Some(ContextValue::String(extra)) if !extra.starts_with('-') => {}
        _ => return error,
    }
    let takes_prompt = argv.iter().skip(1).find_map(|arg| {
        let sub = cmd.find_subcommand(arg.to_str()?)?;
        Some(
            sub.get_positionals()
                .any(|p| matches!(p.get_id().as_str(), "prompt" | "text")),
        )
    });
    if takes_prompt == Some(true) {
        error.insert(
            ContextKind::Suggested,
            ContextValue::StyledStrs(vec!["quote a prompt that contains spaces".into()]),
        );
    }
    error
}

/// `by init`'s protocol steps are one per call: say so beside clap's
/// conflict, and name the topics when a step lacks one.
fn with_step_tip(mut error: clap::Error) -> clap::Error {
    let steps = ["--next", "--dry-run", "--apply"];
    let names = |kind| match error.get(kind) {
        Some(ContextValue::String(arg)) => vec![arg.clone()],
        Some(ContextValue::Strings(args)) => args.clone(),
        _ => Vec::new(),
    };
    let is_step = |arg: &String| steps.iter().any(|s| arg.starts_with(s));
    let tip = match error.kind() {
        ErrorKind::ArgumentConflict
            if names(ContextKind::InvalidArg).iter().any(is_step)
                && names(ContextKind::PriorArg).iter().any(is_step) =>
        {
            "--next, --dry-run and --apply are separate steps; give one".to_owned()
        }
        ErrorKind::MissingRequiredArgument
            if names(ContextKind::InvalidArg)
                .iter()
                .any(|a| a == "<TOPIC>") =>
        {
            let ids: Vec<&str> = branchyard_setup::Topic::ALL
                .iter()
                .map(|t| t.id())
                .collect();
            format!("give a topic: {}", ids.join(", "))
        }
        _ => return error,
    };
    error.insert(
        ContextKind::Suggested,
        ContextValue::StyledStrs(vec![tip.into()]),
    );
    error
}

/// Where `by serve`, `by worker` or `by help serve|worker` hands over to
/// the server's own parser; see [`server_call`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerCall {
    /// How many leading arguments (after the program name) are `by`'s own
    /// to parse: global options, and the command unless `help`.
    pub prefix: usize,
    /// `by serve` or `by worker`, naming the server in its messages.
    pub program: &'static str,
    /// The server's arguments, `--worker` first for `by worker`.
    pub args: Vec<String>,
    /// `by help serve|worker`: print the server's help instead.
    pub help: bool,
}

/// `by [GLOBALS] serve|worker ARGS...`, `by [GLOBALS] help serve|worker`:
/// the server parses its own options (some share names with `by`'s global
/// ones, such as `--repo`), so they are handed over whole. `args` are after
/// the program name.
pub fn server_call(args: &[OsString]) -> Option<ServerCall> {
    let mut at = 0;
    while let Some(arg) = args.get(at).and_then(|a| a.to_str()) {
        let Some(long) = arg.strip_prefix("--") else {
            break;
        };
        let name = long.split_once('=').map_or(long, |(name, _)| name);
        if !["remote", "token-file", "repo", "ca-file"].contains(&name) {
            break;
        }
        at += if long.contains('=') { 1 } else { 2 };
    }
    let rest: Vec<&str> = args
        .get(at..)?
        .iter()
        .map(|a| a.to_str())
        .collect::<Option<_>>()?;
    let (help, name) = match rest.as_slice() {
        ["help", name, ..] => (true, *name),
        [name, ..] => (false, *name),
        [] => return None,
    };
    let program = match name {
        "serve" => "by serve",
        "worker" => "by worker",
        _ => return None,
    };
    let mut server_args = Vec::new();
    if name == "worker" {
        server_args.push("--worker".to_owned());
    }
    if !help {
        server_args.extend(rest[1..].iter().map(|a| (*a).to_owned()));
    }
    Some(ServerCall {
        prefix: if help { at } else { at + 1 },
        program,
        args: server_args,
        help,
    })
}

pub use branchyard_setup::config::split_words;

/// Quote `word` for a POSIX shell, leaving plain words as they are.
pub use branchyard_recipe::quote as shell_quote;

// Value parsers. Each error reads after "invalid value 'X' for '--flag
// <VALUE>': ".

fn non_blank(text: &str) -> Result<String, String> {
    match text.trim().is_empty() {
        true => Err("needs a value".into()),
        false => Ok(text.to_owned()),
    }
}

fn prompt_text(text: &str) -> Result<String, String> {
    match text.trim().is_empty() {
        true => Err("needs a prompt".into()),
        false => Ok(text.to_owned()),
    }
}

fn check_argv(line: &str) -> Result<Argv, String> {
    let argv = split_words(line)?;
    match argv.is_empty() {
        true => Err("needs a command".into()),
        false => Ok(Argv(argv)),
    }
}

fn command_argv(line: &str) -> Result<Argv, String> {
    let argv = split_words(line)?;
    match argv.is_empty() {
        true => Err("needs an executable".into()),
        false => Ok(Argv(argv)),
    }
}

/// Dollars, with or without a leading `$`.
fn usd(text: &str) -> Result<f64, String> {
    match text.trim().trim_start_matches('$').parse::<f64>() {
        Ok(usd) if usd.is_finite() && usd > 0.0 => Ok(usd),
        _ => Err("needs a positive number of dollars, such as 2.50".into()),
    }
}

fn positive_turns(text: &str) -> Result<u32, String> {
    match text.parse::<u32>() {
        Ok(turns) if turns > 0 => Ok(turns),
        _ => Err("needs a positive whole number".into()),
    }
}

fn delegate_depth(text: &str) -> Result<u32, String> {
    match text.parse::<u32>() {
        Ok(depth) if depth > 0 => Ok(depth),
        _ => Err("needs a positive whole number".into()),
    }
}

fn cpus(text: &str) -> Result<u8, String> {
    match text.parse::<u8>() {
        Ok(cpus) if cpus > 0 => Ok(cpus),
        _ => Err("needs a whole number from 1 to 255".into()),
    }
}

fn memory(text: &str) -> Result<u32, String> {
    match text.parse::<u32>() {
        Ok(mib) if mib > 0 => Ok(mib),
        _ => Err("needs a positive whole number of MiB".into()),
    }
}

/// Non-negative seconds.
fn seconds(text: &str) -> Result<f64, String> {
    match text.parse::<f64>() {
        Ok(secs) if secs.is_finite() && secs >= 0.0 => Ok(secs),
        _ => Err("needs a number of seconds, such as 30 or 2.5".into()),
    }
}

/// `text` as a duration: a number in `unit` seconds, or with a unit of its
/// own: `ms`, `s`, `m` or `h`. `None` unless finite and positive.
fn duration(text: &str, unit: f64) -> Option<Result<Duration, String>> {
    let text = text.trim();
    // A trailing unit only: "1e3" is a number.
    let number = text.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let suffix = &text[number.len()..];
    let scale = match suffix {
        "" => unit,
        "ms" => 0.001,
        "s" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        _ => return None,
    };
    let value = number.trim().parse::<f64>().ok()?;
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    Some(Duration::try_from_secs_f64(value * scale).map_err(|_| "is too large".to_owned()))
}

fn minutes(text: &str) -> Result<Duration, String> {
    duration(text, 60.0).unwrap_or_else(|| {
        Err("needs a positive number of minutes, or a duration such as 90s or 2h".into())
    })
}

fn max_minutes(text: &str) -> Result<Duration, String> {
    minutes(text)
}

fn stall_after(text: &str) -> Result<Duration, String> {
    minutes(text)
}

/// `by report-status --ttl`: a duration such as `10m`; the engine checks
/// its bounds, so a bad one is refused `denied` as the tool's is.
fn report_ttl(text: &str) -> Result<Duration, String> {
    branchyard_support::time::parse_duration(text).map_err(|e| e.to_string())
}

fn watch_interval(text: &str) -> Result<Duration, String> {
    duration(text, 1.0)
        .and_then(Result::ok)
        .filter(|d| (0.05..=3600.0).contains(&d.as_secs_f64()))
        .ok_or_else(|| "needs a number of seconds from 0.05 to 3600, such as 0.5 or 250ms".into())
}

/// Split `claude-code,codex` into IDs. Duplicates are refused because
/// branch names derive from the harness.
fn harness_list(list: &str) -> Result<List, String> {
    let mut ids: Vec<String> = Vec::new();
    for id in list.split(',').map(str::trim) {
        if id.is_empty() {
            return Err(format!("has an empty entry in '{list}'"));
        }
        if ids.iter().any(|seen| seen == id) {
            return Err(format!("lists {id} twice"));
        }
        ids.push(id.to_owned());
    }
    Ok(List(ids))
}

fn variable_names(list: &str) -> Result<List, String> {
    let names: Vec<String> = list.split(',').map(|n| n.trim().to_owned()).collect();
    if names.iter().any(|n| n.is_empty() || n.contains('=')) {
        return Err("takes variable names, such as ANTHROPIC_API_KEY,GH_TOKEN".into());
    }
    Ok(List(names))
}

fn label(text: &str) -> Result<(String, String), String> {
    text.split_once('=')
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .ok_or_else(|| "needs KEY=VALUE".into())
}

#[allow(clippy::unwrap_in_result)] // tests: a panic is the failure report
#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(line: &str) -> Result<Command, clap::Error> {
        let mut argv = vec!["by".to_owned()];
        argv.extend(split_words(line).unwrap());
        parse_from(argv).map(|cli| cli.command.expect("a command"))
    }

    fn err(line: &str) -> String {
        match parse_str(line) {
            Ok(command) => panic!("{line}: parsed as {command:?}"),
            Err(error) => error.to_string(),
        }
    }

    fn kind(line: &str) -> ErrorKind {
        parse_str(line).unwrap_err().kind()
    }

    fn task(line: &str) -> TaskArgs {
        match parse_str(line).unwrap() {
            Command::Run { task, .. } => task.into_inner(),
            Command::Fan { task, .. } => task.into_inner(),
            Command::Fork { task, .. } => task.into_inner(),
            Command::Reincarnate { task, .. } => task.into_inner(),
            Command::Send { task, .. } => task.into_inner(),
            Command::Review { task, .. } => task.into_inner(),
            other => panic!("{other:?} has no task"),
        }
    }

    #[test]
    fn the_command_tree_is_consistent() {
        command().debug_assert();
    }

    /// `--no-preflight` skips the check made before a harness starts, on the
    /// commands that make it (`crate::preflight`) and on no other.
    #[test]
    fn no_preflight_is_a_flag_of_the_commands_that_check_the_harness() {
        for line in ["run go", "fan go --harness a,b", "send b go"] {
            assert!(!task(line).no_preflight, "{line}");
            assert!(
                task(&format!("{line} --no-preflight")).no_preflight,
                "{line}"
            );
        }
        for line in ["fork b go", "map go", "reincarnate b"] {
            assert_eq!(
                kind(&format!("{line} --no-preflight")),
                ErrorKind::UnknownArgument
            );
        }
    }

    #[test]
    fn map_runs_a_prompt_or_takes_an_action() {
        let Command::Map {
            action: None,
            map,
            json: false,
        } = parse_str(
            "map 'look at {{item.repo}}' --items r.csv --schema s.json --out o.csv \
             --concurrency 8 --retries 2 --total-usd 5 --reduce 'sum up' --rm -n look \
             --harness codex --budget-usd 1 --yes",
        )
        .unwrap()
        else {
            panic!("not a map run");
        };
        assert_eq!(map.prompt.as_deref(), Some("look at {{item.repo}}"));
        assert_eq!(map.items.as_deref(), Some("r.csv"));
        assert_eq!(map.concurrency, Some(8));
        assert_eq!(map.retries, Some(2));
        assert_eq!(map.total_usd, Some(5.0));
        assert!(map.rm);
        let task = map.task.clone().into_inner();
        assert_eq!(task.name.as_deref(), Some("look"));
        assert_eq!(task.harness.as_deref(), Some("codex"));
        assert_eq!(task.budget_usd, Some(1.0));
        assert_eq!(task.permissions, Permissions::Yes);
        assert!(matches!(
            parse_str("map resume look --retry-failed --json").unwrap(),
            Command::Map {
                action: Some(MapAction::Resume {
                    retry_failed: true,
                    ..
                }),
                json: true,
                ..
            }
        ));
        assert!(matches!(
            parse_str("map ls").unwrap(),
            Command::Map {
                action: Some(MapAction::Ls),
                ..
            }
        ));
        assert_eq!(kind("map"), ErrorKind::MissingRequiredArgument);
        assert_eq!(kind("map x --concurrency 0"), ErrorKind::ValueValidation);
        assert!(err("map x --items a --from-command b").contains("cannot be used with"));
        assert!(err("map x --reduce-out f").contains("--reduce"));
        assert!(err("map x --input-format xml").contains("not an item format"));
    }

    #[test]
    fn run_takes_every_task_option() {
        let Command::Run { prompt, task, .. } = parse_str(
            "run 'fix the flaky test' --harness codex --name flaky --base main \
             --check 'cargo test -p core' --budget-usd 2.5 --max-turns 3 --yes \
             --max-minutes 1.5 --isolated --command '/opt/codex/bin/codex --flag'",
        )
        .unwrap() else {
            panic!("not run")
        };
        assert_eq!(prompt, "fix the flaky test");
        assert_eq!(
            *task,
            TaskArgs {
                harness: Some("codex".into()),
                name: Some("flaky".into()),
                base: Some("main".into()),
                check: Some(vec![
                    "cargo".into(),
                    "test".into(),
                    "-p".into(),
                    "core".into()
                ]),
                budget_usd: Some(2.5),
                max_turns: Some(3),
                max_duration: Some(Duration::from_secs(90)),
                stall_after: None,
                stall_action: branchyard::StallAction::Notify,
                permissions: Permissions::Yes,
                isolated: true,
                isolation_off: false,
                command: Some(vec!["/opt/codex/bin/codex".into(), "--flag".into()]),
                sandbox: None,
                substrate: None,
                recipe: None,
                local: false,
                delegate: None,
                allow_delegation: false,
                no_wake: false,
                unapproved_tools: false,
                deny: Vec::new(),
                provision: None,
                instructions: None,
                issue: None,
                pr: None,
                require_labels: Vec::new(),
                priority: None,
                auto: false,
                implied_auto: false,
                kind: None,
                seed: None,
                fleet: None,
                fleet_excluded: Vec::new(),
                plan: false,
                goal: None,
                goal_rounds: None,
                goal_judge: None,
                goal_judge_command: None,
                no_preflight: false,
            }
        );
    }

    /// `--isolated` alone means yes; `--isolated=false` says no, and says
    /// it was said, so `[defaults] isolated = true` does not win.
    #[test]
    fn isolated_takes_an_optional_value() {
        let isolated = |line: &str| {
            let task = task(line);
            (task.isolated, task.isolation_off)
        };
        assert_eq!(isolated("run go"), (false, false));
        assert_eq!(isolated("run go --isolated"), (true, false));
        assert_eq!(isolated("run go --isolated=true"), (true, false));
        assert_eq!(isolated("run go --isolated=yes"), (true, false));
        assert_eq!(isolated("run go --isolated=false"), (false, true));
        assert_eq!(isolated("run go --isolated=no"), (false, true));
        // Without the `=`, the next word is still the prompt.
        assert_eq!(isolated("run --isolated go"), (true, false));
        assert!(err("run go --isolated=maybe").contains("--isolated"));
    }

    #[test]
    fn short_flags_and_value_formats() {
        let short = task("run go -n flaky -b main -y --budget-usd $1.25 --max-minutes 90s");
        assert_eq!(short.name.as_deref(), Some("flaky"));
        assert_eq!(short.base.as_deref(), Some("main"));
        assert_eq!(short.permissions, Permissions::Yes);
        assert_eq!(short.budget_usd, Some(1.25));
        assert_eq!(short.max_duration, Some(Duration::from_secs(90)));
        let stall = task("run go --stall-after 2h --stall-action interrupt");
        assert_eq!(stall.stall_after, Some(Duration::from_secs(7200)));
        assert_eq!(stall.stall_action, branchyard::StallAction::Interrupt);
        assert!(err("run go --stall-action interrupt").contains("--stall-after"));
        assert!(err("run go --stall-after 1 --stall-action later").contains("[possible values"));
        assert!(err("run go --max-minutes 5d").contains("positive number of minutes"));
    }

    #[test]
    fn provisioning_flags_repeat_and_parse() {
        let task = task(
            "run go --isolated --secret ANTHROPIC_API_KEY --secret CODEX_AUTH=@/run/auth.json \
             --secret OPENAI_API_KEY=MY_KEY --mcp 'docs=/usr/bin/docs-mcp --stdio' \
             --auth api-key --model large --effort 80 --telemetry http://127.0.0.1:4317 \
             --instructions rules.md",
        );
        let spec = task.provision.unwrap();
        assert_eq!(spec.secrets.len(), 3);
        assert_eq!(
            spec.secrets[1].from,
            Some(branchyard::SecretFrom::File {
                path: "/run/auth.json".into()
            })
        );
        assert_eq!(spec.mcp_servers[0].args, ["--stdio"]);
        assert_eq!(spec.auth.as_deref(), Some("api-key"));
        assert_eq!(spec.effort, Some(branchyard::Effort::Xhigh));
        assert_eq!(spec.telemetry.unwrap().endpoint(), "http://127.0.0.1:4317");
        assert_eq!(task.instructions.as_deref(), Some("rules.md"));
        assert!(self::task("send b go").provision.is_none());
        assert!(self::task("send b go --instructions r.md")
            .provision
            .is_some());
        for (line, error) in [
            ("run go --mcp docs=relative", "absolute command"),
            ("run go --secret 1BAD", "--secret"),
            ("run go --effort max", "--effort"),
            ("run go --telemetry collector:4317", "--telemetry"),
            ("run go --model ' '", "--model"),
        ] {
            assert!(err(line).contains(error), "{line}: {}", err(line));
        }
        assert_eq!(
            kind("run go --model a --model b"),
            ErrorKind::ArgumentConflict
        );
        assert!(err("run go --model a --model b").contains("cannot be used multiple times"));
    }

    #[test]
    fn the_model_gateway_flag_parses() {
        for line in [
            "run go --model-gateway",
            "fan go --harness a,b --model-gateway",
            "fork b go --model-gateway",
            "send b go --model-gateway",
        ] {
            let spec = task(line).provision.unwrap();
            assert_eq!(
                spec.models,
                Some(branchyard::models::ModelAccess::all()),
                "{line}"
            );
        }
        let spec = task("run go --model-gateway='claude-*,gpt-5'")
            .provision
            .unwrap();
        assert_eq!(spec.models.unwrap().allow, ["claude-*", "gpt-5"]);
        assert_eq!(task("run go").provision, None);
        assert!(err("run go --model-gateway='a b'").contains("' '"));
        assert_eq!(
            parse_str("models --period day --json").unwrap(),
            Command::Models {
                period: "day".into(),
                json: true,
                action: None
            }
        );
        assert!(err("models --period week").contains("week"));
        // `list` is a subcommand, and --json goes after it.
        let Command::Models { json, action, .. } = parse_str("models list --json").unwrap() else {
            panic!("models list --json");
        };
        assert!(json);
        assert_eq!(
            action,
            Some(ModelsAction::List {
                refresh: false,
                harness: None,
                gemini_url: "https://generativelanguage.googleapis.com".into(),
                openai_url: "https://api.openai.com".into(),
                anthropic_url: "https://api.anthropic.com".into(),
            })
        );
        let Command::Models { action, .. } =
            parse_str("models list --refresh --harness codex --openai-url http://127.0.0.1:9")
                .unwrap()
        else {
            panic!("models list");
        };
        let Some(ModelsAction::List {
            refresh,
            harness,
            openai_url,
            ..
        }) = action
        else {
            panic!("models list");
        };
        assert!(refresh);
        assert_eq!(harness.as_deref(), Some("codex"));
        assert_eq!(openai_url, "http://127.0.0.1:9");
        // The usage view's flag does not go with a subcommand.
        assert!(parse_str("models --period day list").is_err());
    }

    #[test]
    fn network_and_permission_preset_flags_parse() {
        for line in [
            "run go --network 'github.com,*.npmjs.org:443' --network-enforce required",
            "fan go --harness a,b --network 'github.com,*.npmjs.org:443' --network-enforce required",
            "fork b go --network 'github.com,*.npmjs.org:443' --network-enforce required",
            "send b go --network 'github.com,*.npmjs.org:443' --network-enforce required",
        ] {
            let spec = task(line).provision.unwrap_or_else(|| panic!("{line}"));
            assert_eq!(
                spec.network.unwrap().to_string(),
                "github.com, *.npmjs.org:443 (required)",
                "{line}"
            );
        }
        let none = task("run go --network none").provision.unwrap();
        assert_eq!(none.network, Some(branchyard::Network::none()));
        for (line, error) in [
            ("run go --network https://x.com", "not a URL"),
            ("run go --network '*'", "\"open\""),
            ("run go --network-enforce required", "--network"),
            (
                "run go --network none --network-enforce always",
                "best_effort",
            ),
            (
                "run go --permissions yolo",
                "read-only, edit-worktree, full",
            ),
            ("run go --permissions full --yes", "cannot be used with"),
        ] {
            assert!(err(line).contains(error), "{line}: {}", err(line));
        }
        for (line, preset) in [
            (
                "run go --permissions read-only",
                branchyard::PolicyPreset::ReadOnly,
            ),
            (
                "send b go --permissions edit-worktree",
                branchyard::PolicyPreset::EditWorktree,
            ),
            (
                "fan go --harness a,b --permissions full",
                branchyard::PolicyPreset::Full,
            ),
        ] {
            assert_eq!(
                task(line).permissions,
                Permissions::Preset(preset),
                "{line}"
            );
        }
    }

    #[test]
    fn connector_flags_repeat_and_parse() {
        for line in [
            "run go --isolated --connector github --connector 'github:write+confirm:issues.create'",
            "fan go --harness a,b --isolated --connector github --connector 'github:write+confirm:issues.create'",
            "fork b go --connector github --connector 'github:write+confirm:issues.create'",
            "send b go --connector github --connector 'github:write+confirm:issues.create'",
        ] {
            let spec = task(line).provision.unwrap_or_else(|| panic!("{line}"));
            let grants: Vec<String> = spec.connectors.iter().map(|g| g.to_string()).collect();
            assert_eq!(
                grants,
                ["github:read", "github:write+confirm:issues.create"],
                "{line}"
            );
        }
        for (line, error) in [
            ("run go --connector github:admin", "mode"),
            ("run go --connector 'a b'", "connector id"),
            ("run go --connector github:read:", "operation"),
        ] {
            assert!(err(line).contains(error), "{line}: {}", err(line));
        }
        assert!(matches!(
            parse_str("gateway status --json").unwrap(),
            Command::Gateway {
                json: true,
                action: GatewayAction::Status
            }
        ));
        assert!(matches!(
            parse_str("gateway rotate-key --keep 2").unwrap(),
            Command::Gateway {
                action: GatewayAction::RotateKey { keep: 2 },
                ..
            }
        ));
        assert_eq!(
            parse_str("connect github --account work --api-key-stdin").unwrap(),
            Command::Connect {
                connector: "github".into(),
                account: Some("work".into()),
                api_key_stdin: true,
                open: false,
            }
        );
    }

    #[test]
    fn substrate_flags_configure_tls_and_the_insecure_escape() {
        let task = task(
            "run go --provider substrate --substrate-endpoint https://control:443 \
             --substrate-router 'wss://router/{atespace}/{actor}/' --substrate-template t \
             --substrate-key k --substrate-ca ca.pem --substrate-client-cert c.pem \
             --substrate-client-key c.key --substrate-router-ca router-ca.pem",
        );
        let substrate = task.substrate.unwrap();
        assert_eq!(substrate.endpoint, "https://control:443");
        assert_eq!(substrate.ca.as_deref(), Some("ca.pem"));
        assert_eq!(substrate.client_cert.as_deref(), Some("c.pem"));
        assert_eq!(substrate.client_key.as_deref(), Some("c.key"));
        assert_eq!(substrate.router_ca.as_deref(), Some("router-ca.pem"));
        assert!(!substrate.insecure);
        let task = self::task(
            "fork b go --provider substrate --substrate-endpoint http://10.0.0.1:8080 \
             --substrate-router 'http://10.0.0.2/{actor}/' --substrate-template t \
             --substrate-key k --substrate-insecure",
        );
        let substrate = task.substrate.unwrap();
        assert!(substrate.insecure && substrate.ca.is_none());
        assert!(err("run go --substrate-insecure")
            .contains("--substrate-insecure needs --provider substrate"));
        assert!(err("run go --provider local --substrate-ca ca.pem")
            .contains("--substrate-ca needs --provider substrate"));
        assert!(err("run go --provider substrate --substrate-endpoint e")
            .contains("--provider substrate needs --substrate-router"));
        assert_eq!(
            kind("run go --substrate-insecure"),
            ErrorKind::ArgumentConflict
        );
    }

    #[test]
    fn substrate_flags_select_token_service_name_and_egress() {
        let base = "run go --provider substrate --substrate-endpoint https://control \
            --substrate-router https://router --substrate-template agy --substrate-key bridge.key";
        let chosen = task(&format!(
            "{base} --substrate-api-token-file control.token \
            --substrate-server-name ate-api.ate-system.svc --substrate-allow-host api.example.com \
            --substrate-allow-host '*.googleapis.com'"
        ));
        let saved = chosen.substrate.unwrap();
        assert_eq!(saved.api_token_file.as_deref(), Some("control.token"));
        assert_eq!(saved.server_name.as_deref(), Some("ate-api.ate-system.svc"));
        assert_eq!(
            saved.egress,
            branchyard::config::SubstrateEgress::AllowHosts(vec![
                "api.example.com".into(),
                "*.googleapis.com".into()
            ])
        );
        assert_eq!(
            task(base).substrate.unwrap().egress,
            branchyard::config::SubstrateEgress::Deny
        );
        assert_eq!(
            task(&format!("{base} --substrate-egress unrestricted"))
                .substrate
                .unwrap()
                .egress,
            branchyard::config::SubstrateEgress::Unrestricted
        );
        assert!(err(&format!(
            "{base} --substrate-egress unrestricted --substrate-allow-host api.example.com"
        ))
        .contains("cannot be used with"));
        assert!(err("run go --substrate-api-token-file t").contains("needs --provider substrate"));
    }

    #[test]
    fn provider_flags_select_and_configure_a_sandbox() {
        let task = task(
            "run go --provider microsandbox --image ghcr.io/x/claude:1 --cpus 2 \
             --memory 4096 --pass-env 'ANTHROPIC_API_KEY, GH_TOKEN'",
        );
        assert_eq!(
            task.sandbox,
            Some(SandboxArgs {
                image: "ghcr.io/x/claude:1".into(),
                cpus: Some(2),
                memory_mib: Some(4096),
                pass_env: vec!["ANTHROPIC_API_KEY".into(), "GH_TOKEN".into()],
                ..SandboxArgs::default()
            })
        );
        let kept = self::task(
            "run go --provider microsandbox --image a --live-branch --keep-sandbox pause \
             --sandbox-snapshots 2 --max-paused 6 --max-kept-minutes 90",
        );
        assert_eq!(
            kept.sandbox,
            Some(SandboxArgs {
                image: "a".into(),
                lifecycle: LifecycleArgs {
                    keep: Some(branchyard::SandboxKeep::Pause),
                    snapshots: Some(2),
                    max_paused: Some(6),
                    max_kept_minutes: Some(90),
                },
                live_branch: true,
                ..SandboxArgs::default()
            })
        );
        assert!(err("run go --keep-sandbox pause")
            .contains("--keep-sandbox needs --provider microsandbox, substrate or recipe:NAME"));
        assert!(
            err("run go --provider microsandbox --image a --keep-sandbox forever")
                .contains("not pause or destroy")
        );
        assert!(err("run go --live-branch").contains("--live-branch needs --provider microsandbox"));
        assert!(
            err("run go --provider microsandbox --image a --max-paused 0").contains("--max-paused")
        );
        assert!(!task.local);
        let task = self::task("fork b go --provider local");
        assert!(task.local && task.sandbox.is_none());
        assert!(
            err("run go --provider microsandbox").contains("--provider microsandbox needs --image")
        );
        assert!(err("run go --image alpine").contains("--image needs --provider microsandbox"));
        assert!(err("run go --provider local --cpus 2")
            .contains("--cpus needs --provider microsandbox"));
        assert!(err("run go --provider local --pass-env A")
            .contains("--pass-env needs --provider microsandbox, substrate or recipe:NAME"));
        let docker = err("run go --provider docker");
        assert!(
            docker.contains("[possible values: local, microsandbox, substrate, recipe:NAME]"),
            "{docker}"
        );
        assert!(err("run go --provider microsandbox --image a --cpus 0").contains("--cpus"));
        assert!(err("run go --provider microsandbox --image a --memory 1g").contains("--memory"));
        assert!(err("run go --provider microsandbox --image a --pass-env A=1").contains("names"));
        assert_eq!(
            kind("send b go --provider local"),
            ErrorKind::UnknownArgument
        );
    }

    #[test]
    fn provider_recipe_names_a_recipe_and_its_paths() {
        let task = self::task(
            "run go --provider recipe:devbox --recipe-workdir /srv/work --pass-env TOKEN \
             --keep-sandbox pause --max-paused 2",
        );
        assert_eq!(
            task.recipe,
            Some(RecipeArgs {
                name: "devbox".into(),
                workdir: Some("/srv/work".into()),
                home: None,
                pass_env: vec!["TOKEN".into()],
                lifecycle: LifecycleArgs {
                    keep: Some(branchyard::SandboxKeep::Pause),
                    snapshots: None,
                    max_paused: Some(2),
                    max_kept_minutes: None,
                },
            })
        );
        assert!(task.sandbox.is_none() && task.substrate.is_none() && !task.local);
        let fan = self::task("fan go --harness codex,claude --provider recipe:lab.box");
        assert_eq!(fan.recipe.map(|r| r.name), Some("lab.box".into()));
        assert!(err("run go --provider recipe:").contains("not a recipe name"));
        assert!(err("run go --provider recipe:Dev").contains("not a recipe name"));
        assert!(err("run go --recipe-workdir /w").contains("needs --provider recipe:NAME"));
        assert!(
            err("run go --provider recipe:devbox --recipe-home home").contains("must be absolute")
        );
        assert!(
            err("run go --provider recipe:devbox --sandbox-snapshots 2").contains("no snapshots")
        );
        assert!(err("run go --provider recipe:devbox --image a")
            .contains("--image needs --provider microsandbox"));
    }

    #[test]
    fn delegate_takes_an_optional_inline_depth() {
        assert_eq!(task("run go").delegate, None);
        assert_eq!(task("run go --delegate").delegate, Some(1));
        let Command::Run { prompt, task, .. } = parse_str("run --delegate go").unwrap() else {
            panic!("not run")
        };
        assert_eq!(prompt, "go", "the prompt is not a depth");
        assert_eq!(task.delegate, Some(1));
        assert_eq!(self::task("run go --delegate=3").delegate, Some(3));
        assert_eq!(self::task("send b go --delegate=2").delegate, Some(2));
        assert!(err("run go --delegate=0").contains("positive whole number"));
        assert!(err("run go --delegate=x").contains("positive whole number"));
        let help = help("run");
        assert!(help.contains("--delegate[=<DEPTH>]"), "{help}");
    }

    #[test]
    fn delegation_commands_parse_with_optional_branches() {
        let Command::Spawn { prompt, spawn, .. } = parse_str(
            "spawn 'fix it' --parent root --harness codex --name fix --budget-usd 0.5 \
             --max-depth 0 --deny Bash,mcp__* --wait --json --yes",
        )
        .unwrap() else {
            panic!("not spawn")
        };
        assert_eq!(prompt, "fix it");
        assert_eq!(spawn.parent.as_deref(), Some("root"));
        assert_eq!(spawn.task.harness.as_deref(), Some("codex"));
        assert_eq!(spawn.task.name.as_deref(), Some("fix"));
        assert_eq!(spawn.task.budget_usd, Some(0.5));
        assert_eq!(spawn.task.permissions, Permissions::Yes);
        assert_eq!(spawn.max_depth, Some(0));
        assert_eq!(spawn.deny, ["Bash", "mcp__*"]);
        assert!(spawn.wait && spawn.json);
        let Command::Spawn { spawn, .. } = parse_str(
            "spawn go --parent p --depends-on a,b --after integrated --bind cache:read_only \
             --bind out:exclusive_write --seat worker",
        )
        .unwrap() else {
            panic!("not spawn")
        };
        assert_eq!(spawn.depends_on, ["a", "b"]);
        assert_eq!(spawn.after, branchyard::After::Integrated);
        assert_eq!(spawn.bindings.len(), 2);
        assert!(spawn.connectors.is_empty());
        assert_eq!(spawn.model, None);
        let Command::Spawn { spawn: cheap, .. } = parse_str("spawn go --model haiku").unwrap()
        else {
            panic!("not spawn")
        };
        assert_eq!(cheap.model.as_deref(), Some("haiku"));
        assert!(err("spawn go --model ' '").contains("--model"));
        let Command::Spawn { spawn: granted, .. } =
            parse_str("spawn go --connector github:read --connector 'linear@work:write:issues.*'")
                .unwrap()
        else {
            panic!("not spawn")
        };
        let grants: Vec<String> = granted.connectors.iter().map(|g| g.to_string()).collect();
        assert_eq!(grants, ["github:read", "linear@work:write:issues.*"]);
        let Command::Spawn { spawn: owner, .. } =
            parse_str("spawn go --owns src/parser,tests/parser_*.rs --owns docs/parser.md")
                .unwrap()
        else {
            panic!("not spawn")
        };
        assert_eq!(
            owner.owns,
            ["src/parser", "tests/parser_*.rs", "docs/parser.md"]
        );
        assert!(granted.owns.is_empty());
        assert!(!owner.land);
        let Command::Spawn { spawn: lander, .. } = parse_str("spawn go --land").unwrap() else {
            panic!("not spawn")
        };
        assert!(lander.land);
        assert!(lander.review.is_none());
        let Command::Spawn { spawn: checked, .. } = parse_str(
            "spawn go --land --review codex --review-model large --review-budget-usd 0.5 \
             --review-rubric 'No new crates'",
        )
        .unwrap() else {
            panic!("not spawn")
        };
        assert_eq!(
            checked.review.as_deref(),
            Some(&branchyard::ReviewSpec {
                harness: "codex".into(),
                model: Some("large".into()),
                rubric: Some("No new crates".into()),
                budget_usd: Some(0.5),
            })
        );
        assert!(err("spawn go --review codex").contains("--land"));
        assert!(err("spawn go --land --review-model large").contains("--review"));
        let Command::Delegate { land, .. } = parse_str("delegate --plan p.json --land").unwrap()
        else {
            panic!("not delegate")
        };
        assert!(land);
        assert_eq!(spawn.seat.as_deref(), Some("worker"));
        assert!(err("spawn go --after soon").contains("[possible values: settled, integrated]"));
        assert!(err("spawn go --bind cache").contains("--bind"));
        assert!(err("spawn go --depends-on a,a").contains("lists a twice"));
        assert_eq!(
            parse_str("inspect").unwrap(),
            Command::Inspect {
                branch: None,
                json: false
            }
        );
        assert_eq!(
            parse_str("inspect kid --json").unwrap(),
            Command::Inspect {
                branch: Some("kid".into()),
                json: true
            }
        );
        assert_eq!(
            parse_str("events kid --cursor 7 --limit 3").unwrap(),
            Command::Events {
                branch: Some("kid".into()),
                cursor: Some(7),
                limit: Some(3),
                json: false
            }
        );
        assert_eq!(
            parse_str("children").unwrap(),
            Command::Children {
                branch: None,
                json: false
            }
        );
        assert_eq!(
            parse_str("check").unwrap(),
            Command::Check {
                branch: None,
                json: false
            }
        );
        assert_eq!(
            parse_str("check kid --json").unwrap(),
            Command::Check {
                branch: Some("kid".into()),
                json: true
            }
        );
        assert_eq!(
            parse_str("integrate kid --json").unwrap(),
            Command::Integrate {
                branches: vec!["kid".into()],
                json: true
            }
        );
        assert_eq!(
            parse_str("integrate a b").unwrap(),
            Command::Integrate {
                branches: vec!["a".into(), "b".into()],
                json: false
            }
        );
        assert_eq!(
            parse_str("wait a b --any --timeout 2.5 --json").unwrap(),
            Command::Wait {
                branches: vec!["a".into(), "b".into()],
                any: true,
                all: false,
                timeout: Some(2.5),
                json: true
            }
        );
        assert!(err("wait --any --all").contains("cannot be used with"));
        assert_eq!(
            parse_str("cancel kid").unwrap(),
            Command::Cancel {
                branch: "kid".into(),
                json: false
            }
        );
        assert_eq!(kind("integrate"), ErrorKind::MissingRequiredArgument);
        assert!(err("integrate").contains("<BRANCH>"));
        assert!(err("inspect a b").contains("unexpected argument 'b'"));
        assert!(err("events --cursor x").contains("invalid digit"));
        assert!(help("inspect").contains("Usage: by inspect [OPTIONS] [BRANCH]"));
        assert!(task("run go --delegate --allow-delegation").allow_delegation);
        assert!(task("send b go --allow-unapproved-tools").unapproved_tools);
        for line in [
            "run go --require-label gpu --require-label linux",
            "fan go --harness a,b --require-label gpu --require-label linux",
            "send b go --require-label gpu --require-label linux",
            "fork b go --require-label gpu --require-label linux",
            "reincarnate b --require-label gpu --require-label linux",
        ] {
            assert_eq!(task(line).require_labels, ["gpu", "linux"], "{line}");
        }
    }

    #[test]
    fn messages_take_text_and_an_acting_branch() {
        assert_eq!(
            parse_str("ask 'which lock?' --as kid --wait 2.5 --json").unwrap(),
            Command::Ask {
                as_branch: Some("kid".into()),
                text: "which lock?".into(),
                wait_seconds: Some(2.5),
                json: true
            }
        );
        assert_eq!(
            parse_str("answer 7 'the mutex'").unwrap(),
            Command::Answer {
                as_branch: None,
                message_id: 7,
                text: "the mutex".into(),
                json: false
            }
        );
        assert_eq!(
            parse_str("inbox --unread").unwrap(),
            Command::Inbox {
                as_branch: None,
                unread: true,
                json: false
            }
        );
        assert!(err("answer x hi").contains("<MESSAGE_ID>"));
        assert!(err("ask hi --wait -1").contains("needs a number of seconds"));
    }

    #[test]
    fn send_steer_is_a_switch() {
        let Command::Send {
            steer, json, task, ..
        } = parse_str("send b --steer --json also-this").unwrap()
        else {
            panic!("not send")
        };
        assert!(steer && json);
        assert_eq!(*task, TaskArgs::default());
        let Command::Send { steer, .. } = parse_str("send b go").unwrap() else {
            panic!("not send")
        };
        assert!(!steer);
    }

    #[test]
    fn send_and_steer_take_a_prompt_file() {
        let Command::Send {
            steer, prompt_file, ..
        } = parse_str("send b --steer --prompt-file note.md").unwrap()
        else {
            panic!("not send")
        };
        assert!(steer);
        assert_eq!(prompt_file.as_deref(), Some("note.md"));
        let Command::Send { prompt_file, .. } = parse_str("send b --prompt-file -").unwrap() else {
            panic!("not send")
        };
        assert_eq!(prompt_file.as_deref(), Some("-"));
        assert!(err("send b go --prompt-file f").contains("cannot be used with"));
        assert!(err("send b --retry --prompt-file f").contains("cannot be used with"));
        assert_eq!(
            parse_str("steer b also --json").unwrap(),
            Command::Steer {
                branch: "b".into(),
                prompt: "also".into(),
                prompt_file: None,
                json: true
            }
        );
        let Command::Steer { prompt_file, .. } = parse_str("steer b --prompt-file f").unwrap()
        else {
            panic!("not steer")
        };
        assert_eq!(prompt_file.as_deref(), Some("f"));
        assert_eq!(kind("steer b"), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn mcp_needs_a_root_and_a_branch() {
        assert_eq!(
            parse_str("mcp --root /r --branch b").unwrap(),
            Command::Mcp {
                root: Some("/r".into()),
                branch: Some("b".into()),
                list_tools: false,
            }
        );
        assert_eq!(kind("mcp --root /r"), ErrorKind::MissingRequiredArgument);
        assert!(err("mcp --root /r").contains("--branch <NAME>"));
        // --list-tools stands alone: no repository, branch or token.
        assert_eq!(
            parse_str("mcp --list-tools").unwrap(),
            Command::Mcp {
                root: None,
                branch: None,
                list_tools: true,
            }
        );
        assert_eq!(
            kind("mcp --list-tools --root /r"),
            ErrorKind::ArgumentConflict
        );
    }

    /// `--deny` takes a comma list and repeats, on every command that has
    /// it; a repeat failed with "cannot be used multiple times".
    #[test]
    fn deny_repeats_as_well_as_a_comma_list() {
        let Command::Spawn { spawn, .. } =
            parse_str("spawn go --parent p --deny Edit --deny Write,Bash").unwrap()
        else {
            panic!("not spawn")
        };
        assert_eq!(spawn.deny, ["Edit", "Write", "Bash"]);
        assert_eq!(
            task("run go --deny Edit --deny Write,Bash").deny,
            ["Edit", "Write", "Bash"]
        );
        assert_eq!(task("run go --deny Edit,Write").deny, ["Edit", "Write"]);
    }

    #[test]
    fn run_defaults_and_equals_form() {
        let Command::Run { prompt, task, .. } = parse_str("run --ask --name=x \"do it\"").unwrap()
        else {
            panic!("not run")
        };
        assert_eq!(prompt, "do it");
        assert_eq!(task.name.as_deref(), Some("x"));
        assert_eq!(task.permissions, Permissions::Ask);
        assert_eq!(self::task("run go"), TaskArgs::default());
    }

    #[test]
    fn fan_takes_a_harness_list_or_routes() {
        let Command::Fan {
            harnesses, task, ..
        } = parse_str("fan go --harness 'claude-code, codex' --max-turns 2").unwrap()
        else {
            panic!("not fan")
        };
        assert_eq!(*harnesses.unwrap(), ["claude-code", "codex"]);
        assert_eq!(task.harness, None);
        assert_eq!(task.max_turns, Some(2));
        // Without --harness it routes through [fleet]; the command says so
        // when there is none.
        let Command::Fan {
            harnesses,
            task,
            attempts,
            judge,
            ..
        } = parse_str("fan go --auto --kind bugfix --attempts 3 --judge --seed 9").unwrap()
        else {
            panic!("not fan")
        };
        assert_eq!(harnesses, None);
        assert!(task.auto && judge);
        assert_eq!(
            (task.kind, task.seed, attempts),
            (Some(branchyard::TaskKind::Bugfix), Some(9), Some(3))
        );
        assert!(err("fan go --attempts 0").contains("0"));
        assert!(err("fan go --harness codex,codex").contains("lists codex twice"));
        assert!(err("fan go --harness codex,").contains("empty entry"));
    }

    #[test]
    fn send_and_fork_take_a_branch_and_a_prompt() {
        let Command::Send {
            branch,
            prompt,
            prompt_file,
            task,
            steer,
            retry,
            wait,
            json,
            detach,
        } = parse_str("send flaky 'now add a test' --yes").unwrap()
        else {
            panic!("not send")
        };
        assert_eq!(
            (branch.as_str(), prompt.as_str()),
            ("flaky", "now add a test")
        );
        assert_eq!(
            *task,
            TaskArgs {
                permissions: Permissions::Yes,
                ..TaskArgs::default()
            }
        );
        assert!(!steer && !retry && !wait && !json && !detach && prompt_file.is_none());
        // --retry takes the cut-off turn's prompt, so none is given with it.
        let Command::Send { retry, prompt, .. } = parse_str("send flaky --retry").unwrap() else {
            panic!("not send")
        };
        assert!(retry && prompt.is_empty());
        assert!(parse_str("send flaky").is_err());
        assert!(parse_str("send flaky go --retry").is_err());
        assert!(parse_str("send flaky --retry --steer").is_err());
        let Command::Fork {
            branch,
            prompt,
            fresh_session,
            at: None,
            task,
            ..
        } = parse_str("fork flaky 'try another way' --fresh-session --name alt").unwrap()
        else {
            panic!("not fork")
        };
        assert_eq!(
            (branch.as_str(), prompt.as_str()),
            ("flaky", "try another way")
        );
        assert!(fresh_session);
        assert_eq!(
            *task,
            TaskArgs {
                name: Some("alt".into()),
                ..TaskArgs::default()
            }
        );
        assert!(err("send flaky").contains("<PROMPT>"));
        assert!(err("fork").contains("<BRANCH>"));
        assert!(err("send flaky go --harness codex").contains("unexpected argument '--harness'"));
    }

    #[test]
    fn detach_refuses_what_needs_a_terminal_or_a_wait() {
        let Command::Run { detached, .. } = parse_str("run go --detach --json --yes").unwrap()
        else {
            panic!("not run")
        };
        assert_eq!(
            detached,
            Detached {
                detach: true,
                json: true
            }
        );
        for line in [
            "run go --detach --ask",
            "fork b go --detach --ask",
            "send b go --detach --ask",
            "send b go --detach --steer",
            "send b go --detach --wait",
        ] {
            assert_eq!(kind(line), ErrorKind::ArgumentConflict, "{line}");
        }
        // --json on run and fork needs no --detach: the foreground run prints
        // the branch as `show --json` does (`run_and_fork_take_json_without_detach`).
        assert!(parse_str("run go --json").is_ok());
        assert!(parse_str("fork b go --json").is_ok());
    }

    #[test]
    fn checkpoint_try_and_compare_commands() {
        assert_eq!(
            parse_str("rewind b --to 2 -y").unwrap(),
            Command::Rewind {
                branch: "b".into(),
                to: 2,
                yes: true,
                json: false
            }
        );
        assert!(err("rewind b").contains("--to <N>"));
        let Command::Fork { at, .. } = parse_str("fork b next --at 0").unwrap() else {
            panic!("not fork")
        };
        assert_eq!(at, Some(0));
        assert_eq!(
            kind("fork b next --at 1 --fresh-session"),
            ErrorKind::ArgumentConflict
        );
        assert!(matches!(
            parse_str("try --off --force").unwrap(),
            Command::Try {
                branch: None,
                off: true,
                force: true,
                ..
            }
        ));
        assert!(matches!(
            parse_str("try b").unwrap(),
            Command::Try {
                branch: Some(_),
                off: false,
                status: false,
                ..
            }
        ));
        assert_eq!(kind("try"), ErrorKind::MissingRequiredArgument);
        assert_eq!(kind("try b --off"), ErrorKind::ArgumentConflict);
        assert_eq!(kind("try b --force"), ErrorKind::ArgumentConflict);
        assert_eq!(kind("try --force"), ErrorKind::MissingRequiredArgument);
        let Command::Compare {
            branches,
            fan,
            diff,
            pick,
            discard_others,
            ..
        } = parse_str("compare --fan speed --pick speed-codex --discard-others").unwrap()
        else {
            panic!("not compare")
        };
        assert!(branches.is_empty() && discard_others);
        assert_eq!(
            (fan.as_deref(), pick.as_deref(), diff),
            (Some("speed"), Some("speed-codex"), None)
        );
        assert!(matches!(
            parse_str("compare --diff a b").unwrap(),
            Command::Compare { diff: Some(pair), .. } if pair == ["a", "b"]
        ));
        assert_eq!(kind("compare"), ErrorKind::MissingRequiredArgument);
        assert_eq!(kind("compare a --fan x"), ErrorKind::ArgumentConflict);
        assert_eq!(
            kind("compare a --discard-others"),
            ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn inspection_commands() {
        assert_eq!(parse_str("ls").unwrap(), Command::Ls { json: false });
        assert_eq!(parse_str("ls --json").unwrap(), Command::Ls { json: true });
        assert_eq!(
            parse_str("show b --json").unwrap(),
            Command::Show {
                branch: "b".into(),
                json: true,
                refresh: false
            }
        );
        assert_eq!(
            parse_str("diff b").unwrap(),
            Command::Diff { branch: "b".into() }
        );
        assert_eq!(
            parse_str("log b").unwrap(),
            Command::Log {
                branch: "b".into(),
                json: false,
                follow: false
            }
        );
        assert_eq!(
            parse_str("log -f b").unwrap(),
            Command::Log {
                branch: "b".into(),
                json: false,
                follow: true
            }
        );
        let harnesses = |json, all| Command::Harnesses {
            json,
            all,
            profiles: false,
            refresh: false,
            on: None,
            action: None,
        };
        assert_eq!(
            parse_str("harnesses --json").unwrap(),
            harnesses(true, false)
        );
        assert_eq!(
            parse_str("harnesses --all").unwrap(),
            harnesses(false, true)
        );
        assert_eq!(
            parse_str("harnesses install codex --on ssh://h --version 1.2 --yes --json").unwrap(),
            Command::Harnesses {
                json: true,
                all: false,
                profiles: false,
                refresh: false,
                on: Some("ssh://h".into()),
                action: Some(HarnessesAction::Install(HarnessChange {
                    id: "codex".into(),
                    version: Some("1.2".into()),
                    yes: true,
                })),
            }
        );
        assert_eq!(
            parse_str("harnesses login codex --api-key").unwrap(),
            Command::Harnesses {
                json: false,
                all: false,
                profiles: false,
                refresh: false,
                on: None,
                action: Some(HarnessesAction::Login {
                    id: "codex".into(),
                    api_key: true,
                    yes: false,
                }),
            }
        );
        assert!(parse_str("harnesses --all --on ssh://h").is_err());
        assert_eq!(
            parse_str("connectors --json catalog").unwrap(),
            Command::Connectors {
                json: true,
                action: ConnectorsAction::Catalog
            }
        );
        assert_eq!(
            parse_str("connectors catalog --json").unwrap(),
            Command::Connectors {
                json: true,
                action: ConnectorsAction::Catalog
            }
        );
        assert_eq!(
            parse_str("services --kind connector_gateway --json").unwrap(),
            Command::Services {
                json: true,
                kind: Some("connector_gateway".into()),
                all: false,
                action: None,
            }
        );
        assert_eq!(
            parse_str("sync fix-login --json").unwrap(),
            Command::Sync(SyncArgs {
                json: true,
                task: Some("fix-login".into()),
                action: None,
            })
        );
        assert_eq!(
            parse_str("sync gc --dry-run").unwrap(),
            Command::Sync(SyncArgs {
                json: false,
                task: None,
                action: Some(SyncAction::Gc { dry_run: true }),
            })
        );
        assert_eq!(
            parse_str("sync hold t --reason audit --release").unwrap(),
            Command::Sync(SyncArgs {
                json: false,
                task: None,
                action: Some(SyncAction::Hold {
                    task: "t".into(),
                    reason: Some("audit".into()),
                    release: true,
                }),
            })
        );
        assert_eq!(
            parse_str("services gc --json").unwrap(),
            Command::Services {
                json: true,
                kind: None,
                all: false,
                action: Some(ServicesAction::Gc),
            }
        );
        assert!(matches!(
            parse_str("catalog refresh --only harnesses --npm-registry http://127.0.0.1:1")
                .unwrap(),
            Command::Catalog {
                action: CatalogAction::Refresh { ref only, ref npm_registry, max_pages: 20, .. },
                ..
            } if only.as_deref() == Some("harnesses") && npm_registry == "http://127.0.0.1:1"
        ));
        assert!(err("catalog refresh --only models").contains("invalid value"));
        assert!(err("diff b --json").contains("unexpected argument '--json'"));
    }

    #[test]
    fn merge_and_rm() {
        assert_eq!(
            parse_str("merge b").unwrap(),
            Command::Merge {
                branch: "b".into(),
                into: None,
                rm: false,
                promote_effects: false,
                json: false
            }
        );
        assert_eq!(
            parse_str("merge b --into release").unwrap(),
            Command::Merge {
                branch: "b".into(),
                into: Some("release".into()),
                rm: false,
                promote_effects: false,
                json: false
            }
        );
        assert_eq!(
            parse_str("rm b --keep-credentials").unwrap(),
            Command::Rm {
                branch: "b".into(),
                keep_credentials: true,
                json: false
            }
        );
        assert!(err("merge b --into").contains("a value is required for '--into <TARGET>'"));
        assert_eq!(
            parse_str("merge b --rm").unwrap(),
            Command::Merge {
                branch: "b".into(),
                into: None,
                rm: true,
                promote_effects: false,
                json: false
            }
        );
    }

    #[test]
    fn workspace_actions_parse_with_json_anywhere() {
        let workspace = |line: &str| match parse_str(line).unwrap() {
            Command::Workspace { json, action } => (json, action),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            workspace("workspace show"),
            (false, WorkspaceAction::Show { branch: None })
        );
        assert_eq!(
            workspace("workspace --json show b"),
            (
                true,
                WorkspaceAction::Show {
                    branch: Some("b".into())
                }
            )
        );
        assert_eq!(
            workspace("workspace trust"),
            (false, WorkspaceAction::Trust)
        );
        assert_eq!(
            workspace("workspace untrust --json"),
            (true, WorkspaceAction::Untrust)
        );
        assert_eq!(
            workspace("workspace run b dev --detach"),
            (
                false,
                WorkspaceAction::Run {
                    args: vec!["b".into(), "dev".into()],
                    detach: true
                }
            )
        );
        assert_eq!(
            workspace("workspace run"),
            (
                false,
                WorkspaceAction::Run {
                    args: vec![],
                    detach: false
                }
            )
        );
        assert_eq!(
            parse_str("workspace run a b c --detach").unwrap(),
            Command::Workspace {
                json: false,
                action: WorkspaceAction::Run {
                    args: vec!["a".into(), "b".into(), "c".into()],
                    detach: true,
                },
            }
        );
        assert_eq!(
            parse_str("workspace kill b --port 5173 --yes --json").unwrap(),
            Command::Workspace {
                json: true,
                action: WorkspaceAction::Kill {
                    branch: Some("b".into()),
                    port: Some(5173),
                    yes: true,
                },
            }
        );
        assert!(err("workspace browse b --port 0x1").contains("invalid value"));
        assert!(err("workspace").contains("Usage"));
    }

    #[test]
    fn env_actions_parse() {
        let env = |line: &str| match parse_str(line).unwrap() {
            Command::Env { json, action } => (json, action),
            other => panic!("{other:?}"),
        };
        assert_eq!(env("env list --json"), (true, EnvAction::List));
        assert_eq!(env("env show"), (false, EnvAction::Show { key: None }));
        assert_eq!(env("env rebuild"), (false, EnvAction::Rebuild));
        assert_eq!(
            env("env prune abc def --keep 1 --older-than 7"),
            (
                false,
                EnvAction::Prune {
                    keys: vec!["abc".into(), "def".into()],
                    keep: Some(1),
                    older_than: Some(7),
                }
            )
        );
        assert_eq!(
            env("env pool fill --json"),
            (true, EnvAction::Pool(PoolAction::Fill))
        );
        assert_eq!(
            env("env pool status"),
            (false, EnvAction::Pool(PoolAction::Status))
        );
        assert_eq!(
            env("env pool drain"),
            (false, EnvAction::Pool(PoolAction::Drain))
        );
        assert!(err("env pool").contains("Usage"));
        assert!(err("env").contains("Usage"));
    }

    #[test]
    fn help_and_version() {
        let cli = |line: &str| parse_from(format!("by {line}").split_whitespace());
        assert_eq!(parse_from(["by"]).unwrap().command, None);
        for (line, kind) in [
            ("help", ErrorKind::DisplayHelp),
            ("--help", ErrorKind::DisplayHelp),
            ("-h", ErrorKind::DisplayHelp),
            ("help merge", ErrorKind::DisplayHelp),
            ("help graph apply", ErrorKind::DisplayHelp),
            ("fan -h", ErrorKind::DisplayHelp),
            ("--version", ErrorKind::DisplayVersion),
            ("-V", ErrorKind::DisplayVersion),
        ] {
            let error = cli(line).unwrap_err();
            assert_eq!(error.kind(), kind, "{line}");
            assert_eq!(error.exit_code(), 0, "{line}");
        }
        // --help wins over missing arguments, and over anything after it.
        assert_eq!(
            cli("run --help --bogus").unwrap_err().kind(),
            ErrorKind::DisplayHelp
        );
        assert!(cli("help merge")
            .unwrap_err()
            .to_string()
            .contains("Usage: by merge [OPTIONS] <BRANCH>"));
        assert!(cli("-V").unwrap_err().to_string().starts_with("by "));
        assert!(err("help nope").contains("unrecognized subcommand 'nope'"));
    }

    #[test]
    fn typos_get_suggestions_and_usage_errors_exit_2() {
        let error = parse_str("mrege b").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidSubcommand);
        assert_eq!(error.exit_code(), 2);
        // `review`, `recipe` and `remote` are close to `mrege` too, so clap lists them.
        assert!(
            error
                .to_string()
                .contains("similar subcommands exist: 'review', 'recipe', 'remote', 'merge'"),
            "{error}"
        );
        assert!(err("run go --budget 2").contains("a similar argument exists: '--budget-usd'"));
        assert!(err("graph shwo").contains("a similar subcommand exists: 'show'"));
    }

    #[test]
    fn flag_errors_name_the_command() {
        let error = parse_str("run go --bogus").unwrap_err().to_string();
        assert!(error.contains("unexpected argument '--bogus'"), "{error}");
        assert!(error.contains("Usage: by run"), "{error}");
        assert!(err("nope").contains("unrecognized subcommand 'nope'"));
        assert!(err("run go -x").contains("unexpected argument '-x'"));
        assert!(err("run go --yes --ask").contains("'--yes' cannot be used with '--ask'"));
        assert!(err("run go --yes=1").contains("unexpected value '1' for '--yes'"));
        assert!(err("run go --name a --name b").contains("cannot be used multiple times"));
        assert!(err("run go --harness").contains("a value is required for '--harness <ID>'"));
        assert!(err("run go --budget-usd -1").contains("positive number"));
        assert!(err("run go --budget-usd NaN").contains("positive number"));
        assert!(err("run go --max-turns 0").contains("positive whole number"));
        assert!(err("run go --max-turns 1.5").contains("positive whole number"));
        assert!(err("run go --max-minutes 0").contains("positive number of minutes"));
        assert!(err("run go --max-minutes 1e300").contains("too large"));
        assert!(err("run go --command ''").contains("needs an executable"));
        assert!(err("send b go --isolated").contains("unexpected argument '--isolated'"));
        assert!(err("run go --check ''").contains("needs a command"));
        assert!(err("run go --check '\"cargo'").contains("unterminated quote"));
        let spaces = err("run fix the test");
        assert!(spaces.contains("unexpected argument 'the'"), "{spaces}");
        assert!(
            spaces.contains("tip: quote a prompt that contains spaces"),
            "{spaces}"
        );
        let ls = err("ls extra");
        assert!(ls.contains("unexpected argument 'extra'") && !ls.contains("quote"));
        // A checked flag's error names its own command's usage.
        assert!(err("fork b go --image x").contains("Usage: by fork"));
    }

    #[test]
    fn globals_go_before_or_after_the_command() {
        let before = parse_from(
            split_words("by --remote http://h:1 --token-file=t --repo app ls --json").unwrap(),
        )
        .unwrap();
        let expected = Globals {
            remote: Some("http://h:1".into()),
            token_file: Some("t".into()),
            repo: Some("app".into()),
            ..Globals::default()
        };
        assert_eq!(before.globals, expected);
        let quiet = parse_from(split_words("by watch --no-notify").unwrap()).unwrap();
        assert!(quiet.globals.no_notify);
        assert_eq!(before.command, Some(Command::Ls { json: true }));
        let after = parse_from(
            split_words("by ls --json --remote http://h:1 --repo app --token-file t").unwrap(),
        )
        .unwrap();
        assert_eq!(after, before);
        let nested = parse_from(split_words("by artifact list --ca-file ca.pem").unwrap()).unwrap();
        assert_eq!(nested.globals.ca_file.as_deref(), Some("ca.pem"));
        for (line, error) in [
            ("by --remote", "a value is required for '--remote <URL>'"),
            ("by --remote= ls", "needs a value"),
            ("by --repo a --repo b ls", "cannot be used multiple times"),
            ("by ls --repo a --repo b", "cannot be used multiple times"),
        ] {
            let got = parse_from(split_words(line).unwrap())
                .unwrap_err()
                .to_string();
            assert!(got.contains(error), "{line}: {got}");
        }
    }

    #[test]
    fn watch_and_serve() {
        assert_eq!(
            parse_str("watch").unwrap(),
            Command::Watch {
                interval: Duration::from_secs(1),
                once: false
            }
        );
        assert_eq!(
            parse_str("watch --interval 0.5 --once").unwrap(),
            Command::Watch {
                interval: Duration::from_millis(500),
                once: true
            }
        );
        assert_eq!(
            parse_str("watch --interval 250ms").unwrap(),
            Command::Watch {
                interval: Duration::from_millis(250),
                once: false
            }
        );
        assert!(err("watch --interval 0").contains("from 0.05 to 3600"));
        assert!(err("watch --interval 2h").contains("from 0.05 to 3600"));
        let call = |line: &str| {
            let args: Vec<OsString> = split_words(line)
                .unwrap()
                .into_iter()
                .map(Into::into)
                .collect();
            server_call(&args)
        };
        assert_eq!(
            call("serve --listen 127.0.0.1:0 --repo a=/r --help"),
            Some(ServerCall {
                prefix: 1,
                program: "by serve",
                args: vec![
                    "--listen".into(),
                    "127.0.0.1:0".into(),
                    "--repo".into(),
                    "a=/r".into(),
                    "--help".into()
                ],
                help: false,
            })
        );
        assert_eq!(
            call("--remote=x --repo app worker --database postgres://h/d"),
            Some(ServerCall {
                prefix: 4,
                program: "by worker",
                args: vec![
                    "--worker".into(),
                    "--database".into(),
                    "postgres://h/d".into()
                ],
                help: false,
            })
        );
        assert_eq!(
            call("--repo a help worker").map(|c| (c.prefix, c.help, c.program)),
            Some((2, true, "by worker"))
        );
        assert_eq!(call("ls serve"), None);
        assert_eq!(call("run serve"), None);
        assert_eq!(call("help merge"), None);
        assert_eq!(call(""), None);
    }

    #[test]
    fn nested_commands_map_to_their_actions() {
        let graph = |line: &str| match parse_str(line).unwrap() {
            Command::Graph { json, action } => action.into_args(json),
            other => panic!("{other:?}"),
        };
        let show = graph("graph show root --json");
        assert_eq!(
            (show.action.as_str(), show.arg.as_deref(), show.json),
            ("show", Some("root"), true)
        );
        // The Python module puts --json before the action.
        assert!(graph("graph --json show").json);
        let apply = graph("graph apply --edits [] --expected-revision 3 --parent p --yes");
        assert_eq!(apply.action, "apply");
        assert_eq!(apply.edits.as_deref(), Some("[]"));
        assert_eq!(apply.expected_revision, Some(3));
        assert_eq!(apply.parent.as_deref(), Some("p"));
        assert_eq!(apply.task.permissions, Permissions::Yes);
        assert_eq!(graph("graph apply - --parent p").arg.as_deref(), Some("-"));
        assert_eq!(
            graph("graph resume --ask").task.permissions,
            Permissions::Ask
        );
        assert!(err("graph apply --parent p").contains("<FILE|--edits <JSON>>"));
        assert!(err("graph apply f --edits []").contains("cannot be used with"));
        assert!(err("graph apply --edits []").contains("--expected-revision"));
        assert!(err("graph").contains("Usage: by graph"));

        let rig = |line: &str| match parse_str(line).unwrap() {
            Command::Rig { json, action } => action.into_args(json),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            rig("rig check team.toml --json"),
            RigArgs {
                file: "team.toml".into(),
                json: true,
                ..RigArgs::default()
            }
        );
        let run = rig("rig run team.toml 'build it' -n t --command '/bin/agent -x'");
        assert_eq!(run.prompt.as_deref(), Some("build it"));
        assert_eq!(run.name.as_deref(), Some("t"));
        assert_eq!(run.command, Some(vec!["/bin/agent".into(), "-x".into()]));
        assert!(err("rig run team.toml").contains("<PROMPT>"));
        assert!(err("rig run team.toml ' '").contains("needs a prompt"));
        assert!(err("rig check team.toml extra").contains("unexpected argument 'extra'"));
        assert!(err("rig check team.toml --name x").contains("unexpected argument '--name'"));
        assert!(err("rig start team.toml").contains("unrecognized subcommand 'start'"));

        let artifact = |line: &str| match parse_str(line).unwrap() {
            Command::Artifact {
                branch,
                json,
                action,
            } => action.into_args(branch, json),
            other => panic!("{other:?}"),
        };
        let publish =
            artifact("artifact publish out.txt -n report --label k=v --label a=b=c --branch kid");
        assert_eq!(publish.action, "publish");
        assert_eq!(publish.arg.as_deref(), Some("out.txt"));
        assert_eq!(publish.name.as_deref(), Some("report"));
        assert_eq!(
            publish.labels,
            [("k".into(), "v".into()), ("a".into(), "b=c".into())]
        );
        assert_eq!(publish.branch.as_deref(), Some("kid"));
        assert!(artifact("artifact --json list").json);
        let export = artifact("artifact export 1 2 -o b.tar");
        assert_eq!(export.ids, ["1", "2"]);
        assert_eq!(export.out.as_deref(), Some("b.tar"));
        assert!(err("artifact get 1").contains("--out <PATH>"));
        assert!(err("artifact share 1").contains("--to <BRANCH>"));
        assert!(err("artifact export -o b.tar").contains("<IDS>..."));
        assert!(err("artifact publish").contains("<FILE>"));
        assert!(err("artifact publish f --label novalue").contains("KEY=VALUE"));
        assert!(err("artifact list --out x").contains("unexpected argument '--out'"));

        let scratch = |line: &str| match parse_str(line).unwrap() {
            Command::Scratch {
                branch,
                json,
                action,
            } => action.into_args(branch, json),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            scratch("scratch share cache --to buddy --branch me --json"),
            ScratchArgs {
                action: "share".into(),
                name: Some("cache".into()),
                to: Some("buddy".into()),
                branch: Some("me".into()),
                json: true,
            }
        );
        assert_eq!(scratch("scratch list").name, None);
        assert!(err("scratch lock").contains("<NAME>"));
        assert!(err("scratch share cache").contains("--to <BRANCH>"));
    }

    #[test]
    fn double_dash_allows_prompts_that_look_like_flags() {
        let Command::Run { prompt, task, .. } = parse_str("run --yes -- --explain-flags").unwrap()
        else {
            panic!("not run")
        };
        assert_eq!(prompt, "--explain-flags");
        assert_eq!(task.permissions, Permissions::Yes);
    }

    #[test]
    fn split_words_follows_shell_quoting() {
        let split = |s: &str| split_words(s).unwrap();
        assert_eq!(split("  cargo   test  "), ["cargo", "test"]);
        assert_eq!(split("sh -c 'echo $HOME'"), ["sh", "-c", "echo $HOME"]);
        assert_eq!(
            split(r#"say "a \"quoted\" \$word" and\ more"#),
            ["say", r#"a "quoted" $word"#, "and more"]
        );
        assert_eq!(split(r#""keep \n literal""#), [r"keep \n literal"]);
        assert_eq!(split("''"), [""]);
        assert_eq!(split("a'b'\"c\""), ["abc"]);
        assert_eq!(split("make test # a comment"), ["make", "test"]);
        assert_eq!(split("grep a#b"), ["grep", "a#b"]);
        assert!(split("").is_empty());
        assert!(split_words("a\\").is_err());
        assert!(split_words("'a").is_err());
    }

    #[test]
    fn shell_quote_round_trips() {
        assert_eq!(shell_quote("fix-flaky_1.2"), "fix-flaky_1.2");
        assert_eq!(shell_quote("it's here"), "\"it's here\"");
        assert_eq!(shell_quote(""), "''");
        for word in [
            "plain",
            "two words",
            "it's",
            "$x",
            "",
            "a\"b'c",
            "tab\there",
        ] {
            assert_eq!(split_words(&shell_quote(word)).unwrap(), [word], "{word}");
        }
    }

    fn help(name: &str) -> String {
        let mut cmd = command();
        cmd.build();
        cmd.find_subcommand_mut(name)
            .unwrap()
            .render_help()
            .to_string()
    }

    /// The commands a harness runs for itself, and plumbing: not in `by
    /// --help`, but in `by help all`, and the first group in `by help
    /// agents`.
    const HIDDEN: &[&str] = &[
        "ask",
        "answer",
        "inbox",
        "escalate",
        "report",
        "report-status",
        "brief",
        "delegate",
        "spawn",
        "children",
        "graph",
        "integrate",
        "inspect",
        "events",
        "discard",
        "mcp",
        "hook",
        "statusline",
        "completions",
        "man",
        "worker",
        "serve",
    ];

    /// Every command and subcommand under `cmd`, with the path that names it.
    fn walk<'a>(cmd: &'a clap::Command, path: &str, out: &mut Vec<(String, &'a clap::Command)>) {
        for sub in cmd.get_subcommands().filter(|sub| sub.get_name() != "help") {
            let name = format!("{path} {}", sub.get_name());
            out.push((name.clone(), sub));
            walk(sub, &name, out);
        }
    }

    fn built() -> clap::Command {
        let mut cmd = command();
        cmd.build();
        cmd
    }

    #[test]
    fn help_lists_the_first_run_commands_and_help_all_every_command() {
        let general = command().render_help().to_string();
        for (title, names) in FRONT {
            for name in *names {
                if built().find_subcommand(name).is_some() {
                    assert!(
                        general.contains(&format!("\n  {name} ")),
                        "{name} missing from:\n{general}"
                    );
                }
            }
            assert!(general.contains(&format!("{title}:")), "{title}");
        }
        // What an agent or the plumbing runs is not on the first screen.
        for name in HIDDEN {
            assert!(
                !general.contains(&format!("\n  {name} ")),
                "{name} is listed in:\n{general}"
            );
        }
        // check, wait and cancel are for people too.
        let visible = |name: &str| !built().find_subcommand(name).unwrap().is_hide_set();
        assert!(["check", "wait", "cancel"].into_iter().all(visible));
        assert!(general.contains("Also: "), "{general}");
        assert!(general.contains("by help all") && general.contains("by help agents"));
        assert!(
            general.lines().count() < 70,
            "{} lines:\n{general}",
            general.lines().count()
        );
        assert!(general.contains("[env: BRANCHYARD_REMOTE]"), "{general}");
        assert!(!general.contains("Other commands"));

        // `by help all` lists every command, hidden or not, in its group.
        let all = listing_text(Listing::All, false);
        let cmd = built();
        for sub in cmd.get_subcommands() {
            let name = sub.get_name();
            assert!(
                all.contains(&format!("\n  {name} ")),
                "{name} missing from:\n{all}"
            );
            if name != "help" {
                let order = sub.get_display_order();
                assert!(
                    GROUPS.iter().any(|(group, _)| *group == order / 100),
                    "{name} is in no group ({order})"
                );
            }
        }
        for (_, title) in GROUPS {
            assert!(all.contains(&format!("{title}:")), "{title}");
        }
        assert!(!all.contains("Other commands"));

        // `by help agents` is what a harness runs.
        let agents = listing_text(Listing::Agents, false);
        for name in ["spawn", "ask", "inbox", "wait", "check", "mcp", "hook"] {
            assert!(
                agents.contains(&format!("\n  {name} ")),
                "{name}:\n{agents}"
            );
        }
        assert!(!agents.contains("\n  run "), "{agents}");

        let run = help("run");
        for heading in [
            "Checks and limits:",
            "Permissions:",
            "Launch:",
            "Microsandbox provider",
            "Substrate provider",
            "Delegation:",
            "Provisioning:",
            "Global options:",
            "Examples:",
        ] {
            assert!(run.contains(heading), "{heading} missing from:\n{run}");
        }
        assert!(help("fork").contains("Usage: by fork [OPTIONS] <BRANCH> <PROMPT>"));
        assert!(help("ls").contains("Usage: by ls [OPTIONS]"));
        assert!(help("rm").contains("Usage: by rm [OPTIONS] <BRANCH>"));
        // A hidden command still has its help.
        assert!(help("spawn").contains("Usage: by spawn"));
    }

    #[test]
    fn help_topics_are_recognised_after_the_global_options() {
        let argv =
            |line: &str| -> Vec<OsString> { line.split_whitespace().map(OsString::from).collect() };
        assert_eq!(help_listing(&argv("help all")), Some(Listing::All));
        assert_eq!(help_listing(&argv("help agents")), Some(Listing::Agents));
        assert_eq!(
            help_listing(&argv("--remote http://h:1 --no-notify help all")),
            Some(Listing::All)
        );
        // A command is a command: `by help run` is clap's.
        assert_eq!(help_listing(&argv("help run")), None);
        assert_eq!(help_listing(&argv("help")), None);
        assert_eq!(help_listing(&argv("run all")), None);
        assert_eq!(help_listing(&argv("help all extra")), None);
    }

    #[test]
    fn the_agent_verbs_and_the_plumbing_are_hidden() {
        let cmd = built();
        for name in HIDDEN {
            let sub = cmd
                .find_subcommand(name)
                .unwrap_or_else(|| panic!("no {name}"));
            assert!(sub.is_hide_set(), "{name} is listed");
        }
        // Everything else is listed, so `by help all` and the shell see it.
        let shown: Vec<&str> = cmd
            .get_subcommands()
            .filter(|c| !c.is_hide_set())
            .map(|c| c.get_name())
            .collect();
        for name in [
            "run", "fan", "send", "check", "wait", "cancel", "merge", "doctor",
        ] {
            assert!(shown.contains(&name), "{name} is hidden");
        }
        // The man page lists them all.
        let mut page = Vec::new();
        clap_mangen::Man::new(command_with_hidden())
            .render(&mut page)
            .unwrap();
        let page = String::from_utf8(page).unwrap();
        assert!(
            page.contains("by\\-spawn") && page.contains("by\\-hook"),
            "{page}"
        );
    }

    #[test]
    fn a_summary_is_one_short_line() {
        let cmd = built();
        let mut all = Vec::new();
        walk(&cmd, "by", &mut all);
        let mut long = Vec::new();
        for (path, sub) in &all {
            let about = sub.get_about().map(|a| a.to_string()).unwrap_or_default();
            let top = path.matches(' ').count() == 1;
            let limit = if top { 80 } else { 100 };
            if about.chars().count() >= limit || about.contains('\n') {
                long.push(format!("{path} ({}): {about}", about.chars().count()));
            }
        }
        assert!(
            long.is_empty(),
            "summaries over the limit:\n{}",
            long.join("\n")
        );
    }

    #[test]
    fn every_argument_says_what_it_is() {
        let cmd = built();
        let mut all = Vec::new();
        walk(&cmd, "by", &mut all);
        let mut blank = Vec::new();
        for (path, sub) in &all {
            for arg in sub.get_arguments() {
                let id = arg.get_id().as_str();
                if arg.is_hide_set() || matches!(id, "help" | "version") {
                    continue;
                }
                if arg
                    .get_help()
                    .is_none_or(|h| h.to_string().trim().is_empty())
                {
                    blank.push(format!("{path} {id}"));
                }
            }
        }
        assert!(
            blank.is_empty(),
            "arguments with no help:\n{}",
            blank.join("\n")
        );
    }

    #[test]
    fn placeholders_say_what_they_take() {
        let cmd = built();
        let mut all = Vec::new();
        walk(&cmd, "by", &mut all);
        let mut vague = Vec::new();
        for (path, sub) in &all {
            for arg in sub.get_arguments() {
                for name in arg.get_value_names().unwrap_or_default() {
                    if ["X", "S", "SECS"].contains(&name.as_str()) {
                        vague.push(format!("{path} {} <{name}>", arg.get_id()));
                    }
                }
            }
        }
        assert!(
            vague.is_empty(),
            "vague placeholders:\n{}",
            vague.join("\n")
        );
        let run = help("run");
        assert!(run.contains("--budget-usd <DOLLARS>"), "{run}");
        assert!(help("wait").contains("--timeout <SECONDS>"));
    }

    #[test]
    fn yes_says_what_it_does_in_each_command() {
        let text = |command: &str| help(command);
        assert!(
            text("run").contains("Grant every tool permission request without asking")
                && text("run").contains("the harness may write files and run commands"),
            "{}",
            text("run")
        );
        assert!(text("rewind").contains("Confirm the reset without a prompt"));
        assert!(text("undo").contains("Undo every reversible effect without a confirmation"));
        assert!(text("compare").contains("Confirm removing the other attempts without a prompt"));
        assert!(text("up").contains("Make the installs the [harnesses] policy would ask about"));
        // No command leaves it at the vague wording.
        let cmd = built();
        let mut all = Vec::new();
        walk(&cmd, "by", &mut all);
        for (path, sub) in &all {
            for arg in sub.get_arguments().filter(|a| a.get_id() == "yes") {
                let help = arg.get_help().map(|h| h.to_string()).unwrap_or_default();
                for vague in [
                    "Do not ask first",
                    "Do not ask for confirmation",
                    "Allow every tool",
                ] {
                    assert!(!help.contains(vague), "{path} --yes: {help}");
                }
            }
        }
    }

    #[test]
    fn run_and_fork_take_json_without_detach() {
        let Command::Run { detached, .. } = parse_str("run go --json").unwrap() else {
            panic!("not run")
        };
        assert!(detached.json && !detached.detach);
        let Command::Fork { detached, .. } = parse_str("fork b go --json").unwrap() else {
            panic!("not fork")
        };
        assert!(detached.json && !detached.detach);
        let Command::Run { detached, .. } = parse_str("run go --detach --json").unwrap() else {
            panic!("not run")
        };
        assert!(detached.json && detached.detach);
        // The commands that gained --json this round.
        for line in [
            "merge b --json",
            "rm b --json",
            "fan go --harness codex --json",
            "review b --json",
        ] {
            assert!(parse_str(line).is_ok(), "{line}: {}", err_or_ok(line));
        }
        assert!(parse_str("review b --detach --json").is_err());
    }

    fn err_or_ok(line: &str) -> String {
        parse_str(line)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default()
    }

    #[test]
    fn the_run_help_defaults_say_the_command_waits() {
        for command in ["run", "send", "fork", "fan"] {
            let cmd = built();
            let about = cmd
                .find_subcommand(command)
                .unwrap()
                .get_about()
                .unwrap()
                .to_string();
            assert!(about.contains("waits"), "{command}: {about}");
        }
        let detach = help("run");
        assert!(
            detach.contains(
                "Return as soon as the turn has started, leaving it running in the background"
            ),
            "{detach}"
        );
        assert!(help("send").contains("Return as soon as the turn has started"));
    }

    #[test]
    fn completions_and_the_man_page_generate() {
        for shell in [
            clap_complete::Shell::Bash,
            clap_complete::Shell::Zsh,
            clap_complete::Shell::Fish,
            clap_complete::Shell::PowerShell,
            clap_complete::Shell::Elvish,
        ] {
            let mut out = Vec::new();
            clap_complete::generate(shell, &mut command(), "by", &mut out);
            let script = String::from_utf8(out).unwrap();
            for word in [
                "reincarnate",
                "budget-usd",
                "remote",
                "doctor",
                "init",
                "dry-run",
                "config",
                "validate",
            ] {
                assert!(script.contains(word), "{shell}: {word}");
            }
        }
        let mut page = Vec::new();
        clap_mangen::Man::new(command()).render(&mut page).unwrap();
        let page = String::from_utf8(page).unwrap();
        assert!(page.contains(".TH by") && page.contains("remote"), "{page}");
        assert!(
            page.contains("by\\-init") && page.contains("by\\-config"),
            "{page}"
        );
    }

    fn init(line: &str) -> InitArgs {
        match parse_str(line).unwrap() {
            Command::Init { init } => init.into_inner(),
            other => panic!("{line}: parsed as {other:?}"),
        }
    }

    #[test]
    fn init_takes_a_topic_and_one_protocol_step() {
        use branchyard_setup::Topic;
        assert_eq!(
            init("init"),
            InitArgs {
                topic: None,
                step: None,
                json: false,
                answers: None,
                defaults: false,
                force: false,
                harnesses: Vec::new(),
                all_detected: false,
                status: false,
            }
        );
        assert!(init("init plugin --status").status);
        assert!(init("init plugin --status --json").json);
        let plugin = init("init plugin codex hermes --dry-run");
        assert_eq!(plugin.harnesses, ["codex", "hermes"]);
        assert_eq!(plugin.step, Some(InitStep::DryRun));
        assert!(init("init plugin --all-detected").all_detected);
        assert!(init("init plugin --json").json);
        assert_eq!(init("init --json").step, None);
        assert_eq!(init("init server --defaults").topic, Some(Topic::Server));
        let next = init("init project --json --next --answers=a.json");
        assert_eq!(next.topic, Some(Topic::Project));
        assert_eq!(next.step, Some(InitStep::Next));
        assert!(next.json);
        assert_eq!(next.answers.as_deref(), Some("a.json"));
        let apply = init("init rig --answers - --apply --force --defaults");
        assert_eq!(apply.step, Some(InitStep::Apply));
        assert!(apply.force && apply.defaults);
        assert_eq!(apply.answers.as_deref(), Some("-"));
        assert_eq!(init("init deploy --dry-run").step, Some(InitStep::DryRun));
        // Globals still go anywhere.
        assert_eq!(
            init("init plugin --remote http://h:1 --next").topic,
            Some(Topic::Plugin)
        );
        // Every topic the engine knows parses, in its order.
        for topic in Topic::ALL {
            assert_eq!(init(&format!("init {}", topic.id())).topic, Some(topic));
        }
    }

    fn setup(line: &str) -> SetupFlags {
        match parse_str(line).unwrap() {
            Command::Setup { setup } => setup,
            other => panic!("{line}: parsed as {other:?}"),
        }
    }

    #[test]
    fn setup_takes_its_flags_and_is_the_first_command_listed() {
        assert_eq!(setup("setup"), SetupFlags::default());
        let all = setup("setup --yes --dry-run --json --no-config");
        assert!(all.yes && all.dry_run && all.json && all.no_config && !all.no_hooks);
        // Harnesses, comma-separated or as several values.
        assert_eq!(
            setup("setup --harness claude-code,codex").harnesses,
            ["claude-code", "codex"]
        );
        assert_eq!(
            setup("setup --yes --harness claude-code codex --no-config").harnesses,
            ["claude-code", "codex"]
        );
        assert!(setup("setup --no-hooks").no_hooks);
        let guided = setup("setup --guided --up --directory /example/project");
        assert!(guided.guided && guided.up);
        assert_eq!(guided.directory, Some(PathBuf::from("/example/project")));
        // Naming harnesses and installing none contradict.
        let error = parse_str("setup --harness codex --no-hooks").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ArgumentConflict, "{error}");
        assert_eq!(error.exit_code(), 2);
        assert_eq!(
            parse_str("setup --bogus").unwrap_err().kind(),
            ErrorKind::UnknownArgument
        );
        // "Start" leads the first screen, with setup first in it; `by help
        // all` files it under "Shell and setup".
        let general = command().render_help().to_string();
        let start = general.find("Start:").unwrap();
        assert!(start < general.find("Work:").unwrap(), "{general}");
        assert!(
            general[start..].starts_with("Start:\n  setup "),
            "{general}"
        );
        let all = listing_text(Listing::All, false);
        let shell = all.find("Shell and setup:").unwrap();
        assert!(all[shell..].contains("\n  setup "), "{all}");
        let help = help("setup");
        for word in [
            "--yes",
            "--dry-run",
            "--json",
            "--harness <ID>",
            "--no-hooks",
            "--no-config",
            "--guided",
            "--up",
            "--directory <PATH>",
        ] {
            assert!(help.contains(word), "{word} missing from:\n{help}");
        }
        assert!(help.contains("docs/setup.md#one-command"), "{help}");
    }

    #[test]
    fn init_refuses_conflicting_or_incomplete_steps_as_usage_errors() {
        for (line, kind, text) in [
            (
                "init project --next --apply",
                ErrorKind::ArgumentConflict,
                "separate steps",
            ),
            (
                "init project --dry-run --apply",
                ErrorKind::ArgumentConflict,
                "separate steps",
            ),
            (
                "init project --next --dry-run",
                ErrorKind::ArgumentConflict,
                "separate steps",
            ),
            (
                "init --next",
                ErrorKind::MissingRequiredArgument,
                "give a topic",
            ),
            (
                "init project --force",
                ErrorKind::MissingRequiredArgument,
                "--apply",
            ),
            (
                "init project --force --dry-run",
                ErrorKind::ArgumentConflict,
                "'--force' cannot be used with '--dry-run'",
            ),
            (
                "init project --answers a.json",
                ErrorKind::MissingRequiredArgument,
                "--next",
            ),
            (
                "init nope --next",
                ErrorKind::ValueValidation,
                "unknown topic 'nope'",
            ),
            (
                "init project extra",
                ErrorKind::ArgumentConflict,
                "harnesses, --all-detected and --status go with by init plugin",
            ),
            (
                "init rig --all-detected",
                ErrorKind::ArgumentConflict,
                "go with by init plugin",
            ),
            (
                "init rig --status",
                ErrorKind::ArgumentConflict,
                "go with by init plugin",
            ),
            (
                "init plugin --status --apply",
                ErrorKind::ArgumentConflict,
                "cannot be used with",
            ),
            (
                "init plugin codex --all-detected",
                ErrorKind::ArgumentConflict,
                "cannot be used with",
            ),
            (
                "init project --bogus",
                ErrorKind::UnknownArgument,
                "--bogus",
            ),
            (
                "init project --json --json",
                ErrorKind::ArgumentConflict,
                "cannot be used multiple times",
            ),
            (
                "init project --answers",
                ErrorKind::InvalidValue,
                "a value is required",
            ),
            (
                "init project --json",
                ErrorKind::ArgumentConflict,
                "give --next, --dry-run or --apply",
            ),
            (
                "init --json --apply=yes",
                ErrorKind::TooManyValues,
                "--apply",
            ),
        ] {
            let error = parse_str(line).unwrap_err();
            assert_eq!(error.kind(), kind, "{line}: {error}");
            assert_eq!(error.exit_code(), 2, "{line}");
            let text_of = error.to_string();
            assert!(text_of.contains(text), "{line}: {text_of}");
            // clap shows the usage line with every error but a bad value.
            if !matches!(kind, ErrorKind::ValueValidation | ErrorKind::InvalidValue) {
                assert!(text_of.contains("Usage: by init"), "{line}: {text_of}");
            }
        }
        let help = help("init");
        for word in [
            "[TOPIC]",
            "--next",
            "--dry-run",
            "--apply",
            "--force",
            "--answers <FILE|->",
            "Examples:",
        ] {
            assert!(help.contains(word), "{word} missing from:\n{help}");
        }
        assert!(
            help.contains("[possible values: project, server, rig, deploy, plugin]"),
            "{help}"
        );
    }

    #[test]
    fn config_has_four_actions() {
        let config = |line: &str| match parse_str(line).unwrap() {
            Command::Config { json, action } => (json, action),
            other => panic!("{line}: parsed as {other:?}"),
        };
        assert_eq!(config("config show"), (false, ConfigAction::Show));
        assert_eq!(config("config --json show"), (true, ConfigAction::Show));
        assert_eq!(config("config path --json"), (true, ConfigAction::Path));
        assert_eq!(
            config("config validate"),
            (false, ConfigAction::Validate { file: None })
        );
        assert_eq!(
            config("config validate b.toml --json"),
            (
                true,
                ConfigAction::Validate {
                    file: Some("b.toml".into())
                }
            )
        );
        assert_eq!(config("config schema"), (false, ConfigAction::Schema));
        for (line, kind) in [
            (
                "config",
                ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand,
            ),
            ("config shwo", ErrorKind::InvalidSubcommand),
            ("config show extra", ErrorKind::UnknownArgument),
            ("config validate a b", ErrorKind::UnknownArgument),
            ("config schema --bogus", ErrorKind::UnknownArgument),
        ] {
            let error = parse_str(line).unwrap_err();
            assert_eq!(error.kind(), kind, "{line}: {error}");
            assert_eq!(error.exit_code(), 2, "{line}");
        }
        assert!(err("config shwo").contains("a similar subcommand exists: 'show'"));
        assert!(help("config").contains("BRANCHYARD_USER_CONFIG"));
    }

    #[test]
    fn up_takes_plan_yes_and_json() {
        let up = |line: &str| match parse_str(line).unwrap() {
            Command::Up { json, plan, yes } => (json, plan, yes),
            other => panic!("{line}: parsed as {other:?}"),
        };
        assert_eq!(up("up"), (false, false, false));
        assert_eq!(up("up --plan --json"), (true, true, false));
        assert_eq!(up("up --yes"), (false, false, true));
        assert!(parse_str("up extra").is_err());
    }

    #[test]
    fn doctor_takes_json_and_a_path_and_the_global_server_options() {
        let doctor = |line: &str| match parse_str(line).unwrap() {
            Command::Doctor { json, path } => (json, path),
            other => panic!("{line}: parsed as {other:?}"),
        };
        assert_eq!(doctor("doctor"), (false, None));
        assert_eq!(doctor("doctor --json"), (true, None));
        assert_eq!(
            doctor("doctor --path ~/src/app --json"),
            (true, Some("~/src/app".into()))
        );
        // `--repo` is the global option naming a repository on a server, not
        // a path; doctor leaves it to the server check.
        let cli = parse_from(split_words("by doctor --repo app --remote http://h:1").unwrap());
        let cli = cli.unwrap();
        assert_eq!(cli.globals.repo.as_deref(), Some("app"));
        assert_eq!(cli.globals.remote.as_deref(), Some("http://h:1"));
        for line in ["doctor extra", "doctor --path", "doctor --bogus"] {
            let error = parse_str(line).unwrap_err();
            assert_eq!(error.exit_code(), 2, "{line}: {error}");
        }
        let text = help("doctor");
        for needle in [
            "--json",
            "--path <PATH>",
            "Exit status: 0",
            "docs/doctor.md",
        ] {
            assert!(text.contains(needle), "{needle} missing from:\n{text}");
        }
    }
}
