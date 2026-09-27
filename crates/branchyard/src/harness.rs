//! Choosing a harness profile, finding its executable, the environment it
//! runs with, and its published qualification.

use std::path::{Path, PathBuf};

use branchyard_harness::profiles::{self, Profile, PROFILES};
use branchyard_runtime::Environment;
use serde_json::Value;

use crate::{Error, HarnessInfo};

pub(crate) const DEFAULT_HARNESS: &str = "claude-code";

/// Claude Code configuration a developer may set that a child harness should
/// keep: provider selection, credentials, TLS client identity and limits.
/// Every other `CLAUDE*` variable describes the *current* Claude Code
/// process or host (its session, remote ingress, messaging socket) and is
/// removed, so a child never runs under its parent's identity.
pub(crate) const CLAUDE_CONFIGURATION: &[&str] = &[
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_SKIP_BEDROCK_AUTH",
    "CLAUDE_CODE_SKIP_VERTEX_AUTH",
    "CLAUDE_CODE_SKIP_FOUNDRY_AUTH",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_API_KEY_HELPER_TTL_MS",
    "CLAUDE_CODE_CLIENT_CERT",
    "CLAUDE_CODE_CLIENT_KEY",
    "CLAUDE_CODE_CLIENT_KEY_PASSPHRASE",
    "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
];

/// Whether an inherited variable belongs to the running Claude Code process
/// or its host rather than to the developer's configuration.
pub(crate) fn is_parent_session_variable(name: &str) -> bool {
    name.to_ascii_uppercase().starts_with("CLAUDE") && !CLAUDE_CONFIGURATION.contains(&name)
}

/// Published qualification reports, embedded at build time. A test checks
/// this list against `docs/qualification/`.
pub(crate) const REPORTS: &[(&str, &str)] = &[
    (
        "claude-code-acp",
        include_str!("../../../docs/qualification/claude-code-acp.json"),
    ),
    (
        "claude-code-stream-json",
        include_str!("../../../docs/qualification/claude-code-stream-json.json"),
    ),
];

/// A harness ID picks its default profile; otherwise a profile ID.
pub(crate) fn select(id: Option<&str>) -> Result<&'static Profile, Error> {
    let id = id.unwrap_or(DEFAULT_HARNESS);
    profiles::default_for(id)
        .or_else(|| profiles::by_id(id))
        .ok_or_else(|| Error::UnknownHarness(id.to_owned()))
}

/// The command to launch: the override, else the profile's own.
pub(crate) fn command(profile: &Profile, over: Option<&[String]>) -> Vec<String> {
    match over {
        Some(command) => command.to_vec(),
        None => profile.command.iter().map(|p| (*p).to_owned()).collect(),
    }
}

/// Fail with [`Error::HarnessUnavailable`] unless `command[0]` is an
/// executable file, by path or on `PATH`.
pub(crate) fn check_available(harness: &str, command: &[String]) -> Result<(), Error> {
    let unavailable = |reason: String| Error::HarnessUnavailable {
        harness: harness.to_owned(),
        reason,
    };
    let Some(program) = command.first().filter(|p| !p.is_empty()) else {
        return Err(unavailable("the command is empty".into()));
    };
    if program.contains('/') {
        if executable(Path::new(program)) {
            return Ok(());
        }
        return Err(unavailable(format!("{program} is not an executable file")));
    }
    match find_on_path(program) {
        Some(_) => Ok(()),
        None => Err(unavailable(format!("{program} was not found on PATH"))),
    }
}

pub(crate) fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| executable(candidate))
}

pub(crate) fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Whether an inherited variable is Branchyard's own: a server to talk to,
/// or an outer harness's branch and delegation token. The engine sets the
/// ones this branch should have; none is inherited.
pub(crate) fn is_branchyard_variable(name: &str) -> bool {
    name.starts_with("BRANCHYARD_")
}

/// The harness's environment: this process's, minus the parent Claude Code
/// session's variables and Branchyard's own; or, isolated, the runtime's
/// scrubbed environment with `home`.
pub(crate) fn environment(isolated_home: Option<&Path>) -> Environment {
    match isolated_home {
        Some(home) => Environment::new(home).strip("BRANCHYARD_"),
        None => std::env::vars_os()
            .filter_map(|(name, _)| name.into_string().ok())
            .filter(|name| is_parent_session_variable(name) || is_branchyard_variable(name))
            .fold(Environment::inherit(), |env, name| env.remove(name)),
    }
}

pub(crate) fn list() -> Vec<HarnessInfo> {
    PROFILES
        .iter()
        .map(|profile| HarnessInfo {
            harness: profile.harness.to_owned(),
            profile: profile.id.to_owned(),
            default: profiles::default_for(profile.harness).map(|p| p.id) == Some(profile.id),
            available: profile
                .command
                .first()
                .is_some_and(|program| find_on_path(program).is_some()),
            qualification: qualification(profile.id),
        })
        .collect()
}

/// "9/9 on <harness_version>", noting a correction when the report has one.
pub(crate) fn qualification(profile: &str) -> Option<String> {
    let (_, text) = REPORTS.iter().find(|(id, _)| *id == profile)?;
    let report: Value = serde_json::from_str(text).ok()?;
    let scenarios = report["scenarios"].as_array()?;
    let passed = scenarios
        .iter()
        .filter(|s| s["passed"].as_bool() == Some(true))
        .count();
    let version = report["harness_version"]
        .as_str()
        .unwrap_or("unknown version");
    let mut summary = format!("{passed}/{} on {version}", scenarios.len());
    if report.get("correction").is_some_and(|c| !c.is_null()) {
        summary.push_str(&format!(
            "; corrected since, see docs/qualification/{profile}.json"
        ));
    }
    Some(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_ids_pick_default_profiles_and_profile_ids_work_too() {
        assert_eq!(select(None).unwrap().id, "claude-code-stream-json");
        assert_eq!(select(Some("codex")).unwrap().id, "codex-app-server");
        assert_eq!(select(Some("codex-acp")).unwrap().id, "codex-acp");
        assert!(matches!(
            select(Some("nope")),
            Err(Error::UnknownHarness(id)) if id == "nope"
        ));
    }

    #[test]
    fn missing_executables_are_reported_before_launch() {
        let missing = check_available("x", &["branchyard-no-such-binary".into()]);
        assert!(
            matches!(&missing, Err(Error::HarnessUnavailable { reason, .. }) if reason.contains("not found on PATH")),
            "{missing:?}"
        );
        assert!(check_available("x", &["/nonexistent/bin".into()]).is_err());
        assert!(check_available("x", &[]).is_err());
        assert!(check_available("x", &["sh".into()]).is_ok());
    }

    #[test]
    fn every_published_report_is_embedded_and_summarized() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/qualification");
        let mut published: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| {
                let name = e.unwrap().file_name().into_string().unwrap();
                name.strip_suffix(".json").map(str::to_owned)
            })
            .collect();
        published.sort();
        let embedded: Vec<&str> = REPORTS.iter().map(|(id, _)| *id).collect();
        assert_eq!(published, embedded);
        for (id, _) in REPORTS {
            assert!(profiles::by_id(id).is_some(), "{id} is not a profile");
        }
        assert_eq!(qualification("claude-code-acp").unwrap(), "9/9 on 0.81.2");
        assert_eq!(qualification("codex-app-server"), None);
    }

    #[test]
    fn inherited_environments_drop_the_parent_session_but_keep_configuration() {
        for name in [
            "CLAUDECODE",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_REMOTE_SESSION_ID",
            "CLAUDE_SESSION_INGRESS_TOKEN_FILE",
            "CLAUDE_CODE_MESSAGING_SOCKET",
            "claude_code_anything_new",
        ] {
            assert!(is_parent_session_variable(name), "{name} must be removed");
        }
        for name in [
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CONFIG_DIR",
            "ANTHROPIC_API_KEY",
            "PATH",
        ] {
            assert!(!is_parent_session_variable(name), "{name} must be kept");
        }
        let env = environment(None);
        assert_eq!(
            env.home(),
            Path::new(&std::env::var_os("HOME").unwrap_or_default())
        );
        let isolated = environment(Some(Path::new("/h")));
        assert_eq!(isolated.home(), Path::new("/h"));
    }
}
