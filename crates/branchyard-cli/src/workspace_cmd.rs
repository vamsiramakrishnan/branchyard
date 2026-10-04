//! `by workspace show|trust|untrust|run`, and the trust decision every new
//! local branch goes through before its repository's `[workspace]`
//! scripts may run. See docs/workspace.md.
//!
//! Which `[workspace]` applies: the user file's
//! `[projects."<repository root>".workspace]` when it has one (yours, so
//! trusted), else the repository's own `branchyard.toml` `[workspace]`.
//!
//! Trust is per user, per repository root, and per content: the trust file
//! (`trusted-workspaces.json` beside the user configuration, or
//! `BRANCHYARD_TRUST_FILE`) maps a repository's canonical root to the
//! SHA-256 of its `[workspace]` section as it was trusted. A section with
//! no commands (only `copy`) needs no trust. Changing any glob or command
//! changes the digest, and the scripts are refused until trusted again.
//! On a terminal, `by run` (and fan, fork, reincarnate, rig) asks once;
//! otherwise it refuses and points to `by workspace trust`. A harness on a
//! branch (`BRANCHYARD_BRANCH` set) can never trust anything.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use branchyard::{WorkspacePhase, WorkspaceReport, WorkspaceSpec, Yard};
use branchyard_setup::config::{self, ProjectConfig, WorkspaceConfig, WorkspaceOrigin};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::args::WorkspaceAction;
use crate::commands::{self, print, Env, Failure, Outcome, Target};
use crate::setup_io;

/// The trust file's format version.
const TRUST_VERSION: u32 = 1;

/// Where the trust decisions are kept.
pub fn trust_file() -> PathBuf {
    if let Some(path) = std::env::var_os("BRANCHYARD_TRUST_FILE").filter(|v| !v.is_empty()) {
        return PathBuf::from(path);
    }
    setup_io::user_file().with_file_name("trusted-workspaces.json")
}

#[derive(Default, Serialize, Deserialize)]
struct TrustFile {
    version: u32,
    /// By canonical repository root.
    #[serde(default)]
    repositories: BTreeMap<String, Trusted>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Trusted {
    /// [`WorkspaceConfig::digest`] of the section trusted.
    digest: String,
    /// Seconds since the Unix epoch.
    trusted_at: u64,
}

fn read_trust() -> Result<TrustFile, Failure> {
    let path = trust_file();
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            Failure::Message(format!(
                "{} is not a trust file ({e}); remove it to start over",
                path.display()
            ))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TrustFile::default()),
        Err(e) => Err(Failure::Message(format!("{}: {e}", path.display()))),
    }
}

/// Write the trust file through a temporary file and a rename, private to
/// the user.
fn write_trust(trust: &TrustFile) -> Result<(), Failure> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = trust_file();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)
            .map_err(|e| Failure::Message(format!("create {}: {e}", dir.display())))?;
    }
    let temporary = path.with_extension(format!("json.{}", std::process::id()));
    let text = serde_json::to_string_pretty(trust).unwrap_or_default() + "\n";
    let written = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .and_then(|()| fs::rename(&temporary, &path));
    written.map_err(|e| Failure::Message(format!("write {}: {e}", path.display())))
}

/// Whether the trust decision for `root` covers `workspace`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustState {
    /// Nothing to trust: no commands, or your own user file's.
    NotNeeded,
    Trusted,
    /// Never trusted.
    Untrusted,
    /// Trusted once, and the section changed since.
    Changed,
}

/// The `[workspace]` that applies to a repository, and its trust state.
pub struct Resolved {
    pub root: PathBuf,
    pub workspace: WorkspaceConfig,
    pub origin: WorkspaceOrigin,
    /// The file it came from.
    pub file: PathBuf,
    pub digest: String,
    pub trust: TrustState,
}

impl Resolved {
    /// What the engine stores with a new branch.
    pub fn spec(&self) -> WorkspaceSpec {
        WorkspaceSpec {
            copy: self.workspace.copy.clone(),
            setup: self.workspace.setup.commands(),
            teardown: self.workspace.teardown.commands(),
            digest: Some(self.digest.clone()),
            prepare: self.workspace.prepare,
            inputs: self.workspace.inputs.clone(),
            share: self.workspace.share.clone(),
            pool: self.workspace.pool.as_ref().map(pool_spec),
        }
    }

    fn runs_ok(&self) -> bool {
        matches!(self.trust, TrustState::NotNeeded | TrustState::Trusted)
    }
}

/// What the engine stores for `[workspace.pool]`.
pub fn pool_spec(pool: &config::PoolConfig) -> branchyard::PoolSpec {
    branchyard::PoolSpec {
        size: pool.size,
        labels: pool.labels.clone(),
        max_age_secs: pool.max_age_secs(),
        max_behind: pool.max_behind,
        base: pool.base.clone(),
    }
}

/// The repository root spelled as the trust file and the user file's
/// `[projects]` keys are: canonical.
fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The `[workspace]` for the repository at `root`, if any applies.
pub fn resolve(root: &Path) -> Result<Option<Resolved>, Failure> {
    let root = canonical(root);
    let project_path = root.join(config::PROJECT_FILE);
    let project = match project_path.is_file() {
        true => Some(read(&project_path)?),
        false => None,
    };
    let user_path = setup_io::user_file();
    let user = match user_path.is_file() {
        true => {
            let mut user = read(&user_path)?;
            // Its keys name directories, however spelled.
            user.projects = std::mem::take(&mut user.projects)
                .into_iter()
                .map(|(key, value)| (canonical(Path::new(&key)).display().to_string(), value))
                .collect();
            Some(user)
        }
        false => None,
    };
    let root_text = root.display().to_string();
    let Some((workspace, origin)) =
        config::effective_workspace(&root_text, project.as_ref(), user.as_ref())
    else {
        return Ok(None);
    };
    let digest = workspace.digest();
    let trust = match (&origin, workspace.has_scripts()) {
        (WorkspaceOrigin::User, _) | (_, false) => TrustState::NotNeeded,
        (WorkspaceOrigin::Project, true) => match read_trust()?.repositories.get(&root_text) {
            Some(trusted) if trusted.digest == digest => TrustState::Trusted,
            Some(_) => TrustState::Changed,
            None => TrustState::Untrusted,
        },
    };
    let file = match origin {
        WorkspaceOrigin::User => user_path,
        WorkspaceOrigin::Project => project_path,
    };
    Ok(Some(Resolved {
        root,
        workspace,
        origin,
        file,
        digest,
        trust,
    }))
}

fn read(path: &Path) -> Result<ProjectConfig, Failure> {
    setup_io::read_layer(path).map_err(|e| {
        Failure::Message(format!(
            "{e}\n(fix it, or check it with `by config validate`)"
        ))
    })
}

/// Inside a harness running on a branch.
pub(crate) fn in_harness() -> Option<String> {
    std::env::var(branchyard::ENV_BRANCH)
        .ok()
        .filter(|v| !v.is_empty())
}

/// The scripts a trust decision is about, for a person to read.
fn describe(resolved: &Resolved) -> String {
    let w = &resolved.workspace;
    let mut text = format!(
        "{}'s [workspace] ({}) will run, as you, in each new branch's worktree:\n",
        resolved.root.display(),
        resolved.file.display()
    );
    if !w.copy.is_empty() {
        text.push_str(&format!("  copy      {}\n", w.copy.join(", ")));
    }
    for command in w.setup.commands() {
        text.push_str(&format!("  setup     {command}\n"));
    }
    for (name, run) in &w.run {
        for command in run.command.commands() {
            text.push_str(&format!("  run {name:<5} {command}\n"));
        }
    }
    for command in w.teardown.commands() {
        text.push_str(&format!("  teardown  {command}\n"));
    }
    text
}

fn refusal(resolved: &Resolved) -> Failure {
    let why = match resolved.trust {
        TrustState::Changed => "changed since you trusted it",
        _ => "has scripts you have not trusted",
    };
    Failure::Message(format!(
        "{}'s [workspace] in {} {why}, and there is no terminal to ask on; nothing was \
         created. Review it with `by workspace show`, then run `by workspace trust` in the \
         repository",
        resolved.root.display(),
        resolved.file.display()
    ))
}

/// Ask on the terminal whether to trust `resolved`; record a yes.
fn ask(resolved: &Resolved) -> Result<bool, Failure> {
    let changed = match resolved.trust {
        TrustState::Changed => " It changed since you last trusted it.",
        _ => "",
    };
    eprint!("{}", describe(resolved));
    let answer = crate::console::terminal_prompt(&format!(
        "by: trust these scripts for {}?{changed} [y/N] ",
        resolved.root.display()
    ))?;
    let yes = matches!(answer.trim(), "y" | "Y" | "yes" | "Yes");
    if yes {
        trust(resolved)?;
    }
    Ok(yes)
}

fn trust(resolved: &Resolved) -> Result<(), Failure> {
    let mut file = read_trust()?;
    file.version = TRUST_VERSION;
    file.repositories.insert(
        resolved.root.display().to_string(),
        Trusted {
            digest: resolved.digest.clone(),
            trusted_at: branchyard_support::time::now_ms() / 1000,
        },
    );
    write_trust(&file)
}

/// Require that `resolved`'s scripts may run: trusted already, or trusted
/// now on the terminal.
pub(crate) fn require_trust(env: &Env, resolved: &Resolved) -> Result<(), Failure> {
    if resolved.runs_ok() {
        return Ok(());
    }
    if in_harness().is_none() && env.stdin_tty && env.stderr_tty {
        return match ask(resolved)? {
            true => Ok(()),
            false => Err(Failure::Message(format!(
                "not trusted; nothing was created. Change or remove [workspace] in {}, or trust \
                 it with `by workspace trust`",
                resolved.file.display()
            ))),
        };
    }
    Err(refusal(resolved))
}

/// The workspace a new local branch of the repository at `root` is created
/// with: `None` without a `[workspace]`, or inside a harness (a delegated
/// branch takes its parent's). Refuses untrusted scripts; see the module
/// documentation.
pub fn for_new_branch(env: &Env, root: &Path) -> Result<Option<WorkspaceSpec>, Failure> {
    if in_harness().is_some() {
        return Ok(None);
    }
    let Some(resolved) = resolve(root)? else {
        // `.worktreeinclude` alone: its files are copied, nothing runs.
        let include = root.join(branchyard_workspace::include::WORKTREE_INCLUDE_FILE);
        return Ok(include.is_file().then(WorkspaceSpec::default));
    };
    require_trust(env, &resolved)?;
    Ok(Some(resolved.spec()))
}

/// `by workspace ACTION`.
pub fn main(env: &Env, target: &Target, action: &WorkspaceAction, json: bool) -> Outcome {
    if let Target::Remote(_) = target {
        return Err(Failure::Message(
            "by workspace acts on a local repository; a server runs a repository's workspace \
             scripts only when its operator allows them (allow_workspace_scripts)"
                .into(),
        ));
    }
    match action {
        WorkspaceAction::Show { branch } => show(branch.as_deref(), json),
        WorkspaceAction::Trust => trust_command(json),
        WorkspaceAction::Untrust => untrust_command(json),
        WorkspaceAction::Run { args, detach } => run(env, args, *detach, json),
        WorkspaceAction::Ports { branch } => ports(branch.as_deref(), json),
        WorkspaceAction::Browse {
            branch,
            port,
            print,
        } => browse(branch.as_deref(), *port, *print, json),
        WorkspaceAction::Kill { branch, port, yes } => {
            kill(env, branch.as_deref(), *port, *yes, json)
        }
    }
}

fn to_json(value: &serde_json::Value) -> String {
    format!(
        "{}\n",
        serde_json::to_string_pretty(value).unwrap_or_default()
    )
}

fn show(branch: Option<&str>, json: bool) -> Outcome {
    let yard = commands::open()?;
    if let Some(branch) = branch {
        let info = yard.workspace(branch)?;
        if json {
            let mut value = serde_json::to_value(&info).unwrap_or_default();
            value["listening"] = serde_json::to_value(
                crate::ports::of_yard(&yard)
                    .remove(branch)
                    .unwrap_or_default(),
            )
            .unwrap_or_default();
            return print(&to_json(&value));
        }
        let mut text = format!(
            "branch    {}\nworktree  {}\n",
            info.branch,
            info.worktree.display()
        );
        match &info.spec {
            None => text.push_str("workspace none (created without one)\n"),
            Some(spec) => {
                let state = match info.ready {
                    true => "ready",
                    false => "not ready: setup runs before its next turn",
                };
                text.push_str(&format!("setup     {state}\n"));
                if !spec.copy.is_empty() {
                    text.push_str(&format!("copy      {}\n", spec.copy.join(", ")));
                }
                for command in &spec.setup {
                    text.push_str(&format!("setup     {command}\n"));
                }
                for command in &spec.teardown {
                    text.push_str(&format!("teardown  {command}\n"));
                }
                if !info.copied.is_empty() {
                    text.push_str(&format!("copied    {}\n", info.copied.join(", ")));
                }
            }
        }
        match info.port {
            Some(port) => text.push_str(&format!("port      {port}\n")),
            None => text.push_str("port      none reserved\n"),
        }
        let listening = crate::ports::of_yard(&yard)
            .remove(branch)
            .unwrap_or_default();
        for line in crate::ports::lines(&listening) {
            text.push_str(&format!("listening {line}\n"));
        }
        return print(&text);
    }
    let resolved = resolve(yard.root())?;
    let ports: Vec<(String, Option<u16>, bool)> = yard
        .branches()?
        .into_iter()
        .filter_map(|b| {
            let info = yard.workspace(&b.name).ok()?;
            Some((b.name, info.port, info.ready))
        })
        .filter(|(_, port, _)| port.is_some())
        .collect();
    if json {
        let value = match &resolved {
            None => {
                json!({ "root": canonical(yard.root()), "workspace": null, "ports": ports_json(&ports) })
            }
            Some(r) => json!({
                "root": r.root,
                "workspace": r.workspace,
                "origin": r.origin,
                "file": r.file,
                "digest": r.digest,
                "trust": r.trust,
                "trust_file": trust_file(),
                "ports": ports_json(&ports),
            }),
        };
        return print(&to_json(&value));
    }
    let mut text = String::new();
    match &resolved {
        None => text.push_str(&format!(
            "No [workspace] for {}: new worktrees get nothing copied and run no setup.\nAdd one \
             to branchyard.toml (see docs/workspace.md), or run `by init project`.\n",
            canonical(yard.root()).display()
        )),
        Some(r) => {
            text.push_str(&describe(r));
            let origin = match r.origin {
                WorkspaceOrigin::Project => "the repository's branchyard.toml",
                WorkspaceOrigin::User => "your user configuration's [projects] entry",
            };
            text.push_str(&format!("from      {origin}\ndigest    {}\n", r.digest));
            let trust = match r.trust {
                TrustState::NotNeeded => "not needed".to_owned(),
                TrustState::Trusted => format!("trusted ({})", trust_file().display()),
                TrustState::Untrusted => {
                    "not trusted: `by workspace trust` to let its scripts run".to_owned()
                }
                TrustState::Changed => {
                    "changed since you trusted it: review it, then `by workspace trust`".to_owned()
                }
            };
            text.push_str(&format!("trust     {trust}\n"));
        }
    }
    for (name, port, ready) in &ports {
        let state = if *ready { "" } else { " (setup not complete)" };
        text.push_str(&format!(
            "port      {} {name}{state}\n",
            port.map(|p| p.to_string()).unwrap_or_default()
        ));
    }
    print(&text)
}

fn ports_json(ports: &[(String, Option<u16>, bool)]) -> serde_json::Value {
    ports
        .iter()
        .map(|(name, port, ready)| json!({ "branch": name, "port": port, "ready": ready }))
        .collect()
}

/// The repository root `by workspace trust` acts on: the yard's.
fn repository_root() -> Result<PathBuf, Failure> {
    Ok(commands::open()?.root().to_path_buf())
}

fn trust_command(json: bool) -> Outcome {
    if in_harness().is_some() {
        return Err(Failure::Message(
            "a harness running on a branch cannot trust workspace scripts; trusting is a \
             person's decision, made outside any branch"
                .into(),
        ));
    }
    let root = repository_root()?;
    let Some(resolved) = resolve(&root)? else {
        return Err(Failure::Message(format!(
            "{} has no [workspace] to trust",
            canonical(&root).display()
        )));
    };
    if resolved.trust != TrustState::NotNeeded {
        trust(&resolved)?;
    }
    if json {
        return print(&to_json(&json!({
            "root": resolved.root,
            "digest": resolved.digest,
            "trusted": true,
            "needed": resolved.trust != TrustState::NotNeeded,
            "trust_file": trust_file(),
        })));
    }
    match resolved.trust {
        TrustState::NotNeeded => print(&format!(
            "{}'s [workspace] needs no trust: {}\n",
            resolved.root.display(),
            match resolved.origin {
                WorkspaceOrigin::User => "it comes from your own user configuration",
                WorkspaceOrigin::Project => "it runs no commands",
            }
        )),
        _ => print(&format!(
            "{}trusted {} as it is now (digest {}); a change to it asks again\n",
            describe(&resolved),
            resolved.root.display(),
            &resolved.digest[..12]
        )),
    }
}

fn untrust_command(json: bool) -> Outcome {
    let root = canonical(&repository_root()?);
    let mut file = read_trust()?;
    let removed = file
        .repositories
        .remove(&root.display().to_string())
        .is_some();
    if removed {
        file.version = TRUST_VERSION;
        write_trust(&file)?;
    }
    if json {
        return print(&to_json(&json!({ "root": root, "removed": removed })));
    }
    match removed {
        true => print(&format!(
            "{}'s workspace scripts are no longer trusted\n",
            root.display()
        )),
        false => print(&format!("{} was not trusted\n", root.display())),
    }
}

/// `by workspace run [BRANCH] [NAME...]`: which branch and which scripts
/// (none: the default one).
fn pick(yard: &Yard, args: &[String]) -> Result<(String, Vec<String>), Failure> {
    let inside = in_harness();
    let known = |name: &str| yard.branch(name).is_ok();
    match (args, inside) {
        ([], Some(branch)) => Ok((branch, Vec::new())),
        ([], None) => Err(Failure::Message(
            "name the branch whose worktree it runs in: by workspace run BRANCH [NAME...]".into(),
        )),
        ([first, rest @ ..], _) if known(first) => Ok((first.clone(), rest.to_vec())),
        (names, Some(branch)) => Ok((branch, names.to_vec())),
        ([first, ..], None) => Err(branchyard::Error::UnknownBranch(first.clone()).into()),
    }
}

/// The ports reserved by the store, or listened on at 127.0.0.1, other
/// than by this process.
fn taken_ports(yard: &Yard) -> std::collections::BTreeSet<u16> {
    yard.branches()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|b| yard.workspace(&b.name).ok()?.port)
        .collect()
}

fn listening(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_err()
}

/// The port the `index`th of several detached scripts gets: the branch's
/// own for the first, then the next ports after it that no branch holds
/// and nothing listens on.
fn script_ports(
    base: Option<u16>,
    count: usize,
    taken: &std::collections::BTreeSet<u16>,
) -> Vec<Option<u16>> {
    let Some(base) = base else {
        return vec![None; count];
    };
    let mut out = vec![Some(base)];
    let mut next = base;
    while out.len() < count {
        next = match next.checked_add(1) {
            Some(n) => n,
            None => break,
        };
        if !taken.contains(&next) && !listening(next) {
            out.push(Some(next));
        }
    }
    out.resize(count, None);
    out
}

fn run(env: &Env, args: &[String], detach: bool, json: bool) -> Outcome {
    let yard = commands::open()?;
    let (branch, names) = pick(&yard, args)?;
    if names.len() > 1 {
        if !detach {
            return Err(Failure::Message(
                "several run scripts run at once only in the background: add --detach".into(),
            ));
        }
        return run_many(env, &yard, &branch, &names, json);
    }
    let name = names.into_iter().next();
    let info = yard.workspace(&branch)?;
    let resolved = resolve(yard.root())?.ok_or_else(|| {
        Failure::Message(format!(
            "{} has no [workspace] with run scripts",
            canonical(yard.root()).display()
        ))
    })?;
    let (name, commands) = resolved
        .workspace
        .run_script(name.as_deref())
        .map_err(Failure::Message)?;
    require_trust(env, &resolved)?;
    if !info.worktree.is_dir() {
        return Err(Failure::Message(format!(
            "{branch}'s worktree {} is missing",
            info.worktree.display()
        )));
    }
    let vars = yard.workspace_env(&branch)?;
    let port = vars
        .iter()
        .find(|(name, _)| name == branchyard::ENV_PORT)
        .and_then(|(_, v)| v.parse::<u16>().ok());
    let mut report = WorkspaceReport::new(WorkspacePhase::Run, port);
    // Each entry is its own `sh -c`, in order, stopping at the first that
    // fails, as setup runs its list: joining them into one script would
    // change what they mean (a `#` comment, a trailing `&`).
    let sh = |args: &[&str]| {
        let mut command = Command::new("sh");
        command
            .args(args)
            .current_dir(&info.worktree)
            .envs(vars.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        for name in [branchyard::ENV_TOKEN, branchyard::ENV_BY] {
            command.env_remove(name);
        }
        command
    };
    if detach {
        use std::os::unix::process::CommandExt;
        let logs = yard.root().join(".branchyard/logs");
        fs::create_dir_all(&logs)?;
        let log = logs.join(format!("{branch}.{name}.log"));
        let out = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)?;
        // The same sequence, detached: the entries are passed as
        // arguments, each run with `sh -c "$command"`, never concatenated.
        let mut args = vec![
            "-c",
            "for command do sh -c \"$command\" || exit; done",
            "by-workspace-run",
        ];
        args.extend(commands.iter().map(String::as_str));
        let child = sh(&args)
            .stdin(Stdio::null())
            .stdout(out.try_clone()?)
            .stderr(out)
            .process_group(0)
            .spawn()
            .map_err(|e| Failure::Message(format!("could not start sh: {e}")))?;
        report.commands = commands.clone();
        report.output = format!("pid {}, output in {}", child.id(), log.display());
        yard.record_workspace(&branch, report)?;
        if json {
            return print(&to_json(&json!({
                "branch": branch, "script": name, "pid": child.id(), "log": log, "port": port,
            })));
        }
        return print(&format!(
            "started {name} in {branch} (pid {}, port {}); output in {}\n",
            child.id(),
            port.map(|p| p.to_string()).unwrap_or_default(),
            log.display()
        ));
    }
    let started = Instant::now();
    let mut status = None;
    for command in &commands {
        eprintln!(
            "by: running {name} in {} (port {}): {command}",
            info.worktree.display(),
            port.map(|p| p.to_string()).unwrap_or_default()
        );
        report.commands.push(command.clone());
        let ran = sh(&["-c", command])
            .status()
            .map_err(|e| Failure::Message(format!("could not start sh: {e}")))?;
        status = Some(ran);
        if !ran.success() {
            break;
        }
    }
    report.duration_ms = started.elapsed().as_millis() as u64;
    let code = status.and_then(|s| s.code());
    let success = status.is_none_or(|s| s.success());
    report.exit_code = code;
    report.ok = success;
    if status.is_some_and(|s| s.code().is_none()) {
        report.error = Some("was killed by a signal".into());
    }
    yard.record_workspace(&branch, report)?;
    if json {
        print(&to_json(&json!({
            "branch": branch, "script": name, "exit_code": code, "ok": success,
        })))?;
    }
    match success {
        true => Ok(()),
        false => Err(Failure::Reported),
    }
}

/// One script started in the background: what `run_many` reports.
#[derive(Serialize)]
struct Started {
    script: String,
    pid: u32,
    port: Option<u16>,
    log: PathBuf,
}

/// `by workspace run BRANCH A B ... --detach`: each named script in its own
/// process group, at once, the first with the branch's port and each next
/// with the next free one (as `BRANCHYARD_PORT`; the branch's own is
/// `BRANCHYARD_BRANCH_PORT` for all of them). Each start is a `run` event.
#[allow(clippy::map_unwrap_or)] // ratchet: branchyard-cli
fn run_many(env: &Env, yard: &Yard, branch: &str, names: &[String], json: bool) -> Outcome {
    use std::os::unix::process::CommandExt;
    let info = yard.workspace(branch)?;
    let resolved = resolve(yard.root())?.ok_or_else(|| {
        Failure::Message(format!(
            "{} has no [workspace] with run scripts",
            canonical(yard.root()).display()
        ))
    })?;
    let mut seen = std::collections::BTreeSet::new();
    let scripts = names
        .iter()
        .map(|name| {
            if !seen.insert(name.as_str()) {
                return Err(Failure::Message(format!("{name} is named twice")));
            }
            resolved
                .workspace
                .run_script(Some(name))
                .map_err(Failure::Message)
        })
        .collect::<Result<Vec<_>, _>>()?;
    require_trust(env, &resolved)?;
    if !info.worktree.is_dir() {
        return Err(Failure::Message(format!(
            "{branch}'s worktree {} is missing",
            info.worktree.display()
        )));
    }
    let vars = yard.workspace_env(branch)?;
    let base = vars
        .iter()
        .find(|(name, _)| name == branchyard::ENV_PORT)
        .and_then(|(_, v)| v.parse::<u16>().ok());
    let mut taken = taken_ports(yard);
    if let Some(base) = base {
        taken.remove(&base);
    }
    let ports = script_ports(base, scripts.len(), &taken);
    let logs = yard.root().join(".branchyard/logs");
    fs::create_dir_all(&logs)?;
    let mut started = Vec::new();
    for ((name, commands), port) in scripts.into_iter().zip(ports) {
        let log = logs.join(format!("{branch}.{name}.log"));
        let out = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)?;
        let mut args = vec![
            "-c".to_owned(),
            "for command do sh -c \"$command\" || exit; done".to_owned(),
            "by-workspace-run".to_owned(),
        ];
        args.extend(commands.iter().cloned());
        let mut command = Command::new("sh");
        command
            .args(&args)
            .current_dir(&info.worktree)
            .envs(vars.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        for name in [branchyard::ENV_TOKEN, branchyard::ENV_BY] {
            command.env_remove(name);
        }
        if let Some(port) = port {
            command.env(branchyard::ENV_PORT, port.to_string());
        }
        if let Some(base) = base {
            command.env("BRANCHYARD_BRANCH_PORT", base.to_string());
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(out.try_clone()?)
            .stderr(out)
            .process_group(0)
            .spawn()
            .map_err(|e| Failure::Message(format!("could not start sh: {e}")))?;
        let mut report = WorkspaceReport::new(WorkspacePhase::Run, port);
        report.commands = commands.clone();
        report.output = format!("{name}: pid {}, output in {}", child.id(), log.display());
        yard.record_workspace(branch, report)?;
        started.push(Started {
            script: name,
            pid: child.id(),
            port,
            log,
        });
    }
    if json {
        return print(&to_json(&json!({ "branch": branch, "started": started })));
    }
    let mut text = String::new();
    for s in &started {
        text.push_str(&format!(
            "started {} in {branch} (pid {}, port {}); output in {}\n",
            s.script,
            s.pid,
            s.port
                .map(|p| p.to_string())
                .unwrap_or_else(|| "none".into()),
            s.log.display()
        ));
    }
    print(&text)
}

/// The branch an action on ports is for: the one named, else the harness's.
fn ports_branch(branch: Option<&str>, what: &str) -> Result<String, Failure> {
    match (branch, in_harness()) {
        (Some(branch), _) => Ok(branch.to_owned()),
        (None, Some(branch)) => Ok(branch),
        (None, None) => Err(Failure::Message(format!(
            "name the branch: by workspace {what} BRANCH"
        ))),
    }
}

/// `by workspace ports [BRANCH]`.
fn ports(branch: Option<&str>, json: bool) -> Outcome {
    let yard = commands::open()?;
    if let Some(branch) = branch {
        yard.branch(branch)?;
    }
    let mut all = crate::ports::of_yard(&yard);
    if let Some(branch) = branch {
        all.retain(|b, _| b == branch);
    }
    if json {
        let list: Vec<&crate::ports::Listener> = all.values().flatten().collect();
        return print(&to_json(&serde_json::to_value(list).unwrap_or_default()));
    }
    if all.is_empty() {
        return print(&match branch {
            Some(branch) => format!("{branch}'s processes listen on no TCP port\n"),
            None => "no branch's processes listen on a TCP port\n".to_owned(),
        });
    }
    let mut text = String::new();
    for (branch, listeners) in &all {
        for line in crate::ports::lines(listeners) {
            text.push_str(&format!("{branch}  {line}\n"));
        }
    }
    print(&text)
}

/// `by workspace browse [BRANCH] [--port N] [--print]`.
fn browse(branch: Option<&str>, port: Option<u16>, print_only: bool, json: bool) -> Outcome {
    let yard = commands::open()?;
    let branch = ports_branch(branch, "browse")?;
    let info = yard.workspace(&branch)?;
    let listening = crate::ports::of_yard(&yard)
        .remove(&branch)
        .unwrap_or_default();
    let chosen = match port {
        Some(port) => listening.iter().find(|l| l.port == port).cloned(),
        None => match listening.as_slice() {
            [one] => Some(one.clone()),
            many => many
                .iter()
                .find(|l| Some(l.port) == info.port)
                .cloned()
                .or_else(|| many.iter().find(|l| l.protocol != "unknown").cloned()),
        },
    };
    let Some(listener) = chosen else {
        return Err(Failure::Message(match (port, listening.is_empty()) {
            (Some(port), _) => format!("{branch} does not listen on port {port}"),
            (None, true) => format!(
                "{branch}'s processes listen on no TCP port; start one with by workspace run \
                 {branch} --detach"
            ),
            (None, false) => format!(
                "{branch} listens on several ports ({}); pick one with --port",
                listening
                    .iter()
                    .map(|l| l.port.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }));
    };
    let url = listener.url();
    if json {
        return print(&to_json(
            &json!({ "branch": branch, "port": listener.port, "url": url }),
        ));
    }
    if print_only {
        return print(&format!("{url}\n"));
    }
    crate::ports::open_browser(&url).map_err(Failure::Message)?;
    print(&format!("opened {url} ({branch})\n"))
}

/// `by workspace kill [BRANCH] [--port N] [--yes]`: SIGTERM to each
/// process listening on the branch's ports.
fn kill(env: &Env, branch: Option<&str>, port: Option<u16>, yes: bool, json: bool) -> Outcome {
    let yard = commands::open()?;
    let branch = ports_branch(branch, "kill")?;
    yard.branch(&branch)?;
    let listening = crate::ports::of_yard(&yard)
        .remove(&branch)
        .unwrap_or_default();
    let mut pids: Vec<(u32, u16, String)> = listening
        .iter()
        .filter(|l| port.is_none_or(|p| l.port == p))
        .filter_map(|l| Some((l.pid?, l.port, l.process.clone().unwrap_or_default())))
        .collect();
    pids.sort();
    pids.dedup_by_key(|p| p.0);
    if pids.is_empty() {
        return Err(Failure::Message(match port {
            Some(port) => format!("no process of {branch} listens on port {port}"),
            None => format!("{branch}'s processes listen on no TCP port"),
        }));
    }
    let described: Vec<String> = pids
        .iter()
        .map(|(pid, port, name)| format!("{name} (pid {pid}) on :{port}"))
        .collect();
    if !yes {
        if !env.stdin_tty {
            return Err(Failure::Message(format!(
                "would stop {}; pass --yes to do it without a terminal",
                described.join(", ")
            )));
        }
        eprint!("stop {}? [y/N] ", described.join(", "));
        std::io::stderr().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            return print("nothing stopped\n");
        }
    }
    let mut stopped = Vec::new();
    for (pid, port, _) in &pids {
        let ok = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        stopped.push(json!({ "pid": pid, "port": port, "signalled": ok }));
    }
    if json {
        return print(&to_json(&json!({ "branch": branch, "stopped": stopped })));
    }
    print(&format!("sent SIGTERM to {}\n", described.join(", ")))
}

/// The digest trusted under `key` in the trust file, if any: for trust
/// decisions about something other than a repository's `[workspace]`,
/// such as one of its `[recipes]` (docs/recipes.md), whose keys are the
/// repository's canonical root with a suffix of their own.
pub(crate) fn trusted_digest(key: &str) -> Result<Option<String>, Failure> {
    Ok(read_trust()?
        .repositories
        .get(key)
        .map(|t| t.digest.clone()))
}

/// Record `digest` as trusted under `key`.
pub(crate) fn record_trust(key: &str, digest: &str) -> Result<(), Failure> {
    let mut file = read_trust()?;
    file.version = TRUST_VERSION;
    file.repositories.insert(
        key.to_owned(),
        Trusted {
            digest: digest.to_owned(),
            trusted_at: branchyard_support::time::now_ms() / 1000,
        },
    );
    write_trust(&file)
}

/// Forget the decision under `key`; whether there was one.
pub(crate) fn forget_trust(key: &str) -> Result<bool, Failure> {
    let mut file = read_trust()?;
    let removed = file.repositories.remove(key).is_some();
    if removed {
        file.version = TRUST_VERSION;
        write_trust(&file)?;
    }
    Ok(removed)
}
