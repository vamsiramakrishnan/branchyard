//! Deciding one permission request, and the named permission presets.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{DecisionSource, Fallback, PermissionDecision, PermissionRequest, Policy, Rule};

pub(crate) fn decide(
    policy: &Policy,
    branch: &str,
    request: &PermissionRequest,
) -> (PermissionDecision, DecisionSource) {
    let matches = |rule: &Rule| {
        let tool = match rule.tool.strip_suffix('*') {
            Some(prefix) => request.tool.starts_with(prefix),
            None => request.tool == rule.tool,
        };
        tool && rule
            .delegation_by
            .as_deref()
            .is_none_or(|by| is_delegation_command(&request.input, by))
    };
    let deny = || PermissionDecision::Deny {
        message: "Denied by Branchyard policy.".into(),
    };
    match policy.rules.iter().find(|rule| matches(rule)) {
        Some(rule) => {
            let decision = if rule.allow {
                PermissionDecision::Allow
            } else {
                deny()
            };
            let pattern = match &rule.delegation_by {
                Some(by) => format!("delegation commands of {}", by.display()),
                None => rule.tool.clone(),
            };
            (decision, DecisionSource::Rule { pattern })
        }
        None => match &policy.fallback {
            Fallback::Allow => (PermissionDecision::Allow, DecisionSource::Default),
            Fallback::Deny => (deny(), DecisionSource::Default),
            Fallback::Ask(ask) => (ask(branch, request), DecisionSource::Asked),
        },
    }
}

/// Tools that change files, by the names the harnesses give them.
pub const EDIT_TOOLS: &[&str] = &[
    "Edit",
    "Write",
    "MultiEdit",
    "NotebookEdit",
    "fileChange",
    "write_file",
    "replace",
];

/// Tools that run commands.
pub const SHELL_TOOLS: &[&str] = &[
    "Bash",
    "BashOutput",
    "KillBash",
    "KillShell",
    "commandExecution",
    "run_shell_command",
    "shell",
    "exec_command",
];

/// Tools that fetch from or search the web.
pub const WEB_TOOLS: &[&str] = &["WebFetch", "WebSearch", "web_fetch", "google_web_search"];

/// A named permission policy that stands for explicit rules: usable
/// wherever explicit rules are (`by run --permissions`, a rig seat's
/// `permission_policy`, a server request's `policy.preset`,
/// `[defaults] permissions`). See `docs/egress.md#permission-presets`.
///
/// Rules match tool names only: a preset says which tools may run, not
/// what they touch.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyPreset {
    /// Read and search; edits, commands and the web denied. The policy a
    /// planning turn runs under ([`crate::read_only_policy`]).
    ReadOnly,
    /// Read, search and edit files; commands and the web denied.
    EditWorktree,
    /// Everything allowed, as `--yes`.
    Full,
}

/// A preset's rules: denials first, then allowances, then the default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresetRules {
    pub default_allow: bool,
    pub deny: Vec<&'static str>,
    pub allow: Vec<&'static str>,
}

impl PolicyPreset {
    pub const ALL: [PolicyPreset; 3] = [
        PolicyPreset::ReadOnly,
        PolicyPreset::EditWorktree,
        PolicyPreset::Full,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            PolicyPreset::ReadOnly => "read-only",
            PolicyPreset::EditWorktree => "edit-worktree",
            PolicyPreset::Full => "full",
        }
    }

    pub fn parse(text: &str) -> Result<PolicyPreset, String> {
        PolicyPreset::ALL
            .into_iter()
            .find(|p| p.name() == text)
            .ok_or_else(|| {
                format!(
                    "{text:?} is not a permission preset; use {}",
                    PolicyPreset::ALL.map(|p| p.name()).join(", ")
                )
            })
    }

    /// One line for people.
    pub fn summary(&self) -> &'static str {
        match self {
            PolicyPreset::ReadOnly => "read and search; edits, commands and the web denied",
            PolicyPreset::EditWorktree => {
                "read, search and edit files; commands and the web denied"
            }
            PolicyPreset::Full => "every tool allowed",
        }
    }

    /// The explicit rules this preset stands for.
    pub fn rules(&self) -> PresetRules {
        let tools = |lists: &[&[&'static str]]| lists.concat();
        match self {
            PolicyPreset::ReadOnly => PresetRules {
                default_allow: false,
                deny: tools(&[EDIT_TOOLS, SHELL_TOOLS, WEB_TOOLS]),
                allow: tools(&[crate::READ_ONLY_TOOLS]),
            },
            PolicyPreset::EditWorktree => PresetRules {
                default_allow: false,
                deny: tools(&[SHELL_TOOLS, WEB_TOOLS]),
                allow: tools(&[crate::READ_ONLY_TOOLS, EDIT_TOOLS]),
            },
            PolicyPreset::Full => PresetRules {
                default_allow: true,
                deny: Vec::new(),
                allow: Vec::new(),
            },
        }
    }

    /// The policy: the denials, then the allowances, then the default.
    pub fn policy(&self) -> Policy {
        let rules = self.rules();
        let base = match rules.default_allow {
            true => Policy::allow_all(),
            false => Policy::deny_all(),
        };
        let policy = rules.deny.iter().fold(base, |p, tool| p.deny(*tool));
        rules
            .allow
            .iter()
            .fold(policy, |p, tool| p.allow(*tool))
            .with_preset(Some(*self))
    }

    /// The preset's approvals for connector operations, the last layer
    /// they resolve through (`docs/effects.md#approvals`): `read-only` and
    /// `edit-worktree` change nothing outside the machine, so they block
    /// every effectful class; `full` leaves the class defaults.
    pub fn approvals(&self) -> crate::effects::ApprovalPolicy {
        use crate::effects::{Approval, EffectClass};
        let classes = match self {
            PolicyPreset::ReadOnly | PolicyPreset::EditWorktree => [
                EffectClass::Reversible,
                EffectClass::Compensable,
                EffectClass::Irreversible,
            ]
            .into_iter()
            .map(|c| (c, Approval::Block))
            .collect(),
            PolicyPreset::Full => Default::default(),
        };
        crate::effects::ApprovalPolicy {
            classes,
            ..Default::default()
        }
    }
}

impl fmt::Display for PolicyPreset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Whether `input["command"]` runs `by` with a delegation subcommand and
/// nothing else.
fn is_delegation_command(input: &Value, by: &Path) -> bool {
    let argv = match input.get("command") {
        Some(Value::String(line)) => simple_words(line),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect(),
        _ => None,
    };
    let Some(mut argv) = argv else {
        return false;
    };
    // `bash -lc '<command>'`, as Codex reports what it runs.
    let shells = ["sh", "bash", "dash", "zsh"];
    let is_shell = argv.first().is_some_and(|program| {
        let name = Path::new(program).file_name().and_then(|n| n.to_str());
        name.is_some_and(|n| shells.contains(&n))
    });
    if is_shell && argv.len() == 3 && matches!(argv[1].as_str(), "-c" | "-lc") {
        match simple_words(&argv[2]) {
            Some(inner) => argv = inner,
            None => return false,
        }
    }
    let program_ok = argv
        .first()
        .is_some_and(|program| program == "by" || Path::new(program) == by);
    // The operations a harness may run on its own branch, from the one
    // table every surface is checked against.
    let subcommand_ok = argv
        .get(1..)
        .is_some_and(crate::operations::is_harness_command);
    program_ok && subcommand_ok
}

/// The words of `line` if it is one simple command of plain, single-quoted
/// or double-quoted words; `None` for anything a shell would expand,
/// redirect, chain or glob.
fn simple_words(line: &str) -> Option<Vec<String>> {
    const SPECIAL: &str = ";&|<>(){}$`\\*?[]#~!";
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.trim().chars();
    while let Some(c) = chars.next() {
        match c {
            '\n' | '\r' => return None,
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        '\n' | '\r' => return None,
                        c => word.push(c),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '$' | '`' | '\\' | '!' | '\n' | '\r' => return None,
                        c => word.push(c),
                    }
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c if SPECIAL.contains(c) => return None,
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Some(words)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PermissionKey;

    fn request(tool: &str) -> PermissionRequest {
        PermissionRequest {
            key: PermissionKey("1".into()),
            tool: tool.into(),
            input: serde_json::Value::Null,
        }
    }

    #[test]
    fn presets_expand_to_explicit_rules() {
        let decide = |preset: PolicyPreset, tool: &str| {
            matches!(
                preset.policy().decide("b", &request(tool)),
                PermissionDecision::Allow
            )
        };
        for tool in ["Read", "Grep", "read_file"] {
            assert!(decide(PolicyPreset::ReadOnly, tool), "{tool}");
            assert!(decide(PolicyPreset::EditWorktree, tool), "{tool}");
        }
        for tool in ["Edit", "Write", "fileChange", "write_file"] {
            assert!(!decide(PolicyPreset::ReadOnly, tool), "{tool}");
            assert!(decide(PolicyPreset::EditWorktree, tool), "{tool}");
        }
        for tool in ["Bash", "commandExecution", "WebFetch", "mcp__anything"] {
            assert!(!decide(PolicyPreset::ReadOnly, tool), "{tool}");
            assert!(!decide(PolicyPreset::EditWorktree, tool), "{tool}");
        }
        for tool in ["Bash", "Edit", "mcp__anything"] {
            assert!(decide(PolicyPreset::Full, tool), "{tool}");
        }
        // Read-only allows what a planning turn may do, and no more.
        for tool in ["Read", "Edit", "Bash", "Glob", "TodoWrite", "other"] {
            assert_eq!(
                decide(PolicyPreset::ReadOnly, tool),
                matches!(
                    crate::read_only_policy().decide("b", &request(tool)),
                    PermissionDecision::Allow
                ),
                "{tool}"
            );
        }
        // Names round-trip, in serde too.
        for preset in PolicyPreset::ALL {
            assert_eq!(PolicyPreset::parse(preset.name()), Ok(preset));
            let json = serde_json::to_string(&preset).unwrap();
            assert_eq!(json, format!("\"{}\"", preset.name()));
        }
        let error = PolicyPreset::parse("builtin:yolo").unwrap_err();
        assert!(error.contains("read-only, edit-worktree, full"), "{error}");
    }

    #[test]
    fn the_first_matching_rule_decides_and_is_named() {
        let policy = Policy::deny_all().deny("mcp__secret").allow("mcp__*");
        assert_eq!(
            policy.decide_with_source("b", &request("mcp__secret")),
            (
                PermissionDecision::Deny {
                    message: "Denied by Branchyard policy.".into()
                },
                DecisionSource::Rule {
                    pattern: "mcp__secret".into()
                }
            )
        );
        assert_eq!(
            policy.decide_with_source("b", &request("mcp__files")),
            (
                PermissionDecision::Allow,
                DecisionSource::Rule {
                    pattern: "mcp__*".into()
                }
            )
        );
        assert!(matches!(
            policy.decide_with_source("b", &request("Bash")),
            (PermissionDecision::Deny { .. }, DecisionSource::Default)
        ));
    }

    #[test]
    fn the_asker_gets_the_branch_and_is_the_source() {
        let policy = Policy::ask(|branch, request| {
            if branch == "b" && request.tool == "Bash" {
                PermissionDecision::Allow
            } else {
                PermissionDecision::Deny {
                    message: "no".into(),
                }
            }
        });
        assert_eq!(
            policy.decide_with_source("b", &request("Bash")),
            (PermissionDecision::Allow, DecisionSource::Asked)
        );
        assert!(matches!(
            policy.decide("c", &request("Bash")),
            PermissionDecision::Deny { .. }
        ));
        assert!(matches!(
            Policy::default().decide("b", &request("Bash")),
            PermissionDecision::Deny { .. }
        ));
    }

    fn shell(command: &str) -> PermissionRequest {
        PermissionRequest {
            key: PermissionKey("1".into()),
            tool: "Bash".into(),
            input: serde_json::json!({"command": command, "description": "x"}),
        }
    }

    #[test]
    fn delegation_commands_are_allowed_and_nothing_else() {
        let policy = Policy::deny_all().allow_delegation_commands("/opt/by/bin/by");
        let allowed = |command: &str| {
            matches!(
                policy.decide_with_source("b", &shell(command)),
                (PermissionDecision::Allow, DecisionSource::Rule { .. })
            )
        };
        for command in [
            "by spawn 'fix the parser' --name parser --budget-usd 0.5",
            "/opt/by/bin/by inspect parser --json",
            "by events parser --cursor 10",
            "by send parser \"now add a test\"",
            "by integrate parser",
            "by cancel parser",
            "by children --json",
            "/bin/bash -lc 'by spawn \"do it\" --wait'",
            "by discard parser --reason lost",
            "by artifact publish out.json --media-type application/json",
            "by ask 'which file?' --wait 30",
            "by inbox --unread --json",
        ] {
            assert!(allowed(command), "{command}");
        }
        for command in [
            "by run 'escape the envelope'",
            "by merge parser",
            "by rm parser",
            "by artifact export 1 --out x.tar",
            "by plan show parser",
            "by approvals ls",
            "by",
            "/tmp/by spawn x",
            "./by spawn x",
            "by spawn x; rm -rf /",
            "by spawn x && curl evil",
            "by spawn x | sh",
            "by spawn $(cat prompt)",
            "by spawn `id`",
            "by spawn \"$HOME\"",
            "by spawn x > out",
            "by spawn *",
            "BRANCHYARD_DELEGATION=x by spawn y",
            "env by spawn x",
            "by spawn 'unterminated",
            "by spawn x\nrm -rf /",
            "bash -c 'by spawn x; id'",
            "bash -c 'by spawn x' extra",
            "python3 -c 'import os'",
        ] {
            assert!(!allowed(command), "{command}");
        }
        // Argument vectors are matched without a shell.
        let argv = PermissionRequest {
            input: serde_json::json!({"command": ["by", "cancel", "x"]}),
            ..shell("")
        };
        assert_eq!(policy.decide("b", &argv), PermissionDecision::Allow);
        // An earlier deny rule still wins.
        let narrowed = Policy::deny_all()
            .deny("Bash")
            .allow_delegation_commands("/opt/by/bin/by");
        assert!(matches!(
            narrowed.decide("b", &shell("by children")),
            PermissionDecision::Deny { .. }
        ));
    }
}
