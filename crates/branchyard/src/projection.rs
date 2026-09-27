//! What a delegating turn gives its harness, and what the engine keeps
//! while the turn runs.
//!
//! When a branch may create children, its turn gets:
//!
//! - A token, issued when the turn starts and revoked when it ends, in
//!   [`ENV_TOKEN`] and in `.branchyard/delegation/<branch>.json` (mode
//!   0600) with the broker's socket, so `by`, the Python module and the MCP
//!   server can reach the engine that runs the turn.
//! - [`ENV_BY`] when `by` was found, with `by`'s directory first on `PATH`,
//!   and the Python module's directory first on `PYTHONPATH`.
//! - Branchyard's MCP server (`by mcp`, or `branchyard-mcp`).
//! - The delegation skill as standing instructions (see
//!   [`branchyard_harness::Instructions`]).
//!
//! The Python module and the skill are written under `.branchyard/`, which
//! git ignores through `info/exclude`, never into the branch's worktree, so
//! they never reach a candidate.
//!
//! Every harness the engine starts, delegating or not, gets [`ENV_ROOT`] and
//! [`ENV_BRANCH`], so `by` run inside one knows it is not a person and
//! refuses to act without a token.
//!
//! Every variable name avoids `KEY`, `SECRET` and `TOKEN`: Codex drops
//! such variables from the environment of the shell commands it runs by
//! default (`shell_environment_policy`), and `by` must see them there.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use branchyard_harness::{Instructions, McpServer};
use serde::{Deserialize, Serialize};

use crate::broker::Broker;
use crate::delegation::Grant;
use crate::state::Record;
use crate::{harness, Budget, Error, Policy, TaskOptions, Yard};

/// The per-turn delegation token.
pub const ENV_TOKEN: &str = "BRANCHYARD_DELEGATION";
/// The branch the token was issued to.
pub const ENV_BRANCH: &str = "BRANCHYARD_BRANCH";
/// The repository root, where `.branchyard/` is. The harness's working
/// directory is its branch's worktree, which is not that root.
pub const ENV_ROOT: &str = "BRANCHYARD_ROOT";
/// The absolute path of the `by` the engine exposed.
pub const ENV_BY: &str = "BRANCHYARD_BY";

/// The name harnesses know Branchyard's MCP server by. Claude Code names its
/// tools `mcp__branchyard__<tool>`.
pub(crate) const SERVER_NAME: &str = "branchyard";

/// The Python module, written to `.branchyard/sdk/python/branchyard.py`.
const PYTHON_MODULE: &str = include_str!("../../../sdk/python/branchyard.py");
/// The Claude Code plugin carrying the delegation skill, written to
/// `.branchyard/plugin/`.
const PLUGIN_MANIFEST: &str =
    include_str!("../../../plugins/branchyard/.claude-plugin/plugin.json");
const SKILL: &str = include_str!("../../../plugins/branchyard/skills/delegate/SKILL.md");

/// The skill without its frontmatter, for harnesses that take instructions
/// as text.
pub(crate) fn skill_text() -> &'static str {
    SKILL
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .map_or(SKILL, |(_, body)| body)
        .trim()
}

/// A running turn's delegation authority.
pub(crate) struct Context {
    pub token: String,
    /// The options its turn runs with, budget and policy already bounded.
    pub options: TaskOptions,
    /// The branch's own spend so far, updated as the harness reports it.
    pub cost: Arc<Mutex<Option<f64>>>,
}

/// Delegation state shared by a yard's clones.
#[derive(Default)]
pub(crate) struct Hub {
    pub contexts: Mutex<HashMap<String, Context>>,
    pub running: Mutex<HashMap<String, JoinHandle<()>>>,
    broker: Mutex<Option<Broker>>,
    /// Checks and reservations for one spawn or send happen together.
    pub spawning: Mutex<()>,
}

impl fmt::Debug for Hub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hub")
            .field("contexts", &lock(&self.contexts).len())
            .field("running", &lock(&self.running).len())
            .finish_non_exhaustive()
    }
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Hub {
    /// Record `name`'s running turn, starting the broker if needed, and
    /// return the broker's socket.
    fn register(&self, yard: &Yard, name: &str, context: Context) -> Result<PathBuf, Error> {
        let mut broker = lock(&self.broker);
        lock(&self.contexts).insert(name.to_owned(), context);
        if broker.is_none() {
            match Broker::start(yard.clone()) {
                Ok(started) => *broker = Some(started),
                Err(error) => {
                    lock(&self.contexts).remove(name);
                    return Err(Error::State(format!(
                        "could not start the delegation broker: {error}"
                    )));
                }
            }
        }
        Ok(broker.as_ref().expect("started above").path().to_path_buf())
    }

    /// Revoke `name`'s context if it still holds `token`; stop the broker
    /// when no turn delegates any more.
    fn unregister(&self, name: &str, token: &str) {
        let stopped = {
            let mut broker = lock(&self.broker);
            let mut contexts = lock(&self.contexts);
            if contexts.get(name).is_some_and(|c| c.token == token) {
                contexts.remove(name);
            }
            match contexts.is_empty() {
                true => broker.take(),
                false => None,
            }
        };
        if let Some(broker) = stopped {
            broker.stop();
        }
    }
}

/// `.branchyard/delegation/<branch>.json`, written for the harness's tools.
#[derive(Serialize, Deserialize)]
pub(crate) struct TokenFile {
    pub branch: String,
    pub token: String,
    /// The broker's Unix socket.
    pub broker: PathBuf,
    /// The engine process.
    pub pid: u32,
}

/// Where delegating harnesses find Branchyard.
pub(crate) struct Tools {
    /// `by`, when found.
    pub by: Option<PathBuf>,
    /// The MCP server's argument vector, with an absolute executable.
    pub server: Vec<String>,
}

/// Find `by` and the MCP server for `options`.
pub(crate) fn tools(options: &TaskOptions) -> Result<Tools, Error> {
    let unavailable = |reason: String| {
        Error::Unsupported(format!(
            "delegation needs Branchyard's `by` or MCP server, and {reason}"
        ))
    };
    let beside = |name: &str| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
            .filter(|path| harness::executable(path))
    };
    let by = match &options.delegation_cli {
        Some(path) => {
            let path = std::path::absolute(path)
                .map_err(|e| unavailable(format!("{}: {e}", path.display())))?;
            if !harness::executable(&path) {
                return Err(unavailable(format!(
                    "{} is not an executable file",
                    path.display()
                )));
            }
            Some(path)
        }
        None => std::env::current_exe()
            .ok()
            .filter(|exe| exe.file_name().is_some_and(|n| n == "by"))
            .or_else(|| beside("by"))
            .or_else(|| harness::find_on_path("by")),
    };
    let mut server = match (&options.delegation_server, &by) {
        (Some(argv), _) => argv.clone(),
        (None, Some(by)) => vec![by.display().to_string(), "mcp".into()],
        (None, None) => match beside("branchyard-mcp")
            .or_else(|| harness::find_on_path("branchyard-mcp"))
        {
            Some(path) => vec![path.display().to_string()],
            None => {
                return Err(unavailable(
                    "neither `by` nor `branchyard-mcp` is beside this executable or on PATH".into(),
                ))
            }
        },
    };
    let Some(program) = server.first().filter(|p| !p.is_empty()) else {
        return Err(unavailable("the MCP server command is empty".into()));
    };
    let path = if program.contains('/') {
        std::path::absolute(program).map_err(|e| unavailable(format!("{program}: {e}")))?
    } else {
        harness::find_on_path(program)
            .ok_or_else(|| unavailable(format!("{program} was not found on PATH")))?
    };
    if !harness::executable(&path) {
        return Err(unavailable(format!(
            "{} is not an executable file",
            path.display()
        )));
    }
    server[0] = path.display().to_string();
    Ok(Tools { by, server })
}

/// What one delegating turn was given. Dropping it revokes the token.
pub(crate) struct Projection {
    yard: Yard,
    name: String,
    token: String,
    pub server: McpServer,
    /// Variables set on the harness process.
    pub env: Vec<(String, String)>,
    pub instructions: Instructions,
    cost: Arc<Mutex<Option<f64>>>,
}

impl Projection {
    /// The branch's own spend so far this turn, for its children's budget
    /// checks.
    pub fn observe_cost(&self, spent: f64) {
        let mut cost = lock(&self.cost);
        *cost = Some(cost.map_or(spent, |c| c.max(spent)));
    }
}

impl Drop for Projection {
    fn drop(&mut self) {
        let path = self.yard.store().token_path(&self.name);
        let ours = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<TokenFile>(&text).ok())
            .is_some_and(|file| file.token == self.token);
        if ours {
            let _ = fs::remove_file(&path);
        }
        self.yard.hub.unregister(&self.name, &self.token);
    }
}

/// Offer delegation to `record`'s turn if it may create children: issue a
/// token, register the turn, and describe what the harness gets.
pub(crate) fn project(
    yard: &Yard,
    record: &Record,
    options: &TaskOptions,
    budget: Budget,
    policy: Policy,
) -> Result<Option<Projection>, Error> {
    if !record.grant.as_ref().is_some_and(Grant::can_spawn) {
        return Ok(None);
    }
    let name = record.info.name.clone();
    let tools = tools(options)?;
    let store = yard.store();
    let dir = store.dir().to_path_buf();
    let python = dir.join("sdk").join("python");
    let plugin = dir.join("plugin");
    install(&python.join("branchyard.py"), PYTHON_MODULE)?;
    install(
        &plugin.join(".claude-plugin").join("plugin.json"),
        PLUGIN_MANIFEST,
    )?;
    install(
        &plugin.join("skills").join("delegate").join("SKILL.md"),
        SKILL,
    )?;

    let token = new_token()?;
    let cost = Arc::new(Mutex::new(record.info.cost_usd));
    let context = Context {
        token: token.clone(),
        options: TaskOptions {
            budget,
            policy,
            ..options.clone()
        },
        cost: cost.clone(),
    };
    let socket = yard.hub.register(yard, &name, context)?;
    let root = yard.root.display().to_string();
    let mut env = vec![
        (ENV_ROOT.to_owned(), root.clone()),
        (ENV_BRANCH.to_owned(), name.clone()),
        (ENV_TOKEN.to_owned(), token.clone()),
        (
            "PYTHONPATH".to_owned(),
            prepend(&python, std::env::var_os("PYTHONPATH")),
        ),
    ];
    if let Some(by) = &tools.by {
        env.push((ENV_BY.to_owned(), by.display().to_string()));
        if let Some(dir) = by.parent() {
            env.push(("PATH".to_owned(), prepend(dir, std::env::var_os("PATH"))));
        }
    }
    let projection = Projection {
        yard: yard.clone(),
        name: name.clone(),
        token: token.clone(),
        server: McpServer {
            name: SERVER_NAME.into(),
            command: tools.server[0].clone(),
            args: tools.server[1..]
                .iter()
                .cloned()
                .chain(["--root".into(), root, "--branch".into(), name.clone()])
                .collect(),
            env: vec![(ENV_TOKEN.into(), token.clone())],
        },
        env,
        instructions: Instructions {
            text: skill_text().to_owned(),
            plugin_dir: Some(plugin.display().to_string()),
        },
        cost,
    };
    let file = TokenFile {
        branch: name.clone(),
        token,
        broker: socket,
        pid: std::process::id(),
    };
    write_private(
        &store.token_path(&name),
        &serde_json::to_vec_pretty(&file).expect("a token file serializes"),
    )
    .map_err(|e| Error::State(format!("delegation token for {name}: {e}")))?;
    Ok(Some(projection))
}

/// `dir` first, then the inherited value of a `PATH`-like variable.
fn prepend(dir: &Path, inherited: Option<std::ffi::OsString>) -> String {
    let dir = dir.display().to_string();
    match inherited.map(|v| v.to_string_lossy().into_owned()) {
        Some(rest) if !rest.is_empty() => format!("{dir}:{rest}"),
        _ => dir,
    }
}

/// Write `content` to `path` unless it is already there, through a
/// temporary file and a rename, so concurrent turns never see half a file.
fn install(path: &Path, content: &str) -> Result<(), Error> {
    if fs::read_to_string(path).is_ok_and(|current| current == content) {
        return Ok(());
    }
    let failed = |e: std::io::Error| Error::State(format!("install {}: {e}", path.display()));
    let dir = path.parent().expect("installed files have a directory");
    fs::create_dir_all(dir).map_err(failed)?;
    let temp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        new_token()?
    ));
    fs::write(&temp, content)
        .and_then(|()| fs::rename(&temp, path))
        .map_err(|e| {
            let _ = fs::remove_file(&temp);
            failed(e)
        })
}

/// Write `bytes` to `path`, readable only by this user.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let _ = fs::remove_file(path);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.write_all(b"\n")
}

/// 256 random bits, hex encoded.
pub(crate) fn new_token() -> Result<String, Error> {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|e| Error::State(format!("could not read /dev/urandom for a token: {e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Compare without stopping at the first difference.
pub(crate) fn same_token(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// The token file in `root` holding `token`, if any. Every file is read and
/// compared in full.
pub(crate) fn find_token(root: &Path, token: &str) -> Option<TokenFile> {
    let dir = crate::state::Store::new(root).dir().join("delegation");
    let mut found = None;
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(file) = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<TokenFile>(&text).ok())
        else {
            continue;
        };
        if same_token(&file.token, token) {
            found = Some(file);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_random_and_compared_in_full() {
        let a = new_token().unwrap();
        let b = new_token().unwrap();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert!(same_token(&a, &a.clone()));
        assert!(!same_token(&a, &b));
        assert!(!same_token(&a, &a[..63]));
    }

    #[test]
    fn variable_names_survive_codexs_default_shell_filter() {
        for name in [ENV_TOKEN, ENV_BRANCH, ENV_ROOT, ENV_BY] {
            for word in ["KEY", "SECRET", "TOKEN"] {
                assert!(!name.contains(word), "{name}");
            }
        }
    }

    #[test]
    fn the_skill_has_frontmatter_and_the_text_drops_it() {
        assert!(SKILL.starts_with("---\nname: delegate\ndescription: "));
        let text = skill_text();
        assert!(!text.starts_with("---"));
        assert!(text.contains("by spawn"));
        let manifest: serde_json::Value = serde_json::from_str(PLUGIN_MANIFEST).unwrap();
        assert_eq!(manifest["name"], "branchyard");
    }

    #[test]
    fn prepended_paths_keep_what_was_there() {
        assert_eq!(
            prepend(Path::new("/by"), Some("/usr/bin".into())),
            "/by:/usr/bin"
        );
        assert_eq!(prepend(Path::new("/by"), None), "/by");
        assert_eq!(prepend(Path::new("/by"), Some("".into())), "/by");
    }
}
