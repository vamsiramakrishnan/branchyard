//! Qualified harness updates: a new release installed beside the current
//! one, checked without a model, and only then the version Branchyard pins
//! (docs/harness-lifecycle.md, "Qualifying an update").
//!
//! This module holds what is not the command line's: the pin and its file
//! (`qualified.toml` beside the user configuration), the side install's
//! command, and the deterministic checks. `by harnesses update ID
//! --qualify` runs them; [`qualified_pin`] is how the toolchain image
//! builder, or anything else, reads the result.
//!
//! The checks start the candidate in a private home with a scrubbed
//! environment, as `branchyard-qualify` does, and reuse the profile's driver
//! for the handshake: no protocol is written twice. They call no model. A
//! pin therefore says "hermetic" unless a recorded run with a model
//! (`--with-model`) stands behind it ([`QualifiedPin::live_record`]).

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use branchyard_controls::catalog;
use branchyard_harness::profiles::{self, Profile, Protocol};
use branchyard_harness::{NativeSession, Open, SessionMode};
use branchyard_runtime::{Environment, Session};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::inventory;

/// The pin file's name, beside the user configuration.
pub const FILE: &str = "qualified.toml";

/// The checks a pin from a run without a model rests on, in the order they
/// run.
pub const HERMETIC_CHECKS: &[&str] = &["install", "version", "handshake", "flags", "plugin"];

/// How long the candidate has to answer `--version` or `--help`.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the candidate has to finish its protocol handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long it has to exit once its input is closed.
const CLOSE_GRACE: Duration = Duration::from_secs(10);

/// One check of a qualification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    /// `install`, `version`, `handshake`, `flags`, `plugin` or `live`.
    pub name: String,
    /// Whether it passed.
    pub passed: bool,
    /// What was seen, or why it failed.
    pub detail: String,
}

impl Check {
    /// A check that passed.
    pub fn pass(name: &str, detail: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            passed: true,
            detail: detail.into(),
        }
    }

    /// A check that failed.
    pub fn fail(name: &str, detail: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            passed: false,
            detail: detail.into(),
        }
    }
}

/// A harness version that passed its qualification: the version Branchyard
/// pins for that harness and the toolchain image builder bakes in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualifiedPin {
    /// The catalog harness ID: `codex`.
    pub id: String,
    /// The release: `0.170.0`.
    pub version: String,
    /// The npm package it was installed from: `@openai/codex`.
    pub package: String,
    /// The default profile whose handshake it passed: `codex-app-server`.
    pub profile: String,
    /// npm's `dist.integrity` of the release (`sha512-...`) as the install
    /// resolved it, when it could be read; `None` when it could not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrity: Option<String>,
    /// When it was qualified, UTC, RFC 3339.
    pub qualified_at: String,
    /// The dated record of a run with a model (`--with-model`), a file in
    /// the format of `docs/qualification/`; `None` for a pin with no such
    /// run behind it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_record: Option<String>,
    /// The checks that passed.
    #[serde(default)]
    pub checks: Vec<Check>,
}

impl QualifiedPin {
    /// `live` when a recorded run with a model stands behind the pin,
    /// otherwise `hermetic`: the deterministic checks only.
    pub fn evidence(&self) -> &'static str {
        match self.live_record {
            Some(_) => "live",
            None => "hermetic",
        }
    }
}

/// The file of `Pins`, as stored.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Pins {
    #[serde(default, rename = "pin")]
    pins: Vec<QualifiedPin>,
}

/// Where the pins are stored: [`FILE`] in the directory of the user
/// configuration (`BRANCHYARD_USER_CONFIG`'s, else `~/.config/branchyard`).
pub fn qualified_file() -> PathBuf {
    branchyard_config::user_file()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
        .join(FILE)
}

/// The pins in `path`, oldest harness first by ID. A missing file is no
/// pins; a file that does not parse is an error naming it.
pub fn read_pins(path: &Path) -> Result<Vec<QualifiedPin>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let mut pins: Pins =
        toml_edit::de::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    pins.pins.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(pins.pins)
}

/// Store `pin` in `path`, replacing the harness's earlier pin; every other
/// pin stays. The file is written beside itself and renamed, so a reader
/// never sees half of it.
pub fn store_pin(path: &Path, pin: &QualifiedPin) -> Result<(), String> {
    let mut pins = read_pins(path)?;
    pins.retain(|p| p.id != pin.id);
    pins.push(pin.clone());
    pins.sort_by(|a, b| a.id.cmp(&b.id));
    let body = toml_edit::ser::to_string_pretty(&Pins { pins })
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    let text = format!(
        "# Written by `by harnesses update ID --qualify`: the harness versions that passed their\n\
         # checks here. `by harnesses update ID` takes the pin; docs/harness-lifecycle.md.\n\n{body}"
    );
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let partial = path.with_extension("toml.partial");
    fs::write(&partial, text).map_err(|e| format!("{}: {e}", partial.display()))?;
    fs::rename(&partial, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// The qualified pin of `harness` (a catalog ID) on this machine: the
/// version that passed `by harnesses update ID --qualify`, with the
/// package, integrity hash and evidence it was qualified with. `None` when
/// the harness has none, or when the pin file is missing or unreadable.
pub fn qualified_pin(id: &str) -> Option<QualifiedPin> {
    read_pins(&qualified_file())
        .ok()?
        .into_iter()
        .find(|p| p.id == id)
}

/// Whether `version` is newer than the version `harness`'s default profile
/// was checked against; a harness whose profile names none has nothing to
/// be newer than.
pub fn newer_than_checked(harness: &str, version: &str) -> bool {
    match inventory::checked_version(harness) {
        Some(checked) => inventory::version_below(&checked, version),
        None => true,
    }
}

/// Why a release cannot be installed beside the current one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The error kind: `unknown_harness`, `unsupported` or `tool_unavailable`.
    pub kind: &'static str,
    /// What cannot be done, and why, in a sentence.
    pub why: String,
}

/// The command that installs `version` of a harness into a private prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SideInstall {
    /// The catalog harness ID.
    pub harness: String,
    /// The npm package, without a version.
    pub package: String,
    /// The release it installs.
    pub version: String,
    /// A shell command: the catalog's `npm install -g PACKAGE`, as
    /// `npm install --prefix PREFIX PACKAGE@VERSION`.
    pub command: String,
    /// The private directory it installs into.
    pub prefix: PathBuf,
}

/// The side install of `version` of `harness` into `prefix`: only for a
/// harness the catalog installs with `npm install -g`. A piped installer
/// (`curl ... | sh`) or a system package manager decides where it writes
/// and replaces the current install, so it is refused.
pub fn side_install(id: &str, version: &str, prefix: &Path) -> Result<SideInstall, Refusal> {
    let entry = catalog::harness(id).ok_or_else(|| Refusal {
        kind: "unknown_harness",
        why: format!("{id} is not in the harness catalog"),
    })?;
    let Some(command) = entry.install.iter().find(|c| inventory::is_npm_global(c)) else {
        let first = entry.install.first();
        let why = match first {
            None => format!("the catalog records no install command for {id}"),
            Some(c) if c.contains('|') => format!(
                "{id} installs with `{c}`, a piped installer: it decides where it writes and \
                 replaces the current install, so it cannot be installed beside it"
            ),
            Some(c) => format!(
                "{id} installs with `{c}`, which is not an `npm install -g` package: only npm \
                 can install into a private prefix (`--prefix`) beside the current version"
            ),
        };
        return Err(Refusal {
            kind: "unsupported",
            why,
        });
    };
    let pinned = inventory::pin(command, version).ok_or_else(|| Refusal {
        kind: "unsupported",
        why: format!("{id}'s install command ({command}) cannot be pinned to a version"),
    })?;
    let package = inventory::package_of(command).ok_or_else(|| Refusal {
        kind: "unsupported",
        why: format!("{id}'s install command ({command}) names no package"),
    })?;
    let prefix_text = branchyard_recipe::quote(&prefix.display().to_string());
    let words: Vec<String> = pinned
        .split_whitespace()
        .map(|w| match w {
            "-g" | "--global" => format!("--prefix {prefix_text}"),
            other => other.to_owned(),
        })
        .collect();
    Ok(SideInstall {
        harness: id.to_owned(),
        package,
        version: version.to_owned(),
        command: words.join(" "),
        prefix: prefix.to_path_buf(),
    })
}

/// The harness's executable inside `prefix` after an npm install there:
/// the first of the catalog's binary names in `node_modules/.bin`.
pub fn installed_binary(id: &str, prefix: &Path) -> Option<PathBuf> {
    let bin = prefix.join("node_modules").join(".bin");
    catalog::harness(id)?
        .binaries
        .iter()
        .map(|name| bin.join(name))
        .find(|path| path.is_file())
}

/// npm's integrity hash of `package` as the install in `prefix` resolved
/// it, from the `package-lock.json` npm wrote there.
pub fn lock_integrity(prefix: &Path, package: &str) -> Option<String> {
    let text = branchyard_support::best_effort(
        "read the install's package-lock.json",
        fs::read_to_string(prefix.join("package-lock.json")),
    )?;
    let lock: Value = serde_json::from_str(&text).ok()?;
    let entry = lock["packages"][format!("node_modules/{package}")]
        .as_object()
        .or_else(|| lock["dependencies"][package].as_object())?;
    entry
        .get("integrity")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// What the candidate is checked against.
#[derive(Clone, Debug)]
pub struct Candidate<'a> {
    /// The catalog harness ID.
    pub harness: &'a str,
    /// The version that must be reported.
    pub version: &'a str,
    /// The executable under test.
    pub bin: &'a Path,
    /// A directory the checks may fill: their private home and workspace.
    pub workdir: &'a Path,
    /// Variables of this process kept for the candidate by name (its key
    /// variables), as `branchyard-qualify --keep-env` does.
    pub keep_env: &'a [String],
}

impl Candidate<'_> {
    fn home(&self) -> PathBuf {
        self.workdir.join("home")
    }

    fn workspace(&self) -> PathBuf {
        self.workdir.join("workspace")
    }

    fn environment(&self) -> Environment {
        self.keep_env
            .iter()
            .fold(Environment::new(self.home()), |env, name| {
                env.keep(name.as_str())
            })
    }

    fn prepare(&self) -> Result<(), String> {
        for dir in [self.home(), self.workspace()] {
            fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        Ok(())
    }
}

/// The default profile whose handshake qualifies `harness`.
pub fn profile_of(harness: &str) -> Option<&'static Profile> {
    profiles::default_for(harness)
}

/// The deterministic checks without a model, after the install: `version`,
/// `handshake` and `flags`. Every check runs, so one report names every
/// failure. The `plugin` check needs the setup crate's manifests and is the
/// command line's to add.
pub fn run_checks(candidate: &Candidate<'_>) -> Vec<Check> {
    let Some(profile) = profile_of(candidate.harness) else {
        return vec![Check::fail(
            "handshake",
            format!(
                "{} has no driver profile, so there is no handshake to check",
                candidate.harness
            ),
        )];
    };
    if let Err(e) = candidate.prepare() {
        return vec![Check::fail("version", e)];
    }
    vec![
        check_version(candidate),
        check_handshake(candidate, profile),
        check_flags(candidate, profile),
    ]
}

/// A command's exit status and output (both streams), killed when it runs
/// past `limit`.
fn run_bounded(
    command: &mut Command,
    output: &Path,
    limit: Duration,
) -> Result<(Option<i32>, String), String> {
    let file = File::create(output).map_err(|e| format!("{}: {e}", output.display()))?;
    let again = file
        .try_clone()
        .map_err(|e| format!("{}: {e}", output.display()))?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(file)
        .stderr(again)
        .spawn()
        .map_err(|e| format!("could not start it: {e}"))?;
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait().map_err(|e| format!("wait: {e}"))? {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => {
                branchyard_support::best_effort("stop the candidate", child.kill());
                branchyard_support::best_effort("reap the candidate", child.wait());
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let text =
        branchyard_support::best_effort("read the candidate's output", fs::read_to_string(output))
            .unwrap_or_default();
    match status {
        Some(status) => Ok((status.code(), text)),
        None => Err(format!("did not finish within {limit:?}")),
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .chars()
        .take(160)
        .collect()
}

/// `<bin> --version` reports exactly the version asked for.
fn check_version(candidate: &Candidate<'_>) -> Check {
    let mut command = branchyard_support::child::command(candidate.bin);
    command.args(inventory::version_command_args(candidate.harness));
    candidate.environment().apply(&mut command);
    command.current_dir(candidate.workspace());
    let ran = run_bounded(
        &mut command,
        &candidate.workdir.join("version.out"),
        COMMAND_TIMEOUT,
    );
    match ran {
        Err(e) => Check::fail("version", format!("the version command {e}")),
        Ok((status, text)) => match inventory::parse_version(&text) {
            Some(reported) if reported == candidate.version => Check::pass(
                "version",
                format!("reports {reported} (`{}`)", first_line(&text)),
            ),
            Some(reported) => Check::fail(
                "version",
                format!(
                    "reports {reported}, not the requested {} (`{}`)",
                    candidate.version,
                    first_line(&text)
                ),
            ),
            None => Check::fail(
                "version",
                format!(
                    "printed no version (exit {}): `{}`",
                    status.map_or("none".to_owned(), |s| s.to_string()),
                    first_line(&text)
                ),
            ),
        },
    }
}

/// What the profile's protocol handshake is called.
fn handshake_name(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Acp => "ACP initialize",
        Protocol::CodexAppServer => "app-server initialize",
        Protocol::ClaudeStreamJson | Protocol::AntigravityStreamJson | Protocol::AmpStreamJson => {
            "stream-json startup"
        }
        Protocol::PiRpc => "RPC startup",
    }
}

/// The default profile's driver, launched from the candidate, reaches
/// `Ready`: the handshake Branchyard does before every run.
fn check_handshake(candidate: &Candidate<'_>, profile: &Profile) -> Check {
    let name = handshake_name(profile.protocol);
    let mut command: Vec<String> = profile.command.iter().map(|s| (*s).to_owned()).collect();
    command[0] = candidate.bin.display().to_string();
    let open = Open::new(
        SessionMode::Fresh,
        candidate.workspace().display().to_string(),
    );
    let started = Instant::now();
    let mut session = match Session::start(
        profile.driver_with(command),
        open,
        &candidate.environment(),
        None,
    ) {
        Ok(session) => session,
        Err(e) => return Check::fail("handshake", format!("{name} ({}) failed: {e}", profile.id)),
    };
    let ready = session.wait_ready(HANDSHAKE_TIMEOUT);
    let stderr = session.stderr_tail();
    match ready {
        Ok(()) => {
            let closed = session.close(CLOSE_GRACE);
            let took = started.elapsed().as_millis();
            match closed {
                Ok(_) => Check::pass(
                    "handshake",
                    format!("{name} succeeded ({}, ready in {took} ms)", profile.id),
                ),
                Err(e) => Check::fail(
                    "handshake",
                    format!("{name} succeeded but the harness did not close cleanly: {e}"),
                ),
            }
        }
        Err(e) => {
            branchyard_support::best_effort("stop the candidate", session.kill());
            let stderr = stderr.trim();
            let tail = match stderr.is_empty() {
                true => String::new(),
                false => format!("; stderr: {}", first_line(stderr)),
            };
            Check::fail(
                "handshake",
                format!("{name} ({}) failed: {e}{tail}", profile.id),
            )
        }
    }
}

/// The subcommand the profile's launch starts and the long flags it passes,
/// over a fresh, resumed and forked session, with and without a model.
fn launch_flags(profile: &Profile, cwd: &str) -> (Vec<String>, Vec<String>) {
    let parent = NativeSession::new("qualification-session");
    let modes = std::iter::once(SessionMode::Fresh)
        .chain(parent.iter().cloned().map(SessionMode::Resume))
        .chain(parent.iter().cloned().map(SessionMode::Fork));
    let mut subcommand: Vec<String> = Vec::new();
    let mut flags: Vec<String> = Vec::new();
    for (index, mode) in modes.enumerate() {
        for model in [Some("model".to_owned()), None] {
            let mut open = Open::new(mode.clone(), cwd);
            open.model = model;
            let Ok(opened) = profile.driver().open(open) else {
                continue;
            };
            let args: Vec<&String> = opened.launch.argv.iter().skip(1).collect();
            if index == 0 && subcommand.is_empty() {
                subcommand = args
                    .iter()
                    .take_while(|a| !a.starts_with('-'))
                    .map(|a| (*a).clone())
                    .collect();
            }
            for arg in args {
                if let Some(flag) = arg.strip_prefix("--") {
                    let name = format!("--{}", flag.split('=').next().unwrap_or_default());
                    if name.len() > 2 && !flags.contains(&name) {
                        flags.push(name);
                    }
                }
            }
            break;
        }
    }
    (subcommand, flags)
}

/// Every long flag the profile's launch passes is still in the candidate's
/// `--help` (of the subcommand the launch starts).
fn check_flags(candidate: &Candidate<'_>, profile: &Profile) -> Check {
    let (subcommand, flags) = launch_flags(profile, &candidate.workspace().display().to_string());
    if flags.is_empty() {
        return Check::pass(
            "flags",
            format!("{} passes no long flags on launch", profile.id),
        );
    }
    let mut command = branchyard_support::child::command(candidate.bin);
    command.args(&subcommand).arg("--help");
    candidate.environment().apply(&mut command);
    command.current_dir(candidate.workspace());
    let asked = format!("`{} --help`", subcommand.join(" ")).replace("` ", "`");
    let help = match run_bounded(
        &mut command,
        &candidate.workdir.join("help.out"),
        COMMAND_TIMEOUT,
    ) {
        Ok((_, text)) if !text.trim().is_empty() => text,
        Ok(_) => {
            return Check::fail(
                "flags",
                format!("{asked} printed nothing, so {flags:?} cannot be checked"),
            )
        }
        Err(e) => return Check::fail("flags", format!("{asked} {e}")),
    };
    let missing: Vec<&String> = flags
        .iter()
        .filter(|flag| !mentions_flag(&help, flag))
        .collect();
    if missing.is_empty() {
        Check::pass(
            "flags",
            format!("{} still listed in {asked}", flags.join(", ")),
        )
    } else {
        Check::fail(
            "flags",
            format!(
                "{} no longer in {asked}",
                missing
                    .iter()
                    .map(|f| f.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    }
}

/// Whether `help` names `flag` as a whole option (`--acp` is not `--acpx`).
fn mentions_flag(help: &str, flag: &str) -> bool {
    help.match_indices(flag).any(|(at, _)| {
        help[at + flag.len()..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    })
}

/// The `live` check: the record `branchyard-qualify` wrote for the
/// candidate shows every scenario passed, for the profile, on this version.
/// `report` is the record's text, in the format of `docs/qualification/`.
pub fn live_check(report: &str, profile: &str, version: &str, record: &Path) -> Check {
    let Ok(value) = serde_json::from_str::<Value>(report) else {
        return Check::fail("live", "the runner's record is not JSON");
    };
    if value["profile"] != profile {
        return Check::fail(
            "live",
            format!(
                "the record is for profile {}, not {profile}",
                value["profile"]
            ),
        );
    }
    let reported = value["harness_version"]
        .as_str()
        .and_then(inventory::parse_version);
    if reported.as_deref() != Some(version) {
        return Check::fail(
            "live",
            format!(
                "the record's harness_version is {}, not {version}",
                value["harness_version"]
            ),
        );
    }
    let scenarios = value["scenarios"].as_array().cloned().unwrap_or_default();
    let failed: Vec<String> = scenarios
        .iter()
        .filter(|s| s["passed"] != true)
        .map(|s| s["name"].as_str().unwrap_or("?").to_owned())
        .collect();
    if scenarios.is_empty() {
        return Check::fail("live", "the record has no scenarios");
    }
    if !failed.is_empty() {
        return Check::fail(
            "live",
            format!(
                "{} of {} scenarios failed ({}); record {}",
                failed.len(),
                scenarios.len(),
                failed.join(", "),
                record.display()
            ),
        );
    }
    Check::pass(
        "live",
        format!(
            "{n} of {n} scenarios passed on {date}; record {path}",
            n = scenarios.len(),
            date = value["date"].as_str().unwrap_or("an unrecorded day"),
            path = record.display()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(id: &str, version: &str) -> QualifiedPin {
        QualifiedPin {
            id: id.into(),
            version: version.into(),
            package: "@scope/pkg".into(),
            profile: "p".into(),
            integrity: Some("sha512-abc".into()),
            qualified_at: "2026-11-05T10:00:00Z".into(),
            live_record: None,
            checks: vec![Check::pass("version", "reports it")],
        }
    }

    #[test]
    fn a_pin_is_stored_replaced_and_read_back_without_touching_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("qualified.toml");
        assert_eq!(read_pins(&file).unwrap(), []);
        store_pin(&file, &pin("codex", "0.170.0")).unwrap();
        store_pin(&file, &pin("claude-code", "2.2.0")).unwrap();
        let mut newer = pin("codex", "0.171.0");
        newer.live_record = Some("/records/codex.json".into());
        store_pin(&file, &newer).unwrap();
        let pins = read_pins(&file).unwrap();
        assert_eq!(
            pins.iter()
                .map(|p| (p.id.as_str(), p.version.as_str(), p.evidence()))
                .collect::<Vec<_>>(),
            [
                ("claude-code", "2.2.0", "hermetic"),
                ("codex", "0.171.0", "live")
            ]
        );
        assert_eq!(pins[1].live_record.as_deref(), Some("/records/codex.json"));
        assert_eq!(pins[0].integrity.as_deref(), Some("sha512-abc"));
        let text = fs::read_to_string(&file).unwrap();
        assert!(
            text.contains("[[pin]]") && text.contains("[[pin.checks]]"),
            "{text}"
        );
        assert!(!dir.path().join("qualified.toml.partial").exists());
    }

    #[test]
    fn a_pin_file_that_does_not_parse_is_an_error_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("qualified.toml");
        fs::write(&file, "[[pin]]\nid = 3\n").unwrap();
        assert!(read_pins(&file).unwrap_err().contains("qualified.toml"));
    }

    #[test]
    fn only_npm_global_installs_can_be_installed_beside_the_current_one() {
        let prefix = Path::new("/cache/codex/0.170.0");
        let side = side_install("codex", "0.170.0", prefix).unwrap();
        assert_eq!(side.package, "@openai/codex");
        assert_eq!(
            side.command,
            "npm install --prefix /cache/codex/0.170.0 @openai/codex@0.170.0"
        );
        // Flags the catalog gives the install stay.
        let pi = side_install("pi", "0.90.0", prefix).unwrap();
        assert_eq!(
            pi.command,
            "npm install --prefix /cache/codex/0.170.0 @earendil-works/pi-coding-agent@0.90.0 \
             --ignore-scripts"
        );
        let piped = side_install("claude-code", "2.2.0", prefix).unwrap_err();
        assert_eq!(piped.kind, "unsupported");
        assert!(piped.why.contains("piped installer"), "{}", piped.why);
        let none = side_install("goose", "1.0.0", prefix).unwrap_err();
        assert!(none.why.contains("piped installer"), "{}", none.why);
        assert_eq!(
            side_install("nope", "1.0.0", prefix).unwrap_err().kind,
            "unknown_harness"
        );
    }

    #[test]
    fn a_flag_is_named_as_a_whole_option() {
        assert!(mentions_flag("  --acp  Start in ACP mode", "--acp"));
        assert!(mentions_flag("--acp", "--acp"));
        assert!(!mentions_flag("--acpx  other", "--acp"));
        assert!(!mentions_flag("--acp-mode", "--acp"));
    }

    #[test]
    fn the_integrity_is_read_from_the_lockfile_npm_wrote() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("package-lock.json"),
            r#"{"packages":{"":{},"node_modules/@scope/pkg":{"version":"1.0.0","integrity":"sha512-xyz"}}}"#,
        )
        .unwrap();
        assert_eq!(
            lock_integrity(dir.path(), "@scope/pkg").as_deref(),
            Some("sha512-xyz")
        );
        assert_eq!(lock_integrity(dir.path(), "other"), None);
        assert_eq!(lock_integrity(&dir.path().join("missing"), "x"), None);
    }

    #[test]
    fn a_live_record_counts_only_when_every_scenario_passed_on_this_version() {
        let record = |passed: bool, version: &str, profile: &str| {
            serde_json::json!({
                "profile": profile, "harness_version": version, "date": "2026-11-05",
                "scenarios": [
                    {"name": "fresh_turn", "passed": true},
                    {"name": "resume", "passed": passed},
                ],
            })
            .to_string()
        };
        let path = Path::new("/records/r.json");
        let ok = live_check(
            &record(true, "codex-cli 0.170.0", "codex-app-server"),
            "codex-app-server",
            "0.170.0",
            path,
        );
        assert!(ok.passed, "{ok:?}");
        assert!(ok.detail.contains("2 of 2") && ok.detail.contains("2026-11-05"));
        let failed = live_check(
            &record(false, "codex-cli 0.170.0", "codex-app-server"),
            "codex-app-server",
            "0.170.0",
            path,
        );
        assert!(
            !failed.passed && failed.detail.contains("resume"),
            "{failed:?}"
        );
        let other = live_check(
            &record(true, "codex-cli 0.169.0", "codex-app-server"),
            "codex-app-server",
            "0.170.0",
            path,
        );
        assert!(
            !other.passed && other.detail.contains("0.169.0"),
            "{other:?}"
        );
        let profile = live_check(
            &record(true, "codex-cli 0.170.0", "x"),
            "codex-app-server",
            "0.170.0",
            path,
        );
        assert!(!profile.passed);
        assert!(!live_check("not json", "p", "1.0.0", path).passed);
    }

    #[test]
    fn launch_flags_come_from_the_drivers_own_launch() {
        let profile = profiles::by_id("qwen-code-acp").unwrap();
        let (subcommand, flags) = launch_flags(profile, "/work");
        assert_eq!(subcommand, Vec::<String>::new());
        assert!(flags.contains(&"--acp".to_owned()), "{flags:?}");
        let codex = profiles::by_id("codex-app-server").unwrap();
        let (subcommand, _) = launch_flags(codex, "/work");
        assert_eq!(subcommand.first().map(String::as_str), Some("app-server"));
    }
}
