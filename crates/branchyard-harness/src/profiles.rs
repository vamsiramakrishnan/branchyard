//! Harness profiles: which driver speaks to each harness, and how it starts.
//!
//! Every integration target in `docs/harness-integration.md` appears here,
//! either with one or more implemented profiles or as not yet implemented
//! with the reason. Harness IDs are the ones in `branchyard_controls::harness`.
//!
//! Commands name executables resolved inside the harness image; images pin
//! the versions. Implemented means the driver exists and passes its protocol
//! tests, not that the profile has passed runtime qualification.

use crate::acp::Acp;
use crate::claude_code::ClaudeCode;
use crate::codex::Codex;
use crate::Driver;

/// The wire protocol a profile uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// Claude Code print mode with stream-json and stdio permission prompts.
    ClaudeStreamJson,
    /// Codex App Server JSON-RPC.
    CodexAppServer,
    /// Agent Client Protocol v1.
    Acp,
}

/// One way to drive one harness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile {
    /// Profile identifier, unique across the catalog.
    pub id: &'static str,
    /// Harness ID in `branchyard_controls::harness`.
    pub harness: &'static str,
    pub protocol: Protocol,
    /// Executable and fixed arguments. Drivers append protocol arguments.
    pub command: &'static [&'static str],
    /// Harness version whose protocol the driver was checked against, when a
    /// real transcript or generated schema exists for it.
    pub checked_against: Option<&'static str>,
    /// ACP `_meta` for session requests, as JSON.
    pub acp_session_meta: Option<&'static str>,
}

impl Profile {
    /// A fresh driver for this profile.
    pub fn driver(&self) -> Box<dyn Driver> {
        self.driver_with(self.command.iter().map(|part| (*part).to_owned()).collect())
    }

    /// A fresh driver launching `command` instead of the profile's own, for
    /// an executable installed under another path.
    pub fn driver_with(&self, command: Vec<String>) -> Box<dyn Driver> {
        match self.protocol {
            Protocol::ClaudeStreamJson => Box::new(ClaudeCode::new(command)),
            Protocol::CodexAppServer => Box::new(Codex::new(command)),
            Protocol::Acp => {
                let driver = Acp::new(command);
                Box::new(match self.acp_session_meta {
                    Some(meta) => driver
                        .with_session_meta(serde_json::from_str(meta).expect("valid profile meta")),
                    None => driver,
                })
            }
        }
    }
}

const fn acp(id: &'static str, harness: &'static str, command: &'static [&'static str]) -> Profile {
    Profile {
        id,
        harness,
        protocol: Protocol::Acp,
        command,
        checked_against: None,
        acp_session_meta: None,
    }
}

/// Keeps permission bypass unavailable in claude-agent-acp sessions, so no
/// mode switch can skip Branchyard's permission answers. The adapter
/// otherwise enables it unless it runs as root without `IS_SANDBOX`.
const CLAUDE_ACP_NO_BYPASS: &str =
    r#"{"claudeCode":{"options":{"allowDangerouslySkipPermissions":false}}}"#;

/// Implemented profiles. The first profile for a harness is its default.
pub const PROFILES: &[Profile] = &[
    Profile {
        id: "claude-code-stream-json",
        harness: "claude-code",
        protocol: Protocol::ClaudeStreamJson,
        command: &["claude"],
        checked_against: Some("Claude Code 2.1.283"),
        acp_session_meta: None,
    },
    Profile {
        checked_against: Some("claude-agent-acp 0.81.2"),
        acp_session_meta: Some(CLAUDE_ACP_NO_BYPASS),
        ..acp("claude-code-acp", "claude-code", &["claude-agent-acp"])
    },
    Profile {
        id: "codex-app-server",
        harness: "codex",
        protocol: Protocol::CodexAppServer,
        command: &["codex"],
        checked_against: Some("codex-cli 0.157.1"),
        acp_session_meta: None,
    },
    acp("codex-acp", "codex", &["codex-acp"]),
    acp("oh-my-pi-acp", "oh-my-pi", &["omp", "acp"]),
    acp(
        "deepseek-harness-acp",
        "deepseek-harness",
        &["dsh", "--profile", "acp"],
    ),
    acp(
        "gemini-cli-acp",
        "gemini-cli",
        &["gemini", "--experimental-acp"],
    ),
    acp("opencode-acp", "opencode", &["opencode", "acp"]),
    acp("goose-acp", "goose", &["goose", "acp"]),
    acp("cursor-acp", "cursor", &["agent", "acp"]),
    acp(
        "github-copilot-acp",
        "github-copilot",
        &["copilot", "--acp"],
    ),
    acp("qwen-code-acp", "qwen-code", &["qwen", "--acp"]),
    acp("kimi-cli-acp", "kimi-cli", &["kimi", "acp"]),
    acp("hermes-acp", "hermes", &["hermes", "acp"]),
];

/// Integration targets without a driver yet, and why.
pub const NOT_IMPLEMENTED: &[(&str, &str)] = &[
    (
        "antigravity",
        "native NDJSON streaming CLI; needs its own driver",
    ),
    ("pi", "native RPC mode; needs its own driver"),
    ("amp", "native NDJSON streaming CLI; needs its own driver"),
    ("aider", "batch process without a persistent protocol"),
];

/// The default profile for a harness.
pub fn default_for(harness: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|profile| profile.harness == harness)
}

/// Look up a profile by ID.
pub fn by_id(id: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|profile| profile.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard_controls::harness;
    use std::collections::BTreeSet;

    fn matrix() -> String {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/harness-integration.md"
        );
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn profile_ids_are_unique_and_harnesses_are_registered() {
        let ids: BTreeSet<_> = PROFILES.iter().map(|p| p.id).collect();
        assert_eq!(ids.len(), PROFILES.len());
        for profile in PROFILES {
            assert!(
                harness::by_id(profile.harness).is_some(),
                "{} is not registered",
                profile.harness
            );
        }
    }

    #[test]
    fn every_integration_target_is_implemented_or_explained() {
        for target in harness::HARNESSES.iter().filter(|h| h.target.is_some()) {
            let implemented = default_for(target.id).is_some();
            let explained = NOT_IMPLEMENTED.iter().any(|(id, _)| *id == target.id);
            assert!(
                implemented != explained,
                "{} must be exactly one of implemented or explained",
                target.id
            );
        }
        for (id, _) in NOT_IMPLEMENTED {
            assert!(
                harness::by_id(id).and_then(|h| h.target).is_some(),
                "{id} is not a target"
            );
        }
    }

    #[test]
    fn claude_code_and_codex_default_to_their_native_protocols() {
        assert_eq!(
            default_for("claude-code").unwrap().protocol,
            Protocol::ClaudeStreamJson
        );
        assert_eq!(
            default_for("codex").unwrap().protocol,
            Protocol::CodexAppServer
        );
        assert!(by_id("claude-code-acp").is_some() && by_id("codex-acp").is_some());
    }

    #[test]
    fn acp_commands_match_the_integration_matrix() {
        let matrix = matrix();
        for profile in PROFILES.iter().filter(|p| p.protocol == Protocol::Acp) {
            if matches!(profile.id, "claude-code-acp" | "codex-acp") {
                continue; // adapters, named by package rather than command
            }
            let command = format!("`{}`", profile.command.join(" "));
            assert!(matrix.contains(&command), "{command} is not in the matrix");
        }
    }
}
