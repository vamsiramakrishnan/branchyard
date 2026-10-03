//! Approval policy: whether a tool, or a connector operation, is allowed,
//! asked about, blocked or staged (`docs/effects.md#approvals`).
//!
//! A decision comes from layers, in order: an administrator's locked
//! policy, the seat's or rig's, the person's, then the permission preset.
//! The first of the seat, person and preset layers with a matching rule (or
//! a class setting) decides; without one, the effect class's default
//! does. A deletion is then asked about at least. Last, the administrator's
//! policy is a floor nothing below it can loosen: the result is the
//! stricter of the two, and only an administrator's `deletion` setting
//! changes how deletions are treated.
//!
//! Strictness, loosest first: `allow`, `stage`, `ask`, `block`. A staged
//! call does nothing real until it is approved, but it may leave a draft
//! upstream, so `ask` (nothing happens before an answer) is stricter.
//!
//! A rule's pattern names a tool (`Bash`, `mcp__*`) or, with a colon, a
//! connector operation (`github:issues.create`, `gmail:*`, `*:*.delete`).
//! Both sides are globs: `*` matches any run of characters, `?` one. The
//! most specific matching pattern in a layer wins, the one with the most
//! literal characters; between equally specific ones, the stricter. An
//! operation pattern may leave out the service prefix, as grant globs do
//! (`issues.*` matches `github.issues.list`). Nothing here does I/O.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::connectors::glob_match;

/// What an effect does to the world, as the operation that causes it
/// declares (Anvil's `effect.class`); never guessed by a harness.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    /// Changes nothing: lists, searches, fetches.
    Read,
    /// The upstream offers a true inverse.
    Reversible,
    /// The upstream offers an action that cancels the effect, leaving a
    /// trace.
    Compensable,
    /// No undo exists. What an operation that declares nothing is taken to
    /// be.
    Irreversible,
}

impl EffectClass {
    pub const ALL: [EffectClass; 4] = [
        EffectClass::Read,
        EffectClass::Reversible,
        EffectClass::Compensable,
        EffectClass::Irreversible,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            EffectClass::Read => "read",
            EffectClass::Reversible => "reversible",
            EffectClass::Compensable => "compensable",
            EffectClass::Irreversible => "irreversible",
        }
    }

    pub fn parse(text: &str) -> Result<EffectClass, String> {
        EffectClass::ALL
            .into_iter()
            .find(|c| c.as_str() == text)
            .ok_or_else(|| {
                format!("{text:?} is not an effect class; use read, reversible, compensable or irreversible")
            })
    }

    /// The decision when no policy says otherwise.
    pub fn default_approval(&self) -> Approval {
        match self {
            EffectClass::Read | EffectClass::Reversible => Approval::Allow,
            EffectClass::Compensable => Approval::Ask,
            EffectClass::Irreversible => Approval::Stage,
        }
    }
}

impl fmt::Display for EffectClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a tool or an operation resolves to.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    /// Done without asking.
    Allow,
    /// Done as a draft (or held in the outbox) until a person approves it.
    Stage,
    /// A person is asked first; the turn waits within its budget.
    Ask,
    /// Refused.
    Block,
}

impl Approval {
    pub const ALL: [Approval; 4] = [
        Approval::Allow,
        Approval::Stage,
        Approval::Ask,
        Approval::Block,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Approval::Allow => "allow",
            Approval::Stage => "stage",
            Approval::Ask => "ask",
            Approval::Block => "block",
        }
    }

    pub fn parse(text: &str) -> Result<Approval, String> {
        Approval::ALL
            .into_iter()
            .find(|a| a.as_str() == text)
            .ok_or_else(|| format!("{text:?} is not an approval; use allow, ask, block or stage"))
    }

    /// Loosest first: allow, stage, ask, block.
    pub fn strictness(&self) -> u8 {
        match self {
            Approval::Allow => 0,
            Approval::Stage => 1,
            Approval::Ask => 2,
            Approval::Block => 3,
        }
    }

    /// The stricter of the two.
    pub fn stricter(self, other: Approval) -> Approval {
        match other.strictness() > self.strictness() {
            true => other,
            false => self,
        }
    }
}

impl fmt::Display for Approval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One layer's policy.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalPolicy {
    /// Pattern to decision: a tool name glob, or `connector:operation`
    /// with globs on both sides.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rules: BTreeMap<String, Approval>,
    /// A decision per effect class, for operations no rule matches.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub classes: BTreeMap<EffectClass, Approval>,
    /// How deletions are treated. Only an administrator's is honoured:
    /// otherwise every deletion is asked about at least.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletion: Option<Approval>,
    /// A delegating parent's policy, which this one may only tighten: the
    /// result is the stricter of the two. Set by narrowing, never by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within: Option<Box<ApprovalPolicy>>,
}

/// What is being decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subject<'a> {
    /// A harness's tool, by the name it gives it.
    Tool(&'a str),
    /// A connector operation: the connector id, the operation (the
    /// gateway's tool name, and the AIR operation id when declared), its
    /// class and whether it deletes.
    Operation {
        connector: &'a str,
        operation: &'a str,
        alias: Option<&'a str>,
        class: EffectClass,
        deletion: bool,
    },
}

/// Which layer decided.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    /// An administrator's locked policy.
    Admin,
    /// The seat's or rig's (a delegating parent's included).
    Seat,
    /// The person's.
    Person,
    /// The permission preset's.
    Preset,
    /// The effect class's default.
    Default,
    /// A deletion, asked about whatever the policy said.
    Deletion,
}

impl Layer {
    pub fn as_str(&self) -> &'static str {
        match self {
            Layer::Admin => "admin",
            Layer::Seat => "seat",
            Layer::Person => "person",
            Layer::Preset => "preset",
            Layer::Default => "default",
            Layer::Deletion => "deletion",
        }
    }
}

/// A decision and what made it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolved {
    pub approval: Approval,
    pub layer: Layer,
    /// The rule's pattern, or `class:<class>`, when one decided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
}

impl Resolved {
    /// One line for people: `ask (admin: gmail:*)`.
    pub fn describe(&self) -> String {
        match &self.rule {
            Some(rule) => format!("{} ({}: {rule})", self.approval, self.layer.as_str()),
            None => format!("{} ({})", self.approval, self.layer.as_str()),
        }
    }
}

/// The layers a decision is made from. Any may be absent.
#[derive(Clone, Copy, Debug, Default)]
pub struct Layers<'a> {
    pub admin: Option<&'a ApprovalPolicy>,
    pub seat: Option<&'a ApprovalPolicy>,
    pub person: Option<&'a ApprovalPolicy>,
    pub preset: Option<&'a ApprovalPolicy>,
}

/// Literal characters in a pattern: how specific it is.
fn specificity(pattern: &str) -> usize {
    pattern.chars().filter(|c| *c != '*' && *c != '?').count()
}

/// Whether an operation glob matches `operation`, with or without its
/// service prefix.
fn operation_matches(glob: &str, operation: &str) -> bool {
    if glob_match(glob, operation) {
        return true;
    }
    match operation.split_once('.') {
        Some((_, rest)) => glob_match(glob, rest),
        None => false,
    }
}

fn pattern_matches(pattern: &str, subject: &Subject<'_>) -> bool {
    match (subject, pattern.split_once(':')) {
        (Subject::Tool(tool), None) => glob_match(pattern, tool),
        (Subject::Tool(_), Some(_)) => false,
        (Subject::Operation { .. }, None) => false,
        (
            Subject::Operation {
                connector,
                operation,
                alias,
                ..
            },
            Some((c, o)),
        ) => {
            glob_match(c, connector)
                && (operation_matches(o, operation)
                    || alias.is_some_and(|alias| operation_matches(o, alias)))
        }
    }
}

impl ApprovalPolicy {
    /// Whether nothing is set.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
            && self.classes.is_empty()
            && self.deletion.is_none()
            && self.within.is_none()
    }

    /// A policy from its written form: rules and classes as strings, as a
    /// TOML or JSON file gives them; checked.
    pub fn from_strings(
        rules: BTreeMap<String, String>,
        classes: BTreeMap<String, String>,
        deletion: Option<String>,
    ) -> Result<ApprovalPolicy, String> {
        let policy = ApprovalPolicy {
            rules: rules
                .into_iter()
                .map(|(pattern, approval)| Approval::parse(&approval).map(|a| (pattern, a)))
                .collect::<Result<_, _>>()?,
            classes: classes
                .into_iter()
                .map(|(class, approval)| {
                    Ok((EffectClass::parse(&class)?, Approval::parse(&approval)?))
                })
                .collect::<Result<_, String>>()?,
            deletion: deletion.map(|d| Approval::parse(&d)).transpose()?,
            within: None,
        };
        policy.check()?;
        Ok(policy)
    }

    /// Refuse a pattern that cannot match anything: empty, or with an
    /// empty side.
    pub fn check(&self) -> Result<(), String> {
        for pattern in self.rules.keys() {
            let empty = match pattern.split_once(':') {
                Some((c, o)) => c.is_empty() || o.is_empty(),
                None => pattern.is_empty(),
            };
            if empty {
                return Err(format!(
                    "{pattern:?} is not an approval pattern: name a tool (Bash) or a connector \
                     operation (github:issues.create)"
                ));
            }
        }
        if let Some(within) = &self.within {
            within.check()?;
        }
        Ok(())
    }

    /// The decision this layer's own rules or class settings make, if any.
    fn own(&self, subject: &Subject<'_>) -> Option<(Approval, String)> {
        let best = self
            .rules
            .iter()
            .filter(|(pattern, _)| pattern_matches(pattern, subject))
            .max_by(|(a, x), (b, y)| {
                specificity(a)
                    .cmp(&specificity(b))
                    .then(x.strictness().cmp(&y.strictness()))
                    // Deterministic between equal patterns' decisions.
                    .then(b.cmp(a))
            });
        if let Some((pattern, approval)) = best {
            return Some((*approval, pattern.clone()));
        }
        match subject {
            Subject::Operation { class, .. } => self
                .classes
                .get(class)
                .map(|approval| (*approval, format!("class:{class}"))),
            Subject::Tool(_) => None,
        }
    }

    /// This layer's decision, held to the policies it is within.
    fn decide(&self, subject: &Subject<'_>) -> Option<(Approval, String)> {
        let own = self.own(subject);
        let Some(within) = &self.within else {
            return own;
        };
        match (own, within.decide(subject)) {
            (Some(own), Some(outer)) if outer.0.strictness() > own.0.strictness() => Some(outer),
            (Some(own), _) => Some(own),
            (None, outer) => outer,
        }
    }

    /// The deletion setting, this layer's or the stricter of the policies
    /// it is within.
    fn deletion_setting(&self) -> Option<Approval> {
        let outer = self.within.as_ref().and_then(|w| w.deletion_setting());
        match (self.deletion, outer) {
            (Some(a), Some(b)) => Some(a.stricter(b)),
            (a, b) => a.or(b),
        }
    }
}

/// Resolve `subject` through `layers`. For a tool no layer names, `None`:
/// the turn's permission policy decides it as before.
pub fn resolve(layers: &Layers<'_>, subject: &Subject<'_>) -> Option<Resolved> {
    let lower = [
        (layers.seat, Layer::Seat),
        (layers.person, Layer::Person),
        (layers.preset, Layer::Preset),
    ]
    .into_iter()
    .find_map(|(policy, layer)| {
        policy
            .and_then(|p| p.decide(subject))
            .map(|(approval, rule)| Resolved {
                approval,
                layer,
                rule: Some(rule),
            })
    });
    let mut resolved = match (lower, subject) {
        (Some(resolved), _) => Some(resolved),
        (None, Subject::Operation { class, .. }) => Some(Resolved {
            approval: class.default_approval(),
            layer: Layer::Default,
            rule: None,
        }),
        (None, Subject::Tool(_)) => None,
    };
    let deletion = matches!(subject, Subject::Operation { deletion: true, .. });
    let admin_deletion = layers.admin.and_then(|a| a.deletion_setting());
    if deletion {
        let current = resolved.as_ref().map(|r| r.approval);
        match admin_deletion {
            // An administrator decides how deletions are treated.
            Some(approval) => {
                resolved = Some(Resolved {
                    approval,
                    layer: Layer::Admin,
                    rule: Some("deletion".into()),
                })
            }
            None if current.is_none_or(|c| c.strictness() < Approval::Ask.strictness()) => {
                resolved = Some(Resolved {
                    approval: Approval::Ask,
                    layer: Layer::Deletion,
                    rule: None,
                })
            }
            None => {}
        }
    }
    // The administrator's floor.
    if let Some((approval, rule)) = layers.admin.and_then(|a| a.decide(subject)) {
        let looser = resolved
            .as_ref()
            .is_none_or(|r| r.approval.strictness() < approval.strictness());
        if looser {
            resolved = Some(Resolved {
                approval,
                layer: Layer::Admin,
                rule: Some(rule),
            });
        }
    }
    resolved
}

/// A delegated child's policy: what it asks for (or, without one, none of
/// its own) within its parent's. It may only be stricter.
pub fn narrow(
    requested: Option<&ApprovalPolicy>,
    parent: Option<&ApprovalPolicy>,
) -> Option<ApprovalPolicy> {
    match (requested, parent) {
        (None, None) => None,
        (Some(own), None) => Some(own.clone()),
        (own, Some(parent)) => {
            let mut own = own.cloned().unwrap_or_default();
            // Already within this parent (a send that kept it).
            if own.within.as_deref() == Some(parent) || &own == parent {
                return Some(own);
            }
            own.within = Some(Box::new(match own.within.take() {
                // Keep the chain: the parent's own chain is the outer one.
                Some(inner) if *inner != *parent => {
                    let mut chained = *inner;
                    chained.within = Some(Box::new(parent.clone()));
                    chained
                }
                _ => parent.clone(),
            }));
            Some(own)
        }
    }
}

/// Whether an operation name says it deletes: a word of its last segment
/// (split at `_`, `-` and camelCase) is delete, remove, destroy, purge,
/// erase, trash or unlink.
pub fn is_deletion_name(operation: &str) -> bool {
    const VERBS: [&str; 7] = [
        "delete", "remove", "destroy", "purge", "erase", "trash", "unlink",
    ];
    let last = operation
        .rsplit(['.', '/', ':'])
        .next()
        .unwrap_or(operation);
    let mut words = Vec::new();
    let mut word = String::new();
    for c in last.chars() {
        if c == '_' || c == '-' || c.is_ascii_uppercase() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        }
        if c != '_' && c != '-' {
            word.push(c.to_ascii_lowercase());
        }
    }
    words.push(word);
    words.iter().any(|w| VERBS.contains(&w.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(rules: &[(&str, Approval)]) -> ApprovalPolicy {
        ApprovalPolicy {
            rules: rules.iter().map(|(p, a)| ((*p).to_owned(), *a)).collect(),
            ..ApprovalPolicy::default()
        }
    }

    fn op<'a>(connector: &'a str, operation: &'a str, class: EffectClass) -> Subject<'a> {
        Subject::Operation {
            connector,
            operation,
            alias: None,
            class,
            deletion: is_deletion_name(operation),
        }
    }

    fn approval(layers: &Layers<'_>, subject: &Subject<'_>) -> Approval {
        resolve(layers, subject).unwrap().approval
    }

    #[test]
    fn classes_have_defaults_and_tools_are_left_alone() {
        let none = Layers::default();
        assert_eq!(
            approval(&none, &op("github", "issues.list", EffectClass::Read)),
            Approval::Allow
        );
        assert_eq!(
            approval(&none, &op("slack", "chat.post", EffectClass::Reversible)),
            Approval::Allow
        );
        assert_eq!(
            approval(
                &none,
                &op("github", "issues.close", EffectClass::Compensable)
            ),
            Approval::Ask
        );
        assert_eq!(
            approval(
                &none,
                &op("gmail", "messages.send", EffectClass::Irreversible)
            ),
            Approval::Stage
        );
        assert_eq!(resolve(&none, &Subject::Tool("Bash")), None);
    }

    #[test]
    fn layers_resolve_in_order_and_the_admin_is_a_floor() {
        let admin = policy(&[("gmail:*", Approval::Ask), ("Bash", Approval::Block)]);
        let seat = policy(&[("gmail:messages.send", Approval::Allow)]);
        let person = policy(&[
            ("gmail:*", Approval::Allow),
            ("github:issues.*", Approval::Block),
            ("Bash", Approval::Allow),
        ]);
        let preset = policy(&[("github:*", Approval::Allow)]);
        let layers = Layers {
            admin: Some(&admin),
            seat: Some(&seat),
            person: Some(&person),
            preset: Some(&preset),
        };
        // The seat's allow is loosened past the admin's ask: the admin wins.
        let send = op("gmail", "messages.send", EffectClass::Irreversible);
        assert_eq!(
            resolve(&layers, &send),
            Some(Resolved {
                approval: Approval::Ask,
                layer: Layer::Admin,
                rule: Some("gmail:*".into())
            })
        );
        // The person's block is stricter than the admin's floor.
        let create = op("github", "issues.create", EffectClass::Reversible);
        assert_eq!(
            resolve(&layers, &create),
            Some(Resolved {
                approval: Approval::Block,
                layer: Layer::Person,
                rule: Some("github:issues.*".into())
            })
        );
        // The seat comes before the person, the person before the preset.
        let no_seat = Layers {
            seat: None,
            ..layers
        };
        let comment = op("github", "comments.create", EffectClass::Compensable);
        assert_eq!(
            resolve(&no_seat, &comment).unwrap().layer,
            Layer::Preset,
            "no seat or person rule matches"
        );
        // Tools: an admin's block cannot be loosened by the person.
        assert_eq!(approval(&layers, &Subject::Tool("Bash")), Approval::Block);
        assert_eq!(
            resolve(&layers, &Subject::Tool("Edit")),
            None,
            "nothing names Edit"
        );
    }

    #[test]
    fn the_most_specific_pattern_wins_then_the_stricter() {
        let p = policy(&[
            ("github:*", Approval::Allow),
            ("github:issues.*", Approval::Ask),
            ("github:issues.create", Approval::Allow),
            ("*:issues.creat?", Approval::Block),
        ]);
        let layers = Layers {
            person: Some(&p),
            ..Layers::default()
        };
        let a = |o: &str| approval(&layers, &op("github", o, EffectClass::Reversible));
        assert_eq!(a("issues.create"), Approval::Allow);
        assert_eq!(a("issues.close"), Approval::Ask);
        assert_eq!(a("pulls.list"), Approval::Allow);
        // Service prefix optional, as in grants.
        assert_eq!(a("github.issues.close"), Approval::Ask);
        let tie = policy(&[("a:x*", Approval::Allow), ("a:*x", Approval::Ask)]);
        let layers = Layers {
            person: Some(&tie),
            ..Layers::default()
        };
        assert_eq!(
            approval(&layers, &op("a", "x", EffectClass::Reversible)),
            Approval::Ask
        );
    }

    #[test]
    fn deletion_always_asks_unless_an_admin_says_otherwise() {
        let person = policy(&[("slack:*", Approval::Allow)]);
        let layers = Layers {
            person: Some(&person),
            ..Layers::default()
        };
        let delete = op("slack", "messages.delete", EffectClass::Reversible);
        assert_eq!(
            resolve(&layers, &delete),
            Some(Resolved {
                approval: Approval::Ask,
                layer: Layer::Deletion,
                rule: None
            })
        );
        // A person's deletion setting is not honoured.
        let mut lax = person.clone();
        lax.deletion = Some(Approval::Allow);
        let layers = Layers {
            person: Some(&lax),
            ..Layers::default()
        };
        assert_eq!(approval(&layers, &delete), Approval::Ask);
        // A block stays a block.
        let block = policy(&[("slack:*", Approval::Block)]);
        let layers = Layers {
            person: Some(&block),
            ..Layers::default()
        };
        assert_eq!(approval(&layers, &delete), Approval::Block);
        // An administrator's is.
        let admin = ApprovalPolicy {
            deletion: Some(Approval::Allow),
            ..ApprovalPolicy::default()
        };
        let layers = Layers {
            admin: Some(&admin),
            person: Some(&person),
            ..Layers::default()
        };
        assert_eq!(approval(&layers, &delete), Approval::Allow);
        assert!(is_deletion_name("comments.delete"));
        assert!(is_deletion_name("delete_comment"));
        assert!(is_deletion_name("deleteComment"));
        assert!(is_deletion_name("files.remove"));
        assert!(!is_deletion_name("issues.list"));
        assert!(!is_deletion_name("removed_items.list"));
        assert!(!is_deletion_name("deleted"));
    }

    #[test]
    fn classes_are_set_per_layer_and_children_only_tighten() {
        let person = ApprovalPolicy {
            classes: [(EffectClass::Irreversible, Approval::Allow)].into(),
            ..ApprovalPolicy::default()
        };
        let layers = Layers {
            person: Some(&person),
            ..Layers::default()
        };
        let send = op("gmail", "messages.send", EffectClass::Irreversible);
        assert_eq!(
            resolve(&layers, &send).unwrap().rule.as_deref(),
            Some("class:irreversible")
        );
        assert_eq!(approval(&layers, &send), Approval::Allow);

        let parent = policy(&[("gmail:*", Approval::Ask)]);
        let child = policy(&[("gmail:*", Approval::Allow), ("Bash", Approval::Block)]);
        let narrowed = narrow(Some(&child), Some(&parent)).unwrap();
        let layers = Layers {
            seat: Some(&narrowed),
            ..Layers::default()
        };
        assert_eq!(approval(&layers, &send), Approval::Ask);
        assert_eq!(approval(&layers, &Subject::Tool("Bash")), Approval::Block);
        // Narrowing again with the same parent changes nothing.
        assert_eq!(
            narrow(Some(&narrowed), Some(&parent)),
            Some(narrowed.clone())
        );
        // A grandchild is within both.
        let grand = narrow(None, Some(&narrowed)).unwrap();
        let layers = Layers {
            seat: Some(&grand),
            ..Layers::default()
        };
        assert_eq!(approval(&layers, &send), Approval::Ask);
        assert_eq!(approval(&layers, &Subject::Tool("Bash")), Approval::Block);
        assert_eq!(narrow(None, None), None);
    }

    #[test]
    fn names_round_trip_and_patterns_are_checked() {
        for a in Approval::ALL {
            assert_eq!(Approval::parse(a.as_str()), Ok(a));
            assert_eq!(serde_json::to_string(&a).unwrap(), format!("\"{a}\""));
        }
        for c in EffectClass::ALL {
            assert_eq!(EffectClass::parse(c.as_str()), Ok(c));
        }
        assert!(Approval::parse("maybe").is_err());
        let json = serde_json::json!({
            "rules": {"github:issues.*": "allow", "Bash": "ask"},
            "classes": {"compensable": "allow"},
            "deletion": "ask"
        });
        let p: ApprovalPolicy = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(serde_json::to_value(&p).unwrap(), json);
        assert!(p.check().is_ok());
        assert!(policy(&[("github:", Approval::Allow)]).check().is_err());
        assert!(policy(&[("", Approval::Allow)]).check().is_err());
    }
}
