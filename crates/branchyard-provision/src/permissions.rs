//! A branch's permission policy as data, for a harness that enforces its
//! own permission rules instead of asking Branchyard.
//!
//! Most drivers route every permission request to Branchyard, which
//! decides it under the branch's policy. Antigravity's CLI cannot: its
//! headless stream carries no permission requests, and a tool that needs
//! approval is soft-denied unless a rule in its own settings allows it.
//! For such a harness the engine hands its provisioner the policy as
//! [`Permissions`], and the provisioner writes the rules the harness reads
//! ([`PermissionsWritten`] records them). A translation is never wider than
//! the policy: what the policy would ask a person about is not allowed,
//! since nobody can be asked.

use serde::{Deserialize, Serialize};

/// The name harnesses know Branchyard's own MCP server by: the delegation
/// tools. Its tools are allowed by [`Permissions::delegation_by`]'s rule as
/// well as by the rules that name them.
pub const DELEGATION_SERVER: &str = "branchyard";

/// What a policy does with a request no rule decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fallback {
    /// The request is allowed.
    Allow,
    /// The request is denied.
    Deny,
    /// A person is asked: a harness that cannot ask treats it as a denial.
    Ask,
}

/// One rule of the policy, in its order: a tool name, exact or with a `*`
/// suffix wildcard (`mcp__*`), and whether it allows the tool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionRule {
    /// The tool the rule names: exact, or a prefix ending in `*`.
    pub tool: String,
    /// Whether a matching request is allowed (`true`) or denied.
    pub allow: bool,
    /// For the rule that allows only Branchyard's delegation commands (`by`
    /// with an operation a harness may run, `allow_delegation_commands`):
    /// the `by` it names. Such a rule decides nothing else.
    pub delegation_by: Option<String>,
}

/// A branch's permission policy: its rules, first match wins, then the
/// fallback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Permissions {
    /// The rules, in the order they are tried; the first match decides.
    pub rules: Vec<PermissionRule>,
    /// What happens to a request no rule matches.
    pub fallback: Fallback,
    /// `by` as the harness runs it, when the turn's harness has it (a local
    /// delegating turn, which also finds `by` first on its `PATH`).
    pub by: Option<String>,
}

impl Permissions {
    /// A policy with no rules.
    pub fn new(fallback: Fallback) -> Permissions {
        Permissions {
            rules: Vec::new(),
            fallback,
            by: None,
        }
    }

    /// What the policy decides for `tool` without asking anyone:
    /// `Some(true)` allows it, `Some(false)` denies it, `None` would ask a
    /// person. Rules for delegation commands are passed over: they decide
    /// only a command that runs `by`.
    pub fn decide(&self, tool: &str) -> Option<bool> {
        self.rules
            .iter()
            .filter(|rule| rule.delegation_by.is_none())
            .find(|rule| matches(&rule.tool, tool))
            .map_or_else(|| self.fallback_decision(), |rule| Some(rule.allow))
    }

    /// What the first rule naming `tool` decides, when one does:
    /// `Some(true)` allows it, `Some(false)` denies it, `None` when no rule
    /// names it, whatever the fallback would do ([`Permissions::decide`]
    /// applies the fallback). Rules for delegation commands are passed over,
    /// as in `decide`.
    pub fn rule(&self, tool: &str) -> Option<bool> {
        self.rules
            .iter()
            .filter(|rule| rule.delegation_by.is_none())
            .find(|rule| matches(&rule.tool, tool))
            .map(|rule| rule.allow)
    }

    /// Whether the policy allows a shell tool named `tool` to run `by`'s
    /// delegation commands, and which `by` its rule names: the first rule
    /// matching `tool` is the delegation rule. `None` when another rule or
    /// the fallback decides first.
    pub fn delegation_by(&self, tool: &str) -> Option<&str> {
        self.rules
            .iter()
            .find(|rule| matches(&rule.tool, tool))
            .and_then(|rule| rule.delegation_by.as_deref())
    }

    /// What the policy decides for every tool of the MCP server `server`,
    /// by the names Claude Code gives them (`mcp__<server>__<tool>`): the
    /// first rule that covers the whole server (`mcp__<server>__*`,
    /// `mcp__*`, `*`), else the fallback. A rule naming one tool decides
    /// only that tool; see [`Permissions::mcp_tool_rules`]. The delegation
    /// rule allows [`DELEGATION_SERVER`]'s tools, which are the same
    /// operations as its commands.
    pub fn decide_server(&self, server: &str) -> Option<bool> {
        let prefix = format!("mcp__{server}__");
        for rule in &self.rules {
            let covers = rule
                .tool
                .strip_suffix('*')
                .is_some_and(|start| prefix.starts_with(start));
            if !covers {
                continue;
            }
            match &rule.delegation_by {
                Some(_) if server == DELEGATION_SERVER => return Some(true),
                Some(_) => continue,
                None => return Some(rule.allow),
            }
        }
        self.fallback_decision()
    }

    /// The rules that name one tool of `server` (`mcp__<server>__<tool>`),
    /// as `(tool, allow)`, in order.
    pub fn mcp_tool_rules(&self, server: &str) -> Vec<(String, bool)> {
        let prefix = format!("mcp__{server}__");
        self.rules
            .iter()
            .filter(|rule| rule.delegation_by.is_none() && !rule.tool.ends_with('*'))
            .filter_map(|rule| {
                rule.tool
                    .strip_prefix(&prefix)
                    .filter(|tool| !tool.is_empty())
                    .map(|tool| (tool.to_owned(), rule.allow))
            })
            .collect()
    }

    fn fallback_decision(&self) -> Option<bool> {
        match self.fallback {
            Fallback::Allow => Some(true),
            Fallback::Deny => Some(false),
            Fallback::Ask => None,
        }
    }
}

/// Whether the rule pattern `pattern` matches `tool`, as Branchyard's
/// policy matches: exactly, or by prefix with a `*` suffix.
fn matches(pattern: &str, tool: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => tool.starts_with(prefix),
        None => pattern == tool,
    }
}

/// The permission rules a provisioner wrote into a harness's own settings,
/// as recorded in the turn's `Provisioned` activity and in the home, so a
/// later turn removes the ones it no longer writes. Rules only; the file
/// holds nothing secret.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionsWritten {
    /// The settings file, relative to the home.
    pub path: String,
    /// The rules that let the harness's tools run without asking.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// The rules that refuse them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
}

impl PermissionsWritten {
    /// Whether it holds no rule.
    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(tool: &str, allow: bool) -> PermissionRule {
        PermissionRule {
            tool: tool.into(),
            allow,
            delegation_by: None,
        }
    }

    fn delegation(by: &str) -> PermissionRule {
        PermissionRule {
            tool: "*".into(),
            allow: true,
            delegation_by: Some(by.into()),
        }
    }

    #[test]
    fn the_first_matching_rule_decides_then_the_fallback() {
        let permissions = Permissions {
            rules: vec![rule("Bash", false), rule("mcp__*", true)],
            ..Permissions::new(Fallback::Ask)
        };
        assert_eq!(permissions.decide("Bash"), Some(false));
        assert_eq!(permissions.decide("mcp__docs__search"), Some(true));
        assert_eq!(permissions.decide("run_command"), None);
        assert_eq!(Permissions::new(Fallback::Allow).decide("x"), Some(true));
        assert_eq!(Permissions::new(Fallback::Deny).decide("x"), Some(false));
    }

    #[test]
    fn the_delegation_rule_decides_only_by_commands_and_branchyards_tools() {
        let permissions = Permissions {
            rules: vec![delegation("/opt/by")],
            ..Permissions::new(Fallback::Deny)
        };
        assert_eq!(permissions.decide("run_command"), Some(false));
        assert_eq!(permissions.delegation_by("run_command"), Some("/opt/by"));
        assert_eq!(permissions.decide_server(DELEGATION_SERVER), Some(true));
        assert_eq!(permissions.decide_server("docs"), Some(false));
        // An earlier denial of the shell wins over it.
        let denied = Permissions {
            rules: vec![rule("run_command", false), delegation("/opt/by")],
            ..Permissions::new(Fallback::Deny)
        };
        assert_eq!(denied.delegation_by("run_command"), None);
    }

    #[test]
    fn a_server_is_decided_by_rules_covering_all_its_tools() {
        let permissions = Permissions {
            rules: vec![
                rule("mcp__docs__search", false),
                rule("mcp__docs__*", true),
                rule("mcp__*", false),
            ],
            ..Permissions::new(Fallback::Allow)
        };
        assert_eq!(permissions.decide_server("docs"), Some(true));
        assert_eq!(permissions.decide_server("other"), Some(false));
        assert_eq!(
            permissions.mcp_tool_rules("docs"),
            [("search".to_owned(), false)]
        );
        assert!(permissions.mcp_tool_rules("other").is_empty());
    }
}
