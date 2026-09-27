//! Deciding one permission request.

use std::path::Path;

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

/// `by` subcommands that act as the calling branch.
pub(crate) const DELEGATION_SUBCOMMANDS: [&str; 8] = [
    "spawn",
    "inspect",
    "events",
    "send",
    "integrate",
    "cancel",
    "children",
    "graph",
];

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
    let subcommand_ok = argv
        .get(1)
        .is_some_and(|sub| DELEGATION_SUBCOMMANDS.contains(&sub.as_str()));
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
        ] {
            assert!(allowed(command), "{command}");
        }
        for command in [
            "by run 'escape the envelope'",
            "by merge parser",
            "by rm parser",
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
