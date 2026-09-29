//! The I/O under `by init` and `by config`: where the configuration files
//! are, the real [`Probe`] of this machine, the validators that load each
//! generated file with the loader that will read it, and writing a plan.
//!
//! The probe reports whether a variable is set, never its value. The only
//! file contents it hashes are token files `by init` itself generates.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use branchyard_setup::config::{self, ConfigError, Effective, ProjectConfig, Source};
use branchyard_setup::plan::{
    ArtifactKind, BuiltinValidator, FileAction, Plan, PlannedFile, Validation, Validator,
};
use branchyard_setup::probe::{Entropy, GitFact, HarnessFact, Platform, Probe};
use branchyard_setup::protocol::Applied;

/// The directory holding `.git` at or above `cwd`, else `cwd`.
pub fn project_root(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(cwd)
        .to_path_buf()
}

/// `branchyard.toml` at or above `cwd`, up to the repository root.
pub fn project_file(cwd: &Path) -> Option<PathBuf> {
    for dir in cwd.ancestors() {
        let candidate = dir.join(config::PROJECT_FILE);
        if candidate.is_file() {
            return Some(candidate);
        }
        if dir.join(".git").exists() {
            break;
        }
    }
    None
}

pub fn home() -> Option<PathBuf> {
    dirs::home_dir()
}

/// The user configuration: `BRANCHYARD_USER_CONFIG` when set, else
/// `$XDG_CONFIG_HOME/branchyard/config.toml`, else
/// `~/.config/branchyard/config.toml` (on every Unix, macOS included).
pub fn user_file() -> PathBuf {
    if let Some(path) = std::env::var_os("BRANCHYARD_USER_CONFIG").filter(|v| !v.is_empty()) {
        return PathBuf::from(path);
    }
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None if cfg!(unix) => home().unwrap_or_default().join(".config"),
        None => dirs::config_dir().unwrap_or_default(),
    };
    base.join("branchyard").join("config.toml")
}

/// A configuration file, parsed strictly, its paths made absolute.
pub fn read_layer(path: &Path) -> Result<ProjectConfig, ConfigError> {
    let text =
        fs::read_to_string(path).map_err(|e| ConfigError(format!("{}: {e}", path.display())))?;
    let mut parsed =
        config::parse(&text).map_err(|e| ConfigError(format!("{}: {e}", path.display())))?;
    parsed
        .check_layer(config::Layer::of(path))
        .map_err(|e| ConfigError(format!("{}: {e}", path.display())))?;
    parsed.resolve_paths(path.parent().unwrap_or(Path::new(".")), home().as_deref());
    Ok(parsed)
}

/// Where the files are, whether they exist.
pub struct Located {
    pub user: PathBuf,
    /// The project file found, or where `by init project` would write it.
    pub project: PathBuf,
    pub project_exists: bool,
}

pub fn locate(cwd: &Path) -> Located {
    let found = project_file(cwd);
    Located {
        user: user_file(),
        project_exists: found.is_some(),
        project: found.unwrap_or_else(|| project_root(cwd).join(config::PROJECT_FILE)),
    }
}

/// Reads a variable by name.
pub type EnvReader = dyn Fn(&str) -> Option<String>;

/// The user and project files merged, with `env` for the `BRANCHYARD_*`
/// variables; `None` for `env` leaves them out.
pub fn load(cwd: &Path, env: Option<&EnvReader>) -> Result<Effective, ConfigError> {
    let located = locate(cwd);
    let mut layers = Vec::new();
    if located.user.is_file() {
        layers.push((
            Source::User {
                path: located.user.display().to_string(),
            },
            read_layer(&located.user)?,
        ));
    }
    if located.project_exists {
        layers.push((
            Source::Project {
                path: located.project.display().to_string(),
            },
            read_layer(&located.project)?,
        ));
    }
    match env {
        Some(env) => config::merge(&layers, env),
        None => config::merge(&layers, |_| None),
    }
}

/// This machine and repository.
pub struct HostProbe {
    root: PathBuf,
}

impl HostProbe {
    pub fn new(cwd: &Path) -> HostProbe {
        HostProbe {
            root: project_root(cwd),
        }
    }

    pub fn root_path(&self) -> &Path {
        &self.root
    }

    /// A plan's path, as a path on this machine.
    pub fn resolve(&self, path: &str) -> PathBuf {
        resolve(&self.root, path)
    }
}

/// `path` relative to `root`, with `~/` for the home directory.
pub fn resolve(root: &Path, path: &str) -> PathBuf {
    if let (Some(rest), Some(home)) = (path.strip_prefix("~/"), home()) {
        return home.join(rest);
    }
    let path = Path::new(path);
    match path.is_absolute() {
        true => path.to_path_buf(),
        false => root.join(path),
    }
}

/// Find `program` on `PATH`.
fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(candidate)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// The first line a command prints, or `None` if it fails or takes longer
/// than `timeout`. Its input is closed; nothing else is passed.
fn first_line(
    program: &Path,
    args: &[&str],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Option<String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn().ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return status
                    .success()
                    .then(|| out.lines().next().unwrap_or_default().trim().to_owned());
            }
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

const VERSION_TIMEOUT: Duration = Duration::from_secs(3);

/// `git version 2.45.2` as `2.45.2`, `Docker version 27.1.1, build x` as `27.1.1`.
fn short_version(line: &str) -> String {
    line.split_whitespace()
        .find(|word| word.starts_with(|c: char| c.is_ascii_digit()))
        .map(|v| v.trim_end_matches(',').to_owned())
        .unwrap_or_else(|| line.to_owned())
}

impl Probe for HostProbe {
    fn root(&self) -> String {
        self.root.display().to_string()
    }

    fn home(&self) -> Option<String> {
        home().map(|h| h.display().to_string())
    }

    fn git(&self) -> Option<GitFact> {
        if !self.root.join(".git").exists() {
            return None;
        }
        let git = which("git")?;
        let origin = first_line(
            &git,
            &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
            Some(&self.root),
            VERSION_TIMEOUT,
        )
        .map(|b| b.trim_start_matches("origin/").to_owned());
        let default_branch = origin
            .or_else(|| {
                first_line(
                    &git,
                    &["branch", "--show-current"],
                    Some(&self.root),
                    VERSION_TIMEOUT,
                )
            })
            .filter(|b| !b.is_empty());
        Some(GitFact {
            root: self.root(),
            default_branch,
        })
    }

    fn harnesses(&self) -> Vec<HarnessFact> {
        let defaults: Vec<branchyard::HarnessInfo> = branchyard::harnesses()
            .into_iter()
            .filter(|h| h.default)
            .collect();
        // Ask each installed harness for its version at once, bounded.
        let handles: Vec<_> = defaults
            .into_iter()
            .map(|info| {
                std::thread::spawn(move || {
                    let program = branchyard_harness::profiles::by_id(&info.profile)
                        .and_then(|p| p.command.first())
                        .and_then(|program| which(program));
                    let version = match (info.available, program) {
                        (true, Some(program)) => {
                            first_line(&program, &["--version"], None, VERSION_TIMEOUT)
                        }
                        _ => None,
                    };
                    HarnessFact {
                        harness: info.harness,
                        profile: info.profile,
                        installed: info.available,
                        version: version.map(|v| short_version(&v)),
                        qualification: info.qualification,
                    }
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    }

    fn env_is_set(&self, name: &str) -> bool {
        std::env::var_os(name).is_some_and(|v| !v.is_empty())
    }

    fn tool(&self, program: &str) -> Option<String> {
        let path = which(program)?;
        Some(
            first_line(&path, &["--version"], None, VERSION_TIMEOUT)
                .map(|v| short_version(&v))
                .unwrap_or_default(),
        )
    }

    fn platform(&self) -> Platform {
        Platform {
            os: std::env::consts::OS.into(),
            kvm: fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/kvm")
                .is_ok(),
        }
    }

    fn read(&self, path: &str) -> Option<String> {
        fs::read_to_string(self.resolve(path)).ok()
    }

    fn exists(&self, path: &str) -> bool {
        self.resolve(path).exists()
    }

    fn token_sha256(&self, path: &str) -> Option<String> {
        let text = fs::read_to_string(self.resolve(path)).ok()?;
        let token = text.lines().next().unwrap_or_default().trim().to_owned();
        (!token.is_empty()).then(|| branchyard_setup::sha256_hex(token.as_bytes()))
    }

    fn user_config_path(&self) -> String {
        user_file().display().to_string()
    }
}

/// Tokens as `branchyard-server token new` makes them.
pub struct RandomEntropy;

impl Entropy for RandomEntropy {
    fn token(&mut self) -> String {
        branchyard_server::cli::new_token()
    }
}

/// A private scratch directory holding a plan's files at their relative
/// paths, so a loader resolves references between them as it will after
/// applying. Removed on drop.
struct Staging {
    dir: PathBuf,
}

impl Staging {
    fn new(files: &[PlannedFile]) -> std::io::Result<Staging> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("by-init-{}-{nanos}", std::process::id()));
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)?;
        }
        let staging = Staging { dir };
        for file in files {
            let body = match (file.sensitive, file.action) {
                // A kept secret is not read: a stand-in of a valid length.
                (true, FileAction::Keep) => "kept-secret-stand-in-0123456789\n".to_owned(),
                _ => file.body().to_owned(),
            };
            let path = staging.path(&file.path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            write_file(&path, &body, if file.sensitive { 0o600 } else { 0o644 })?;
        }
        Ok(staging)
    }

    fn path(&self, path: &str) -> PathBuf {
        self.dir
            .join(path.trim_start_matches("~/").trim_start_matches('/'))
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The validators `by` adds to the engine's own: the server's loader,
/// `by rig check`'s planner, and `docker compose config` when docker is
/// installed.
pub struct HostValidator;

impl Validator for HostValidator {
    fn validate(&self, file: &PlannedFile, all: &[PlannedFile]) -> Validation {
        match file.kind {
            ArtifactKind::ServerConfig => {
                const NAME: &str = "branchyard-server config (by serve --check)";
                let staging = match Staging::new(all) {
                    Ok(staging) => staging,
                    Err(e) => return Validation::failed(NAME, format!("staging the plan: {e}")),
                };
                let mut args = vec![
                    "--config".to_owned(),
                    staging.path(&file.path).display().to_string(),
                ];
                args.extend(file.serve_flags.iter().cloned());
                match branchyard_server::cli::check(&args) {
                    // Warnings about files the plan does not create (TLS
                    // files, repositories) describe the staging directory,
                    // not this machine; only the insecure-bind one is kept.
                    Ok(warnings) => Validation {
                        messages: warnings
                            .into_iter()
                            .filter(|w| w.starts_with("WARNING"))
                            .collect(),
                        ..Validation::ok(NAME)
                    },
                    Err(e) => {
                        Validation::failed(NAME, e.replace(&staging.dir.display().to_string(), ""))
                    }
                }
            }
            ArtifactKind::Rig => {
                const NAME: &str = "rig planner (by rig check)";
                match crate::rig::parse(file.body()).and_then(|spec| crate::rig::plan(&spec)) {
                    Ok(_) => Validation::ok(NAME),
                    Err(e) => Validation::failed(NAME, e.to_string()),
                }
            }
            ArtifactKind::Compose => {
                const NAME: &str = "docker compose config";
                let Some(docker) = which("docker") else {
                    return Validation::skipped(NAME, "docker is not installed here; not checked");
                };
                let staging = match Staging::new(all) {
                    Ok(staging) => staging,
                    Err(e) => return Validation::failed(NAME, format!("staging the plan: {e}")),
                };
                let path = staging.path(&file.path).display().to_string();
                match first_line(
                    &docker,
                    &["compose", "-f", &path, "config", "-q"],
                    None,
                    Duration::from_secs(20),
                ) {
                    Some(_) => Validation::ok(NAME),
                    None => Validation::skipped(
                        NAME,
                        "docker compose config failed or is unavailable; not checked",
                    ),
                }
            }
            _ => BuiltinValidator.validate(file, all),
        }
    }
}

/// Write `body` to `path` with `mode` through a temporary file and a
/// rename, so a reader never sees half a file.
pub fn write_file(path: &Path, body: &str, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = path.with_file_name(format!(".{name}.by-init-{}", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temp)?;
    let result = file
        .write_all(body.as_bytes())
        .and_then(|_| file.sync_all())
        .and_then(|_| fs::set_permissions(&temp, fs::Permissions::from_mode(mode)))
        .and_then(|_| fs::rename(&temp, path));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Why a plan was not applied.
pub enum Refusal {
    Invalid(Vec<String>),
    WouldOverwrite(Vec<String>),
    Io(String),
}

/// Write `plan` under `root`: every file it creates or updates. Refuses an
/// invalid plan, and a plan that replaces a file with different text
/// unless `force`. A secret file is created exclusively and never
/// replaced.
pub fn apply(plan: &Plan, root: &Path, force: bool) -> Result<Applied, Refusal> {
    if !plan.valid {
        return Err(Refusal::Invalid(
            plan.files
                .iter()
                .filter(|f| !f.validation.ok)
                .map(|f| format!("{}: {}", f.path, f.validation.messages.join("; ")))
                .collect(),
        ));
    }
    let overwrites: Vec<String> = plan.overwrites().iter().map(|f| f.path.clone()).collect();
    if !overwrites.is_empty() && !force {
        return Err(Refusal::WouldOverwrite(overwrites));
    }
    let mut applied = Applied {
        protocol: branchyard_setup::PROTOCOL.into(),
        topic: plan.topic,
        written: Vec::new(),
        unchanged: Vec::new(),
        commands: plan.commands.clone(),
    };
    for file in &plan.files {
        if !file.writes() {
            applied.unchanged.push(file.path.clone());
            continue;
        }
        let path = resolve(root, &file.path);
        let io = |e: std::io::Error| Refusal::Io(format!("{}: {e}", path.display()));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(io)?;
        }
        if file.sensitive && path.exists() {
            // Appeared since the plan was made: keep it.
            applied.unchanged.push(file.path.clone());
            continue;
        }
        write_file(&path, file.body(), file.mode_bits()).map_err(io)?;
        applied.written.push(file.path.clone());
    }
    Ok(applied)
}

/// A probe that asks the machine once: the wizard steps the engine after
/// every batch, and asking each harness for its version is slow.
pub struct CachedProbe<P: Probe> {
    inner: P,
    git: std::sync::OnceLock<Option<GitFact>>,
    harnesses: std::sync::OnceLock<Vec<HarnessFact>>,
    platform: std::sync::OnceLock<Platform>,
    tools: std::sync::Mutex<std::collections::BTreeMap<String, Option<String>>>,
}

impl<P: Probe> CachedProbe<P> {
    pub fn new(inner: P) -> CachedProbe<P> {
        CachedProbe {
            inner,
            git: Default::default(),
            harnesses: Default::default(),
            platform: Default::default(),
            tools: Default::default(),
        }
    }
}

impl<P: Probe> Probe for CachedProbe<P> {
    fn root(&self) -> String {
        self.inner.root()
    }
    fn home(&self) -> Option<String> {
        self.inner.home()
    }
    fn git(&self) -> Option<GitFact> {
        self.git.get_or_init(|| self.inner.git()).clone()
    }
    fn harnesses(&self) -> Vec<HarnessFact> {
        self.harnesses
            .get_or_init(|| self.inner.harnesses())
            .clone()
    }
    fn env_is_set(&self, name: &str) -> bool {
        self.inner.env_is_set(name)
    }
    fn tool(&self, program: &str) -> Option<String> {
        let mut tools = self.tools.lock().unwrap_or_else(|p| p.into_inner());
        tools
            .entry(program.to_owned())
            .or_insert_with(|| self.inner.tool(program))
            .clone()
    }
    fn platform(&self) -> Platform {
        self.platform.get_or_init(|| self.inner.platform()).clone()
    }
    fn read(&self, path: &str) -> Option<String> {
        self.inner.read(path)
    }
    fn exists(&self, path: &str) -> bool {
        self.inner.exists(path)
    }
    fn token_sha256(&self, path: &str) -> Option<String> {
        self.inner.token_sha256(path)
    }
    fn user_config_path(&self) -> String {
        self.inner.user_config_path()
    }
}
