// Derived from stablyai/orca src/shared/ephemeral-vm-recipes.ts,
// src/shared/ephemeral-vm-recipe-runner.ts,
// src/shared/ephemeral-vm-recipe-process.ts,
// src/shared/ephemeral-vm-recipe-lifecycle-payload.ts,
// src/shared/ephemeral-vm-recipe-destroy-result.ts,
// src/shared/ephemeral-vm-recipe-doctor.ts and the recipe entries of
// src/shared/orca-yaml.ts and src/shared/orca-yaml-hook-types.ts, at revision
// 280733273545f0b3eeedc1be54b14d406239030e.
// Copyright (c) 2026 Lovecast Inc. Licensed under the MIT License; the
// license text, which must accompany substantial portions of this code, is
// in vendor/orca/LICENSE.
// Modified for Branchyard: translated from TypeScript to Rust. The recipe
// contract is Orca's: `create`, `suspend`, `resume` and `destroy` shell
// commands run in the repository root (`destroy = "none"` disables it); each
// prints one JSON object on stdout (at most 1 MiB kept, as Orca keeps), and
// suspend, resume and destroy receive the lifecycle payload on stdin; the
// process group is killed on cancellation; the doctor's static checks
// (repository-relative command paths, the exec bit, destroy configured, and
// suspend and resume paired) are Orca's. Branchyard's changes: recipes come
// from `[recipes.NAME]` in branchyard.toml and are trust-gated like
// `[workspace]`; the variables are `BRANCHYARD_RECIPE*` instead of
// `ORCA_*`; a recipe may also declare a `doctor` command; the result's
// connection is `ssh` (Orca's target, without its relay and port-forward
// fields) or `exec` (an argument vector prefix, such as `docker exec -i
// NAME`), it may carry `env` for every exec, and Orca's `orca-server`
// pairing connection and `provisioned-root` checkout mode are left out;
// scripts have a timeout.

//! Environment recipes: repository scripts that create, suspend, resume and
//! destroy a VM (or any machine reachable over ssh or an exec command), and
//! [`provider::RecipeProvider`], a [`branchyard_sandbox::SandboxProvider`]
//! over them. See docs/recipes.md.
//!
//! A recipe's `create` prints one JSON object (a [`RecipeResult`]):
//!
//! ```json
//! {"schemaVersion": 1,
//!  "connection": {"type": "ssh",
//!                 "target": {"host": "10.0.0.7", "port": 22, "username": "dev"},
//!                 "projectRoot": "/home/dev/app"},
//!  "env": {"CI": "1"}}
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub mod provider;
mod transport;

pub use provider::RecipeProvider;
pub use transport::{quote, Transport};

/// Output kept per stream, as Orca keeps: the last 1 MiB.
pub const MAX_CAPTURE_BYTES: usize = 1024 * 1024;
/// How long a recipe script may run by default.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// One `[recipes.NAME]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recipe {
    pub name: String,
    /// Prints the [`RecipeResult`] of a new machine.
    pub create: String,
    /// Freezes it; given the lifecycle payload on stdin.
    pub suspend: Option<String>,
    /// Continues it and prints its (possibly new) [`RecipeResult`].
    pub resume: Option<String>,
    /// Releases it; given the lifecycle payload on stdin. `None` when
    /// absent or `"none"` (resources are cleaned up elsewhere).
    pub destroy: Option<String>,
    /// Whether `destroy = "none"` disabled it on purpose.
    pub destroy_disabled: bool,
    /// Checks the host can run the recipe (credentials, CLIs); exit 0 is
    /// healthy. Optional.
    pub doctor: Option<String>,
    /// The repository root the scripts run in.
    pub root: PathBuf,
    pub timeout: Duration,
}

impl Recipe {
    /// A recipe named `name` whose scripts run in `root`. `destroy` of
    /// `"none"` disables destroy, as in Orca.
    pub fn new(
        name: impl Into<String>,
        root: impl Into<PathBuf>,
        create: impl Into<String>,
    ) -> Recipe {
        Recipe {
            name: name.into(),
            create: create.into(),
            suspend: None,
            resume: None,
            destroy: None,
            destroy_disabled: false,
            doctor: None,
            root: root.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Set `destroy` as written in the configuration.
    pub fn with_destroy(mut self, destroy: Option<&str>) -> Recipe {
        match destroy.map(str::trim) {
            Some("none") => {
                self.destroy = None;
                self.destroy_disabled = true;
            }
            Some("") | None => self.destroy = None,
            Some(command) => self.destroy = Some(command.to_owned()),
        }
        self
    }

    /// Whether suspend and resume are both declared: the provider's pause.
    pub fn can_pause(&self) -> bool {
        self.suspend.is_some() && self.resume.is_some()
    }
}

/// Which script runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Create,
    Suspend,
    Resume,
    Destroy,
    Doctor,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mode::Create => "create",
            Mode::Suspend => "suspend",
            Mode::Resume => "resume",
            Mode::Destroy => "destroy",
            Mode::Doctor => "doctor",
        })
    }
}

/// An ssh destination, as Orca's `EphemeralVmRecipeSshTarget` (without its
/// relay and port-forward fields).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SshTarget {
    /// For messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// A `Host` of the user's ssh configuration; used instead of host,
    /// port and username when given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_host: Option<String>,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identities_only: Option<bool>,
}

fn default_port() -> u16 {
    22
}

/// How execs reach the machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Connection {
    /// Over `ssh`.
    Ssh {
        target: SshTarget,
        #[serde(rename = "projectRoot")]
        project_root: String,
    },
    /// By running `argv` followed by `sh -c SCRIPT`, such as
    /// `["docker", "exec", "-i", "vm-1"]`.
    Exec {
        argv: Vec<String>,
        #[serde(rename = "projectRoot")]
        project_root: String,
    },
}

impl Connection {
    /// The working directory on the machine.
    pub fn project_root(&self) -> &str {
        match self {
            Connection::Ssh { project_root, .. } | Connection::Exec { project_root, .. } => {
                project_root
            }
        }
    }
}

/// What `create` and `resume` print.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RecipeResult {
    pub schema_version: u32,
    pub connection: Connection,
    /// Variables every exec on the machine gets.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Anything the recipe wants back in the lifecycle payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_data: Option<serde_json::Value>,
}

/// Parse a script's stdout as Orca's `parseEphemeralVmRecipeResult` does:
/// one JSON object, of a known schema version, whose project root is
/// absolute.
pub fn parse_result(stdout: &str) -> Result<RecipeResult, String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err("the recipe produced no JSON result".into());
    }
    let value: serde_json::Value = serde_json::from_str(trimmed)
        .map_err(|_| "the recipe's stdout must be one JSON object".to_owned())?;
    if !value.is_object() {
        return Err("the recipe's stdout must be one JSON object".into());
    }
    let result: RecipeResult =
        serde_json::from_value(value).map_err(|e| format!("invalid recipe result: {e}"))?;
    if result.schema_version != 1 {
        return Err(format!(
            "schemaVersion {} is not 1, the version this build reads",
            result.schema_version
        ));
    }
    if !result.connection.project_root().starts_with('/') {
        return Err("projectRoot must be an absolute path on the machine".into());
    }
    match &result.connection {
        Connection::Ssh { target, .. } => {
            let host = target.config_host.as_deref().unwrap_or(&target.host);
            if host.is_empty() || host.starts_with('-') || host.contains(char::is_whitespace) {
                return Err(format!("unusable ssh host {host:?}"));
            }
            if target.port == 0 {
                return Err("the ssh port must be from 1 to 65535".into());
            }
            if target.username.starts_with('-') || target.username.contains(char::is_whitespace) {
                return Err(format!("unusable ssh username {:?}", target.username));
            }
        }
        Connection::Exec { argv, .. } => {
            if argv.first().is_none_or(|p| p.is_empty()) {
                return Err("an exec connection needs argv".into());
            }
        }
    }
    for name in result.env.keys() {
        check_variable(name)?;
    }
    Ok(result)
}

/// A POSIX variable name.
pub fn check_variable(name: &str) -> Result<(), String> {
    let ok = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    match ok {
        true => Ok(()),
        false => Err(format!("{name:?} is not a variable name")),
    }
}

/// What suspend, resume and destroy receive on stdin: Orca's lifecycle
/// payload.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Payload<'a> {
    pub schema_version: u32,
    pub mode: Mode,
    pub recipe: &'a str,
    pub instance: &'a str,
    pub recipe_result: &'a RecipeResult,
}

/// How a script ended.
#[derive(Debug)]
pub struct Ran {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

impl Ran {
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.timed_out
    }

    /// Why it failed, in one line with the end of its stderr.
    pub fn failure(&self, recipe: &str, mode: Mode) -> String {
        let why = match (self.timed_out, self.code) {
            (true, _) => "timed out".to_owned(),
            (false, Some(code)) => format!("exited with code {code}"),
            (false, None) => "was killed by a signal".to_owned(),
        };
        let tail: Vec<&str> = self.stderr.trim().lines().rev().take(5).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        match tail.is_empty() {
            true => format!("recipe {recipe}: {mode} {why}"),
            false => format!("recipe {recipe}: {mode} {why}: {}", tail.join(" / ")),
        }
    }
}

/// Run one of `recipe`'s scripts with `sh -c` in its root, as Orca's
/// `runRecipeCommand` does: in its own process group (killed whole on a
/// timeout), with `stdin` written and closed, the variables
/// `BRANCHYARD_RECIPE`, `BRANCHYARD_RECIPE_MODE`,
/// `BRANCHYARD_RECIPE_INSTANCE`, `BRANCHYARD_ROOT` and
/// `BRANCHYARD_RECIPE_RESULT_SCHEMA_VERSION` added to this process's, and
/// the last [`MAX_CAPTURE_BYTES`] of each stream kept.
pub fn run(
    recipe: &Recipe,
    command: &str,
    mode: Mode,
    instance: &str,
    stdin: Option<&[u8]>,
) -> std::io::Result<Ran> {
    use std::os::unix::process::CommandExt;
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(&recipe.root)
        .env("BRANCHYARD_RECIPE", &recipe.name)
        .env("BRANCHYARD_RECIPE_MODE", mode.to_string())
        .env("BRANCHYARD_RECIPE_INSTANCE", instance)
        .env("BRANCHYARD_ROOT", &recipe.root)
        .env("BRANCHYARD_RECIPE_RESULT_SCHEMA_VERSION", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;
    let input = stdin.map(<[u8]>::to_vec).unwrap_or_default();
    let mut pipe = child.stdin.take();
    let writer = thread::spawn(move || {
        if let Some(pipe) = pipe.as_mut() {
            let _ = pipe.write_all(&input);
        }
    });
    let out = tail_reader(child.stdout.take());
    let err = tail_reader(child.stderr.take());
    // Done when the script has exited and its output has closed, as Orca
    // waits for `close`: a machine the script leaves running must not hold
    // its stdout or stderr. At the deadline its whole process group goes.
    let deadline = Instant::now() + recipe.timeout;
    let mut timed_out = false;
    let mut exited = None;
    let status = loop {
        if exited.is_none() {
            exited = child.try_wait()?;
        }
        if let Some(status) = exited {
            if out.is_finished() && err.is_finished() {
                break status;
            }
        }
        if Instant::now() >= deadline {
            timed_out = true;
            kill_group(child.id());
            break match exited {
                Some(status) => status,
                None => child.wait()?,
            };
        }
        thread::sleep(Duration::from_millis(20));
    };
    // After a timeout, a process that left the group may still hold a
    // pipe: give the readers a moment, then leave them.
    let settle = Instant::now() + Duration::from_secs(2);
    while !(out.is_finished() && err.is_finished()) && Instant::now() < settle {
        thread::sleep(Duration::from_millis(20));
    }
    let joined = |reader: thread::JoinHandle<String>| match reader.is_finished() {
        true => reader.join().unwrap_or_default(),
        false => String::new(),
    };
    if writer.is_finished() {
        let _ = writer.join();
    }
    Ok(Ran {
        code: status.code(),
        stdout: joined(out),
        stderr: joined(err),
        timed_out,
    })
}

fn kill_group(pid: u32) {
    if let Some(pgid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process_group(pgid, rustix::process::Signal::KILL);
    }
}

fn tail_reader(pipe: Option<impl Read + Send + 'static>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let Some(mut pipe) = pipe else {
            return String::new();
        };
        let mut kept: Vec<u8> = Vec::new();
        let mut buffer = [0u8; 8192];
        while let Ok(n) = pipe.read(&mut buffer) {
            if n == 0 {
                break;
            }
            kept.extend_from_slice(&buffer[..n]);
            if kept.len() > MAX_CAPTURE_BYTES {
                kept.drain(..kept.len() - MAX_CAPTURE_BYTES);
            }
        }
        // A cut may fall inside a UTF-8 sequence: drop its continuation
        // bytes, as Orca's `takeRetainedTail` does.
        let start = kept
            .iter()
            .position(|b| b & 0xc0 != 0x80)
            .unwrap_or(kept.len());
        String::from_utf8_lossy(&kept[start..]).into_owned()
    })
}

/// One doctor check, as Orca's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Check {
    pub id: String,
    pub status: Status,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Warn,
    Fail,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Status::Pass => "pass",
            Status::Warn => "warn",
            Status::Fail => "fail",
        })
    }
}

fn check(id: &str, status: Status, message: String, remediation: Option<&str>) -> Check {
    Check {
        id: id.into(),
        status,
        message,
        remediation: remediation.map(str::to_owned),
    }
}

/// The first word of a command, unquoted: Orca's `firstRecipeCommandToken`.
pub fn first_token(command: &str) -> Option<&str> {
    let trimmed = command.trim();
    for quote in ['"', '\''] {
        if let Some(rest) = trimmed.strip_prefix(quote) {
            return rest.split(quote).next().filter(|t| !t.is_empty());
        }
    }
    trimmed.split_whitespace().next()
}

fn command_path(root: &Path, command: &str, id: &str) -> Check {
    use std::os::unix::fs::PermissionsExt;
    let Some(executable) = first_token(command) else {
        return check(
            id,
            Status::Fail,
            "the command is empty".into(),
            Some("set a repository-relative command path"),
        );
    };
    if executable.starts_with('/') {
        return check(
            id,
            Status::Warn,
            format!("the command uses an absolute path: {executable}"),
            Some("prefer a repository-relative script so the recipe works across machines"),
        );
    }
    if !executable.starts_with("./") {
        return check(
            id,
            Status::Warn,
            format!("the command is not a repository-relative path: {executable}"),
            Some("use a repository-relative script such as ./scripts/vm/create.sh"),
        );
    }
    let path = root.join(executable.trim_start_matches("./"));
    let Ok(meta) = std::fs::metadata(&path) else {
        return check(
            id,
            Status::Fail,
            format!("the command path does not exist: {executable}"),
            Some("create the script or fix the recipe's command path"),
        );
    };
    if meta.permissions().mode() & 0o111 == 0 {
        return check(
            id,
            Status::Warn,
            format!("the command exists but is not executable: {executable}"),
            Some("make it executable: chmod +x (git: git update-index --chmod=+x)"),
        );
    }
    check(
        id,
        Status::Pass,
        format!("the command path exists: {executable}"),
        None,
    )
}

/// Orca's static doctor checks of `recipe`: each command's path, destroy
/// configured, suspend and resume paired.
pub fn static_checks(recipe: &Recipe) -> Vec<Check> {
    let mut checks = vec![command_path(&recipe.root, &recipe.create, "recipe.create")];
    if recipe.destroy_disabled {
        checks.push(check(
            "recipe.destroy",
            Status::Warn,
            "destroy is explicitly disabled".into(),
            Some("only use destroy = \"none\" when the machine is cleaned up elsewhere"),
        ));
    } else if let Some(destroy) = &recipe.destroy {
        checks.push(command_path(&recipe.root, destroy, "recipe.destroy"));
    } else {
        checks.push(check(
            "recipe.destroy",
            Status::Warn,
            "no destroy command is configured".into(),
            Some("add destroy, or set destroy = \"none\" explicitly"),
        ));
    }
    for (id, command) in [
        ("recipe.suspend", &recipe.suspend),
        ("recipe.resume", &recipe.resume),
        ("recipe.doctor", &recipe.doctor),
    ] {
        if let Some(command) = command {
            checks.push(command_path(&recipe.root, command, id));
        }
    }
    // A machine suspended by `suspend` can only be woken by `resume`;
    // one without the other strands it asleep.
    if recipe.suspend.is_some() != recipe.resume.is_some() {
        checks.push(check(
            "recipe.suspend_resume_pairing",
            Status::Warn,
            "the recipe defines only one of suspend and resume".into(),
            Some("define both so a suspended machine can be resumed, or neither"),
        ));
    }
    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_parse_as_orca_parses_them() {
        let ok = r#"{"schemaVersion":1,"connection":{"type":"ssh","target":{"host":"10.0.0.7","port":2222,"username":"dev"},"projectRoot":"/home/dev/app"},"env":{"CI":"1"}}"#;
        let result = parse_result(ok).unwrap();
        assert_eq!(result.connection.project_root(), "/home/dev/app");
        assert_eq!(result.env["CI"], "1");
        let exec = r#"{"schemaVersion":1,"connection":{"type":"exec","argv":["docker","exec","-i","vm"],"projectRoot":"/w"}}"#;
        assert!(parse_result(exec).is_ok());
        for (bad, why) in [
            ("", "no JSON"),
            ("not json", "one JSON object"),
            ("[1]", "one JSON object"),
            (
                r#"{"schemaVersion":2,"connection":{"type":"exec","argv":["x"],"projectRoot":"/w"}}"#,
                "schemaVersion 2",
            ),
            (
                r#"{"schemaVersion":1,"connection":{"type":"exec","argv":["x"],"projectRoot":"w"}}"#,
                "absolute",
            ),
            (
                r#"{"schemaVersion":1,"connection":{"type":"exec","argv":[],"projectRoot":"/w"}}"#,
                "needs argv",
            ),
            (
                r#"{"schemaVersion":1,"connection":{"type":"ssh","target":{"host":"-oProxyCommand=x"},"projectRoot":"/w"}}"#,
                "unusable ssh host",
            ),
            (
                r#"{"schemaVersion":1,"connection":{"type":"orca-server","pairingCode":"x","projectRoot":"/w"}}"#,
                "invalid recipe result",
            ),
            (
                r#"{"schemaVersion":1,"extra":1,"connection":{"type":"exec","argv":["x"],"projectRoot":"/w"}}"#,
                "invalid recipe result",
            ),
            (
                r#"{"schemaVersion":1,"env":{"A B":"x"},"connection":{"type":"exec","argv":["x"],"projectRoot":"/w"}}"#,
                "not a variable name",
            ),
        ] {
            let error = parse_result(bad).unwrap_err();
            assert!(error.contains(why), "{bad}: {error}");
        }
    }

    #[test]
    fn first_tokens_unquote_as_orca_does() {
        assert_eq!(first_token("  ./a.sh --x"), Some("./a.sh"));
        assert_eq!(first_token("\"./my script.sh\" x"), Some("./my script.sh"));
        assert_eq!(first_token("'./b.sh'"), Some("./b.sh"));
        assert_eq!(first_token("   "), None);
    }

    #[test]
    fn destroy_none_disables_it() {
        let recipe = Recipe::new("vm", "/r", "./c").with_destroy(Some("none"));
        assert!(recipe.destroy_disabled && recipe.destroy.is_none());
        let recipe = Recipe::new("vm", "/r", "./c").with_destroy(Some("./d"));
        assert_eq!(recipe.destroy.as_deref(), Some("./d"));
    }

    #[test]
    fn doctor_checks_paths_bits_and_pairing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("create.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            dir.path().join("create.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::write(dir.path().join("suspend.sh"), "#!/bin/sh\n").unwrap();
        let mut recipe = Recipe::new("vm", dir.path(), "./create.sh");
        recipe.suspend = Some("./suspend.sh".into());
        let checks = static_checks(&recipe);
        let status = |id: &str| checks.iter().find(|c| c.id == id).map(|c| c.status);
        assert_eq!(status("recipe.create"), Some(Status::Pass));
        assert_eq!(status("recipe.destroy"), Some(Status::Warn));
        assert_eq!(
            status("recipe.suspend"),
            Some(Status::Warn),
            "not executable"
        );
        assert_eq!(status("recipe.suspend_resume_pairing"), Some(Status::Warn));
        recipe.create = "./missing.sh".into();
        assert_eq!(static_checks(&recipe)[0].status, Status::Fail);
        recipe.create = "vm-cli up".into();
        assert_eq!(static_checks(&recipe)[0].status, Status::Warn);
    }

    #[test]
    fn scripts_get_the_variables_and_stdin_and_time_out() {
        let dir = tempfile::tempdir().unwrap();
        let mut recipe = Recipe::new("vm", dir.path(), "unused");
        let ran = run(
            &recipe,
            "printf '%s %s %s ' \"$BRANCHYARD_RECIPE\" \"$BRANCHYARD_RECIPE_MODE\" \"$BRANCHYARD_RECIPE_INSTANCE\"; cat",
            Mode::Destroy,
            "box-1",
            Some(b"{\"x\":1}\n"),
        )
        .unwrap();
        assert!(ran.success(), "{ran:?}");
        assert_eq!(ran.stdout, "vm destroy box-1 {\"x\":1}\n");
        recipe.timeout = Duration::from_millis(300);
        let ran = run(
            &recipe,
            "echo started >&2; sleep 30",
            Mode::Create,
            "box-2",
            None,
        )
        .unwrap();
        assert!(ran.timed_out && !ran.success());
        assert!(ran
            .failure("vm", Mode::Create)
            .contains("timed out: started"));
    }
}
