//! Deciding one permission request.

use crate::{DecisionSource, Fallback, PermissionDecision, PermissionRequest, Policy, Rule};

pub(crate) fn decide(
    policy: &Policy,
    branch: &str,
    request: &PermissionRequest,
) -> (PermissionDecision, DecisionSource) {
    let matches = |rule: &Rule| match rule.tool.strip_suffix('*') {
        Some(prefix) => request.tool.starts_with(prefix),
        None => request.tool == rule.tool,
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
            let source = DecisionSource::Rule {
                pattern: rule.tool.clone(),
            };
            (decision, source)
        }
        None => match &policy.fallback {
            Fallback::Allow => (PermissionDecision::Allow, DecisionSource::Default),
            Fallback::Deny => (deny(), DecisionSource::Default),
            Fallback::Ask(ask) => (ask(branch, request), DecisionSource::Asked),
        },
    }
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
}
