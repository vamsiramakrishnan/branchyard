//! Command-line parsing for `by`, with clap's derive API.
//!
//! [`Cli`] is the whole command line: [`Globals`], which choose where
//! commands run and may come before or after the command, and one
//! [`Command`]. `by --help`, `by help <command>`, typo suggestions, shell
//! completions (`by completions`) and the man page (`by man`) are all
//! generated from these types.
//!
//! # Layout
//!
//! - this file: [`Cli`], [`Globals`], [`TaskArgs`] and [`Permissions`];
//! - `command.rs`: [`Command`], one variant per command;
//! - `actions.rs`: the nested subcommands and argument groups of the
//!   commands that take an action;
//! - `flags.rs`: the flag groups commands flatten and the checked flag sets;
//! - `help.rs`: the command groups, each command's examples and the help
//!   listings;
//! - `parse.rs`: parsing on a large stack, error hints and value parsers;
//! - `reference.rs`: the Markdown command reference (`docs/cli-reference.md`);
//! - `tests.rs`: the parser's tests.
//!
//! # Adding a command
//!
//! 1. Add a variant to [`Command`] in `command.rs`. Its doc comment is its one-line summary
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
use std::time::Duration;

use clap::builder::styling::Style;
use clap::builder::{PossibleValue, StringValueParser, TypedValueParser};
use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{
    ArgGroup, ArgMatches, Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum,
    ValueHint,
};

mod actions;
mod command;
mod flags;
mod help;
mod parse;
mod reference;

pub use self::actions::*;
pub use self::command::*;
pub use self::flags::*;
pub use self::help::*;
pub use self::parse::*;
pub use self::reference::*;

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

#[allow(clippy::unwrap_in_result)] // tests: a panic is the failure report
#[cfg(test)]
mod tests;
