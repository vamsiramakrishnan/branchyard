//! Model access: whether a branch's harness calls its model provider
//! through Branchyard's model gateway, and which models it may call there
//! (`--model-gateway`; `docs/model-gateway.md`).
//!
//! A branch with [`ModelAccess`] gets, for each turn, a gateway that speaks
//! its provider's own API and holds the upstream key; the harness holds
//! only the turn's token, whose `by_models` claim is the access's `allow`
//! list. A pattern is a glob over model ids: `*` matches any run of
//! characters, `?` one character, so `claude-sonnet-*` allows every Sonnet
//! and `*` every model a route serves. Nothing here does I/O.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::connectors::{glob_covers, glob_match};

/// What a branch may call through the model gateway.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelAccess {
    /// Globs over model ids; `["*"]` is every model. Empty allows none.
    pub allow: Vec<String>,
}

impl Default for ModelAccess {
    /// Every model.
    fn default() -> ModelAccess {
        ModelAccess::all()
    }
}

impl ModelAccess {
    /// Every model a route serves.
    pub fn all() -> ModelAccess {
        ModelAccess {
            allow: vec!["*".to_owned()],
        }
    }

    /// The flag's form: empty or `*` for every model, else patterns
    /// separated by commas (`claude-*,gpt-5*`).
    pub fn parse_flag(text: &str) -> Result<ModelAccess, String> {
        let patterns: Vec<String> = text
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect();
        let access = match patterns.is_empty() {
            true => ModelAccess::all(),
            false => ModelAccess { allow: patterns },
        };
        access.check()?;
        Ok(access)
    }

    /// Refuse a pattern that cannot name a model.
    pub fn check(&self) -> Result<(), String> {
        for pattern in &self.allow {
            check_pattern(pattern)?;
        }
        Ok(())
    }

    /// Whether `model` is allowed.
    pub fn allows(&self, model: &str) -> bool {
        self.allow.iter().any(|p| glob_match(p, model))
    }

    /// Whether every model `other` allows, this allows too, as far as
    /// [`glob_covers`] can tell.
    pub fn covers(&self, other: &ModelAccess) -> bool {
        other
            .allow
            .iter()
            .all(|inner| self.allow.iter().any(|outer| glob_covers(outer, inner)))
    }
}

impl fmt::Display for ModelAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.allow.is_empty() {
            true => f.write_str("no model"),
            false => f.write_str(&self.allow.join(", ")),
        }
    }
}

/// A model id or glob: 1 to 256 characters, none of them whitespace or a
/// control character, a comma or a quote.
pub fn check_pattern(pattern: &str) -> Result<(), String> {
    if pattern.is_empty() || pattern.len() > 256 {
        return Err(format!(
            "model pattern {pattern:?} must have 1 to 256 characters"
        ));
    }
    if let Some(bad) = pattern
        .chars()
        .find(|c| c.is_whitespace() || c.is_control() || matches!(c, ',' | '"' | '\''))
    {
        return Err(format!("model pattern {pattern:?} may not contain {bad:?}"));
    }
    Ok(())
}

/// The models both allow, as far as [`glob_covers`] can tell: each pattern
/// of one side the other side covers. Sound, not complete: when it cannot
/// tell, it keeps neither, so the result is never wider than either side.
pub fn intersect(a: &ModelAccess, b: &ModelAccess) -> ModelAccess {
    let mut allow: Vec<String> = Vec::new();
    for x in &a.allow {
        for y in &b.allow {
            let common = match (glob_covers(y, x), glob_covers(x, y)) {
                (true, _) => Some(x),
                (false, true) => Some(y),
                (false, false) => None,
            };
            if let Some(pattern) = common {
                if !allow.contains(pattern) {
                    allow.push(pattern.clone());
                }
            }
        }
    }
    ModelAccess { allow }
}

/// A delegated child's access: `requested` (or, without one, the
/// parent's) held within `parent`. A parent off the gateway leaves the
/// child as asked; a child of a parent on the gateway stays on it, and a
/// pattern the parent's do not cover is refused by name.
pub fn narrow(
    requested: Option<&ModelAccess>,
    parent: Option<&ModelAccess>,
) -> Result<Option<ModelAccess>, String> {
    let Some(parent) = parent else {
        return Ok(requested.cloned());
    };
    let Some(requested) = requested else {
        return Ok(Some(parent.clone()));
    };
    if let Some(wide) = requested
        .allow
        .iter()
        .find(|p| !parent.allow.iter().any(|outer| glob_covers(outer, p)))
    {
        return Err(format!(
            "model pattern {wide} is not within the parent's ({parent})"
        ));
    }
    Ok(Some(requested.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(patterns: &[&str]) -> ModelAccess {
        ModelAccess {
            allow: patterns.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    #[test]
    fn flags_parse_and_patterns_match_model_ids() {
        assert_eq!(ModelAccess::parse_flag("").unwrap(), ModelAccess::all());
        assert_eq!(ModelAccess::parse_flag("*").unwrap(), ModelAccess::all());
        let two = ModelAccess::parse_flag("claude-*, gpt-5?").unwrap();
        assert_eq!(two, access(&["claude-*", "gpt-5?"]));
        assert!(two.allows("claude-sonnet-4-6"));
        assert!(two.allows("gpt-51"));
        assert!(!two.allows("gpt-5.1-codex"));
        assert!(!access(&[]).allows("claude-sonnet-4-6"));
        assert!(ModelAccess::parse_flag("a b").unwrap_err().contains("' '"));
        assert!(ModelAccess::parse_flag(&"x".repeat(300))
            .unwrap_err()
            .contains("256"));
        let wire = serde_json::to_value(&two).unwrap();
        assert_eq!(wire, serde_json::json!({"allow": ["claude-*", "gpt-5?"]}));
        assert!(
            serde_json::from_value::<ModelAccess>(serde_json::json!({"allow": [], "x": 1}))
                .is_err()
        );
    }

    #[test]
    fn children_stay_within_their_parent_and_ceilings_intersect() {
        let parent = access(&["claude-*"]);
        assert_eq!(narrow(None, Some(&parent)).unwrap(), Some(parent.clone()));
        assert_eq!(narrow(Some(&parent), None).unwrap(), Some(parent.clone()));
        assert_eq!(narrow(None, None).unwrap(), None);
        let sonnet = access(&["claude-sonnet-*"]);
        assert_eq!(
            narrow(Some(&sonnet), Some(&parent)).unwrap(),
            Some(sonnet.clone())
        );
        let error = narrow(Some(&access(&["gpt-5"])), Some(&parent)).unwrap_err();
        assert!(error.contains("gpt-5 is not within"), "{error}");
        assert!(narrow(Some(&ModelAccess::all()), Some(&parent)).is_err());
        assert!(parent.covers(&sonnet));
        assert!(!sonnet.covers(&parent));
        assert_eq!(
            intersect(&ModelAccess::all(), &parent),
            access(&["claude-*"])
        );
        assert_eq!(intersect(&sonnet, &parent), sonnet);
        assert_eq!(
            intersect(&access(&["claude-*", "gpt-5"]), &access(&["gpt-*"])),
            access(&["gpt-5"])
        );
        assert_eq!(intersect(&access(&["a*b"]), &access(&["a*c"])), access(&[]));
    }
}
