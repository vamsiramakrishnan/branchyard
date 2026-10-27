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
use crate::errors::Report;
use crate::json;
use crate::remote::{self, Remote};
use crate::render::{self, Renderer, Style, Tone};
use crate::rig;

mod artifacts;
mod branch;
mod delegation;
mod live;
mod rig_runs;
mod run;

pub use self::artifacts::*;
pub use self::branch::*;
pub use self::delegation::*;
pub use self::live::*;
pub use self::rig_runs::*;
pub use self::run::*;

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

/// How a command failed. `crate::errors::finish` prints it, once, and
/// exits with the status of its kind (docs/cli.md, "Exit status").
#[derive(Debug)]
pub enum Failure {
    Sdk(branchyard::Error),
    Remote(branchyard_client::Error),
    Io(io::Error),
    /// What failed, its kind and the command to run next
    /// (`Report::new(kind, what).next(..).into()`): the one shape a new
    /// failure takes.
    Report(Box<Report>),
    /// A bare message, reported with the kind `unclassified` (exit 1).
    /// The old shape: tools/failure_message_ratchet.json counts what is
    /// left, and a new failure is a [`Failure::Report`].
    Message(String),
    /// Already explained, such as a branch that failed or a doctor's
    /// failing check; exit 1. A failure with a kind of its own is a
    /// [`Failure::Report`] instead, which `crate::errors::finish` prints
    /// once, with its kind's status.
    Reported,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Sdk(error) => write!(f, "{error}"),
            // A server that does not answer ends with the way out.
            Failure::Remote(error @ branchyard_client::Error::Transport { .. }) => {
                write!(f, "{error}\nNext: {}", remote::UNREACHABLE_FIX)
            }
            Failure::Remote(error) => write!(f, "{error}"),
            Failure::Io(error) => write!(f, "{error}"),
            Failure::Report(report) => f.write_str(&report.said()),
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
    value.ok_or_else(|| {
        Report::usage(format!("missing the {name} argument"))
            .next("see by help <command> for its arguments")
            .into()
    })
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
        // The absolute path, so that "not inside a repository" names where.
        None => Yard::open(std::env::current_dir().unwrap_or_else(|_| ".".into())),
    }?;
    configure(yard)
}

/// `yard` with what the configuration files give it (the gateways, the
/// approvals, the knowledge settings and the task defaults;
/// [`Yard::configure`]), as [`open`] returns it. A harness on a branch
/// reads no configuration: its `by` acts for that branch, whose settings
/// the engine already set.
pub(crate) fn configure(yard: Yard) -> Result<Yard, branchyard::Error> {
    if crate::models_cmd::inside_branch() {
        return Ok(yard);
    }
    let config = yard
        .configuration()?
        .map(|effective| effective.config)
        .unwrap_or_default();
    yard.configure(&config)?;
    Ok(yard)
}

/// The line `by` prints before it runs a harness, one for each way it can
/// run: `local` (your user, your `HOME`, no sandbox), `isolated` (your
/// user, a private `HOME`, still no sandbox), a sandbox provider, or, with
/// `server` (its label), a server doing either on its host. It says what
/// the mode does not give as plainly as what it does.
pub(crate) fn banner(provider: Option<&Provider>, isolated: bool, server: Option<&str>) -> String {
    let (mode, what) = Provider::banner_of(provider, isolated, server.is_some());
    match server {
        None => format!("by: {mode} mode: {what}"),
        Some(label) => format!("by: remote mode on {label}: {what}"),
    }
}

/// The line `by` prints before a local run whose preset names a file
/// scope: `by: guard: landlock (edit-worktree): ...`, or `by: note: no
/// file guard on this host (...)` where Landlock cannot hold it. A
/// `--plan` run's planning turns are read-only. `None` for `--yes`,
/// `--ask`, `full` and no preset, which name no scope.
pub(crate) fn guard_line(task: &TaskArgs, isolated: bool) -> Option<String> {
    let preset = match task.permissions {
        _ if task.plan => Some(branchyard::PolicyPreset::ReadOnly),
        Permissions::Preset(preset) => Some(preset),
        _ => None,
    };
    Some(match branchyard::guard::banner(preset, isolated)? {
        Ok(line) => format!("by: guard: {line}"),
        Err(note) => format!("by: note: {note}"),
    })
}

/// Whether the local-mode note is due in this repository, and the marker
/// that records it was said: the note is a fact about the repository's
/// setup, said until a run it was printed for has started a harness, and not
/// on every run after. A command that fails before its harness starts (an
/// unknown harness, a refused flag) does not spend it. Without a
/// `.branchyard/` directory to keep the marker in, it is said every time.
fn local_note() -> (bool, Option<std::path::PathBuf>) {
    let Some(state) = std::env::current_dir().ok().and_then(|cwd| {
        cwd.ancestors()
            .map(|dir| dir.join(".branchyard"))
            .find(|d| d.is_dir())
    }) else {
        return (true, None);
    };
    let marker = state.join("local-mode-noted");
    match marker.exists() {
        true => (false, None),
        false => (true, Some(marker)),
    }
}

/// Record that the local-mode note was said, once a harness it was said
/// for has started: the marker [`local_note`] named, written at most once.
fn note_said(marker: &std::sync::Mutex<Option<std::path::PathBuf>>) {
    use branchyard_support::LockExt as _;
    let Some(marker) = marker.lock_recovering("local-mode note").take() else {
        return;
    };
    best_effort(
        "record that the local-mode note was printed",
        std::fs::write(
            &marker,
            "harnesses run as your user, with no isolation beyond it\n",
        ),
    );
}

/// Serve a branch's delegation tools on stdio; see `branchyard-mcp`.
pub fn mcp(root: &str, branch: &str) -> Outcome {
    branchyard_mcp::serve_branch(root.into(), branch)
        .map_err(|e| Report::new("io", format!("by mcp for {branch}: {e}")).into())
}

/// `by mcp --list-tools`: the tools `by mcp` serves, with their input
/// schemas, as the MCP `tools/list` result. Needs no branch or token.
pub fn mcp_tools() -> Outcome {
    let listing = serde_json::json!({ "tools": branchyard_mcp::tools() });
    print(&format!("{}\n", to_json(&listing)))
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

/// Fail with `error`'s report (see [`crate::errors`]): `{"error": {"kind",
/// "message", "next", "detail", "exit"}}` on stdout with `json`, else the
/// text on stderr, and the exit status of its kind.
pub(crate) fn fail(json: bool, error: &branchyard::Error) -> Outcome {
    fail_in(json, error, None)
}

/// [`fail`] for a command that acts on `branch`, whose hints may name it.
pub(crate) fn fail_in(json: bool, error: &branchyard::Error, branch: Option<&str>) -> Outcome {
    emit_report(json, crate::errors::sdk(error, branch))
}

/// Fail with `report`, as JSON with `json` or as text, as [`fail`] does.
pub(crate) fn emit_report(json: bool, report: Report) -> Outcome {
    Err(report.with_json(json).into())
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
        branchyard::Error::Usage(format!(
            "{command} needs a branch outside a harness: `by {command} <branch>` (`by ls` lists \
             them)"
        ))
    })
}

/// `--prompt-file PATH`: the prompt a file holds, `-` for standard input,
/// without its trailing newlines. An empty one is refused, as an empty
/// prompt is.
pub fn read_prompt_file(path: &str) -> Result<String, Failure> {
    let text = match path {
        "-" => {
            let mut text = String::new();
            io::Read::read_to_string(&mut io::stdin(), &mut text).map_err(|e| {
                Report::new(
                    "io",
                    format!(
                        "--prompt-file -: could not read standard input: {}",
                        crate::errors::plain_io(&e)
                    ),
                )
            })?;
            text
        }
        path => std::fs::read_to_string(path).map_err(|e| {
            Report::usage(format!(
                "--prompt-file {path}: {}",
                crate::errors::plain_io(&e)
            ))
            .next("give a file that holds the prompt, or - to read it from standard input")
        })?,
    };
    let text = text.trim_end_matches(['\n', '\r']).to_owned();
    match text.trim().is_empty() {
        true => Err(
            Report::usage(format!("--prompt-file {path} holds no prompt"))
                .next("write the prompt into the file, or pass it as an argument")
                .into(),
        ),
        false => Ok(text),
    }
}

/// A JSON value given as text (`--result '{...}'`) or in a file
/// (`--result-file PATH`, `-` for standard input); `None` when neither was
/// given.
fn json_flag(
    flag: &str,
    given: (Option<&str>, Option<&str>),
) -> Result<Option<serde_json::Value>, branchyard::Error> {
    let denied = |why: String| branchyard::Error::Denied(why);
    let text = match given {
        (Some(text), _) => text.to_owned(),
        (None, Some("-")) => {
            let mut text = String::new();
            io::Read::read_to_string(&mut io::stdin(), &mut text)
                .map_err(|e| denied(format!("{flag}-file -: {e}")))?;
            text
        }
        (None, Some(path)) => {
            std::fs::read_to_string(path).map_err(|e| denied(format!("{flag}-file {path}: {e}")))?
        }
        (None, None) => return Ok(None),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| denied(format!("{flag} is not JSON: {e}")))
}

/// `by spawn`'s result contract: its schema, from `--result-schema` or
/// `--result-schema-file`, checked against the subset the contract
/// supports.
fn result_contract(
    args: &SpawnArgs,
) -> Result<Option<branchyard::ResultContract>, branchyard::Error> {
    let given = (
        args.result_schema.as_deref(),
        args.result_schema_file.as_deref(),
    );
    json_flag("--result-schema", given)?
        .map(|schema| branchyard::ResultContract::new(schema, args.result_required))
        .transpose()
}

pub fn harnesses(env: &Env, target: &Target, as_json: bool) -> Outcome {
    let harnesses = match target {
        // The table is static: no repository needed.
        Target::Local => branchyard::harnesses(),
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
        let text = std::fs::read_to_string(path).map_err(|e| {
            Report::usage(format!(
                "--instructions {path}: {}",
                crate::errors::plain_io(&e)
            ))
            .next("give a file that holds the instructions")
        })?;
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
            max_kept_secs: substrate.lifecycle.max_kept_secs(),
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
            max_kept_secs: sandbox.lifecycle.max_kept_secs(),
            live_branch: sandbox.live_branch,
        })),
        (None, true) => Some(Provider::Local),
        (None, false) => None,
    })
}

/// `by merge` without `--into` on a detached HEAD: there is no branch to
/// merge into.
pub(crate) fn detached_head() -> Report {
    Report::usage("HEAD is detached, so there is no branch to merge into")
        .next("pass --into <branch>")
        .next("git switch <branch>, then merge again")
}

#[cfg(test)]
mod tests;
