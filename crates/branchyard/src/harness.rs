//! Choosing a harness profile, finding its executable, the environment it
//! runs with, and its published qualification.

use std::path::{Path, PathBuf};

use branchyard_harness::profiles::{self, Profile, PROFILES};
use branchyard_runtime::Environment;
use serde_json::Value;

use crate::{Error, HarnessInfo, Unavailable};

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
        "antigravity-stream-json",
        include_str!("../../../docs/qualification/antigravity-stream-json.json"),
    ),
    (
        "claude-code-acp",
        include_str!("../../../docs/qualification/claude-code-acp.json"),
    ),
    (
        "claude-code-stream-json",
        include_str!("../../../docs/qualification/claude-code-stream-json.json"),
    ),
    (
        "codex-app-server",
        include_str!("../../../docs/qualification/codex-app-server.json"),
    ),
    (
        "deepseek-harness-acp",
        include_str!("../../../docs/qualification/deepseek-harness-acp.json"),
    ),
    (
        "gemini-cli-acp",
        include_str!("../../../docs/qualification/gemini-cli-acp.json"),
    ),
    (
        "hermes-acp",
        include_str!("../../../docs/qualification/hermes-acp.json"),
    ),
    (
        "oh-my-pi-acp",
        include_str!("../../../docs/qualification/oh-my-pi-acp.json"),
    ),
    (
        "opencode-acp",
        include_str!("../../../docs/qualification/opencode-acp.json"),
    ),
    (
        "pi-rpc",
        include_str!("../../../docs/qualification/pi-rpc.json"),
    ),
    (
        "qwen-code-acp",
        include_str!("../../../docs/qualification/qwen-code-acp.json"),
    ),
];

/// A harness ID picks its default profile; otherwise a profile ID.
pub(crate) fn select(id: Option<&str>) -> Result<&'static Profile, Error> {
    let id = id.unwrap_or(DEFAULT_HARNESS);
    profiles::default_for(id)
        .or_else(|| profiles::by_id(id))
        .ok_or_else(|| Error::UnknownHarness(id.to_owned()))
}

/// What `--harness NAME` was refused for, when no profile answers to `NAME`.
/// A harness the catalog knows but no profile drives (`droid`, `kiro`) is
/// not "no harness named": it says so, why when there is a recorded reason,
/// and what can be driven.
pub(crate) fn unknown_message(name: &str) -> String {
    let mut drivable: Vec<&str> = Vec::new();
    for profile in PROFILES {
        if !drivable.contains(&profile.harness) {
            drivable.push(profile.harness);
        }
    }
    if branchyard_controls::catalog::harness(name).is_none() {
        // `qwen` for `qwen-code`, `gemini` for `gemini-cli`: a name that is
        // the start of one drivable harness is offered as that one.
        let meant: Vec<&str> = drivable
            .iter()
            .copied()
            .filter(|id| id.starts_with(name) || name.starts_with(id))
            .collect();
        let drivable = drivable.join(", ");
        return match meant.as_slice() {
            [] => format!(
                "no harness or profile named {name}; drivable: {drivable} (by harnesses \
                 --profiles lists the profiles)"
            ),
            _ => format!(
                "no harness or profile named {name}; did you mean {}? drivable: {drivable} (by \
                 harnesses --profiles lists the profiles)",
                meant.join(" or ")
            ),
        };
    }
    let drivable = drivable.join(", ");
    let why = profiles::NOT_IMPLEMENTED
        .iter()
        .find(|(id, _)| *id == name)
        .map_or("it has no driver profile".to_owned(), |(_, why)| {
            (*why).to_owned()
        });
    format!("{name} is known to Branchyard but not drivable ({why}); drivable: {drivable}")
}

/// The command to launch: the override, else the profile's own.
pub(crate) fn command(profile: &Profile, over: Option<&[String]>) -> Vec<String> {
    match over {
        Some(command) => command.to_vec(),
        None => profile.command.iter().map(|p| (*p).to_owned()).collect(),
    }
}

/// Refuse a profile whose driver cannot route tool permission requests to
/// Branchyard, unless the task allows it: its tools would run under the
/// harness's own configuration, not the task's policy.
pub(crate) fn check_approvals(profile: &Profile, allowed: bool) -> Result<(), Error> {
    if allowed || profile.driver().capabilities().tool_approvals {
        return Ok(());
    }
    Err(Error::UnroutedApprovals {
        profile: profile.id.to_owned(),
    })
}

/// Refuse `model` for a profile that cannot deliver it. Its provisioner
/// decides first, as a turn's provisioning would: it sets the model itself
/// (Claude Code's `ANTHROPIC_MODEL`, Codex's `config.toml` over ACP, Gemini
/// CLI's `settings.json`, Hermes's `HERMES_INFERENCE_MODEL`), refuses it,
/// or leaves it to the driver, which must then be able to choose one. A
/// setting written into the home needs `private_home`.
pub(crate) fn check_model(profile: &Profile, model: &str, private_home: bool) -> Result<(), Error> {
    let refuse = |reason: &str| {
        Error::Unsupported(format!(
            "{} cannot be given a model: {reason}; drop the model or choose another harness",
            profile.id
        ))
    };
    let mut context =
        branchyard_provision::Context::new(profile.harness, profile.protocol, "/home", "/work");
    context.private_home = private_home;
    context.model = Some(model.to_owned());
    let plan = branchyard_provision::plan(&context).map_err(|why| refuse(&why.0))?;
    let driver = profile.driver();
    if plan.session.model.is_none() || driver.capabilities().model {
        return Ok(());
    }
    let reasons = branchyard_harness::reasons_for(&["model"], driver.capability_reasons());
    Err(refuse(
        reasons.first().map_or("not verified", |(_, why)| why),
    ))
}

/// Refuse a delegated branch on `profile` that could not be given
/// Branchyard's delegation tools, which every delegated turn is given: its
/// provisioner decides, as the turn's provisioning would. A harness that
/// takes MCP servers only in its configuration (Antigravity) needs a home
/// private to the branch for them, so a child of a parent without one is
/// refused when it is spawned rather than failing its first turn.
pub(crate) fn check_delegation_tools(profile: &Profile, private_home: bool) -> Result<(), Error> {
    let mut context =
        branchyard_provision::Context::new(profile.harness, profile.protocol, "/home", "/work");
    context.private_home = private_home;
    context.mcp_servers = vec![branchyard_harness::McpServer {
        name: crate::projection::SERVER_NAME.into(),
        command: "/by".into(),
        args: Vec::new(),
        env: Vec::new(),
    }];
    branchyard_provision::plan(&context)
        .map(drop)
        .map_err(|why| {
            Error::Unsupported(format!(
                "{} cannot be given Branchyard's delegation tools here: {}",
                profile.id, why.0
            ))
        })
}

/// Fail with [`Error::HarnessUnavailable`] unless `command[0]` is an
/// executable file, by path or on `PATH`.
pub fn check_available(harness: &str, command: &[String]) -> Result<(), Error> {
    let unavailable = |cause: Unavailable, reason: String| Error::HarnessUnavailable {
        harness: harness.to_owned(),
        reason,
        cause,
    };
    let Some(program) = command.first().filter(|p| !p.is_empty()) else {
        return Err(unavailable(
            Unavailable::NoCommand,
            "the command is empty".into(),
        ));
    };
    if program.contains('/') {
        if executable(Path::new(program)) {
            return Ok(());
        }
        return Err(unavailable(
            Unavailable::NotExecutable,
            format!("{program} is not an executable file"),
        ));
    }
    match find_on_path(program) {
        Some(_) => Ok(()),
        None => Err(unavailable(
            Unavailable::NotOnPath,
            format!("{program} was not found on PATH"),
        )),
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
    fn operator_command_availability_is_checked_without_running_a_harness() {
        let command = vec![branchyard_testkit::fake_agent_here().display().to_string()];
        assert!(crate::check_harness_command("configured-profile", &command).is_ok());
        assert!(matches!(
            crate::check_harness_command("configured-profile", &[]),
            Err(Error::HarnessUnavailable {
                cause: Unavailable::NoCommand,
                ..
            })
        ));
        assert!(matches!(
            crate::check_harness_command(
                "configured-profile",
                &["/missing-configured-harness".into()]
            ),
            Err(Error::HarnessUnavailable {
                cause: Unavailable::NotExecutable,
                ..
            })
        ));
    }

    /// A name that begins one drivable harness's ID is offered as that one;
    /// a name that begins none lists what is drivable, as before.
    #[test]
    fn an_unknown_harness_name_that_starts_a_known_one_gets_a_did_you_mean() {
        let qwen = unknown_message("qwen");
        assert!(qwen.contains("did you mean qwen-code?"), "{qwen}");
        assert!(
            qwen.starts_with("no harness or profile named qwen;"),
            "{qwen}"
        );
        let nope = unknown_message("nope");
        assert!(!nope.contains("did you mean"), "{nope}");
        assert!(nope.contains("drivable: "), "{nope}");
    }

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
    fn a_harness_the_catalog_knows_but_no_profile_drives_is_not_called_unknown() {
        let droid = Error::UnknownHarness("droid".into()).to_string();
        assert!(
            droid.starts_with("droid is known to Branchyard but not drivable"),
            "{droid}"
        );
        assert!(droid.contains("drivable: claude-code, codex, "), "{droid}");
        // A recorded reason is quoted.
        let aider = Error::UnknownHarness("aider".into()).to_string();
        assert!(aider.contains("batch process"), "{aider}");
        // A name the catalog does not have is still "no harness named".
        let nope = Error::UnknownHarness("nope".into()).to_string();
        assert!(
            nope.starts_with("no harness or profile named nope; drivable: claude-code, "),
            "{nope}"
        );
        // Each harness is listed once, though it has two profiles.
        assert_eq!(nope.matches("claude-code").count(), 1, "{nope}");
    }

    #[test]
    fn a_model_is_refused_only_where_neither_provisioner_nor_driver_delivers_it() {
        let check =
            |id: &str, private: bool| check_model(select(Some(id)).unwrap(), "small", private);
        // A driver that takes one, and provisioners that set one themselves.
        for id in [
            "claude-code-stream-json",
            "claude-code-acp",
            "codex-acp",
            "gemini-cli-acp",
            "hermes-acp",
            "opencode-acp",
        ] {
            assert!(check(id, true).is_ok(), "{id}: {:?}", check(id, true));
        }
        // Through the environment, without a private home too.
        assert!(check("claude-code-acp", false).is_ok());
        assert!(check("hermes-acp", false).is_ok());
        // A setting written into the home needs one of its own.
        for id in ["codex-acp", "gemini-cli-acp", "opencode-acp"] {
            let refused = check(id, false);
            assert!(
                matches!(&refused, Err(Error::Unsupported(why))
                    if why.starts_with(&format!("{id} cannot be given a model"))
                        && why.contains("--isolated")),
                "{refused:?}"
            );
        }
        // A driver left to choose it that cannot.
        let refused = check("goose-acp", true);
        assert!(
            matches!(&refused, Err(Error::Unsupported(why))
                if why.starts_with("goose-acp cannot be given a model")
                    && why.contains("ACP v1 has no model parameter")),
            "{refused:?}"
        );
    }

    #[test]
    fn missing_executables_are_reported_before_launch() {
        let missing = check_available("x", &["branchyard-no-such-binary".into()]);
        assert!(
            matches!(&missing, Err(Error::HarnessUnavailable { reason, cause: Unavailable::NotOnPath, .. }) if reason.contains("not found on PATH")),
            "{missing:?}"
        );
        let cause = |command: &[String]| match check_available("x", command) {
            Err(Error::HarnessUnavailable { cause, .. }) => Some(cause),
            _ => None,
        };
        assert_eq!(
            cause(&["/nonexistent/bin".into()]),
            Some(Unavailable::NotExecutable)
        );
        assert_eq!(cause(&[]), Some(Unavailable::NoCommand));
        assert!(check_available("x", &["sh".into()]).is_ok());
        // The detail says the cause by name, and the name reads back.
        let detail = missing.unwrap_err().detail().unwrap();
        assert_eq!(detail["harness"], "x");
        assert_eq!(detail["cause"], "not_on_path");
        for cause in Unavailable::ALL {
            assert_eq!(Unavailable::from_name(cause.as_str()), Some(cause));
        }
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
        assert_eq!(qualification("gemini-cli-acp").unwrap(), "9/9 on 0.65.0");
        assert_eq!(
            qualification("antigravity-stream-json").unwrap(),
            "9/9 on 1.4.7"
        );
        assert_eq!(
            qualification("codex-app-server").unwrap(),
            "9/9 on codex-cli 0.164.0"
        );
        assert_eq!(qualification("oh-my-pi-acp").unwrap(), "9/9 on omp/18.12.6");
        // 25 October 2026, GLM-4.6 through Z.ai: the three permission
        // scenarios fail on both, since neither asks before a shell command
        // that writes inside its workspace.
        assert_eq!(
            qualification("deepseek-harness-acp").unwrap(),
            "6/9 on 0.2.1-rc.2"
        );
        assert_eq!(
            qualification("hermes-acp").unwrap(),
            "6/9 on Hermes Agent v0.19.0 (2026.7.20)"
        );
        assert_eq!(qualification("qwen-code-acp").unwrap(), "9/9 on 0.26.0");
        assert_eq!(qualification("opencode-acp").unwrap(), "9/9 on 1.19.4");
        assert_eq!(qualification("pi-rpc").unwrap(), "9/9 on 1.4.0");
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
