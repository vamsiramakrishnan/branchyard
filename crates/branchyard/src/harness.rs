//! Choosing a harness profile, finding its executable, the environment it
//! runs with, and its published qualification.

use std::path::{Path, PathBuf};

use branchyard_harness::profiles::{self, Profile, PROFILES};
use branchyard_runtime::Environment;
use serde_json::Value;

use crate::{Error, HarnessInfo};

pub(crate) const DEFAULT_HARNESS: &str = "claude-code";

/// Variables a harness sets for the processes it runs, which would make a
/// harness started from inside one believe it is a nested session. The
/// first seven are the ones Claude Code 2.1 itself removes before starting
/// a child Claude Code; `CLAUDE_CODE_ENTRYPOINT` would misreport how the
/// child was launched, and `CLAUDE_CODE_SSE_PORT` would attach it to the
/// parent's IDE connection. Credentials are not markers and are kept.
pub(crate) const NESTED_SESSION_MARKERS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_CHROME_MCP_ORG_DENIED",
    "CLAUDE_CODE_EVAL_INTERVIEW_SESSION",
    "CLAUDE_CODE_BRIDGE_SESSION_ID",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SSE_PORT",
];

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

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The harness's environment: this process's, minus nested-session
/// markers; or, isolated, the runtime's scrubbed environment with `home`.
pub(crate) fn environment(isolated_home: Option<&Path>) -> Environment {
    match isolated_home {
        Some(home) => Environment::new(home),
        None => NESTED_SESSION_MARKERS
            .iter()
            .fold(Environment::inherit(), |env, name| env.remove(*name)),
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
    fn inherited_environments_drop_only_nested_session_markers() {
        let env = environment(None);
        assert_eq!(
            env.home(),
            Path::new(&std::env::var_os("HOME").unwrap_or_default())
        );
        let isolated = environment(Some(Path::new("/h")));
        assert_eq!(isolated.home(), Path::new("/h"));
    }
}
