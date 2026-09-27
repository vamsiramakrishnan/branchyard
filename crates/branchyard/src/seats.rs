//! Seats: named children a branch may spawn, declared ahead of time.
//!
//! A rig (`by rig`, `docs/rigs.md`) lowers to one root branch whose
//! [`Seats`] name the seat it occupies and the seats below it. A branch in
//! a rig spawns only by seat, and only the seats its own seat
//! `delegates_to`; the seat fills the child's harness, limits, check,
//! denials, isolation and provisioning. The child's envelope is derived
//! from its seat's subtree and still narrowed by its parent's, so a seat
//! never grants more than the envelope allows.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::delegation::ChildBudget;
use crate::{harness, Envelope, Error, Provisioning};

/// A child a rig's branch may spawn by name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seat {
    /// Harness or profile ID the child runs.
    pub harness: String,
    /// The child's limits. A spawn may ask for less, never more.
    #[serde(default)]
    pub budget: ChildBudget,
    /// The child's merge check; `None` keeps its parent's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<Vec<String>>,
    /// Tool patterns the child is denied, before its parent's policy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    /// Run the child with a private home even when its parent has none.
    #[serde(default, skip_serializing_if = "is_false")]
    pub isolated: bool,
    /// What to provision for the child. `None` inherits its parent's, as a
    /// delegated child does without a seat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provision: Option<Provisioning>,
    /// Seats a child in this seat may spawn in turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delegates_to: Vec<String>,
    /// Seats above this one in the tree a branch in this seat may
    /// `escalate` to, besides its parent (which it may always escalate to).
    /// Each must name an ancestor seat.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub escalates_to: Vec<String>,
    /// Children of this seat one parent may have at once, counting finished
    /// ones until they are removed. At least 1.
    #[serde(default = "one")]
    pub instances: u32,
}

fn one() -> u32 {
    1
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The seat a branch occupies in a rig and the seats it may fill.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seats {
    /// The rig's name, for messages.
    pub rig: String,
    /// The seat this branch occupies.
    pub seat: String,
    /// Seats this branch may spawn.
    #[serde(default)]
    pub delegates_to: Vec<String>,
    /// Ancestor seats, besides its parent's, this branch's own seat may
    /// `escalate` to (its seat's `escalates_to`, by name).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub escalates_to: Vec<String>,
    /// Every seat below this branch's, by name.
    #[serde(default)]
    pub table: BTreeMap<String, Seat>,
}

impl Seats {
    /// Check that the table is a forest reachable from `delegates_to`:
    /// every name it references exists, every seat is below exactly one
    /// parent, no seat is below itself, each harness is known, and each
    /// seat's limits are positive.
    pub fn validate(&self) -> Result<(), Error> {
        let invalid = |why: String| Err(Error::Unsupported(format!("rig {}: {why}", self.rig)));
        let mut parents: BTreeMap<&str, &str> = BTreeMap::new();
        let edges = std::iter::once((self.seat.as_str(), &self.delegates_to)).chain(
            self.table
                .iter()
                .map(|(name, seat)| (name.as_str(), &seat.delegates_to)),
        );
        for (from, targets) in edges {
            for to in targets {
                if !self.table.contains_key(to) {
                    return invalid(format!(
                        "seat {from} delegates to {to}, which is not a seat"
                    ));
                }
                if let Some(other) = parents.insert(to, from) {
                    return invalid(format!(
                        "seat {to} is below both {other} and {from}; seats form a tree"
                    ));
                }
            }
        }
        if self.table.contains_key(&self.seat) {
            return invalid(format!("seat {} is below itself", self.seat));
        }
        for name in self.table.keys() {
            if !parents.contains_key(name.as_str()) {
                return invalid(format!("seat {name} is not below seat {}", self.seat));
            }
        }
        // A seat may only escalate to an ancestor seat, besides its parent
        // (always allowed, and not named here). The root seat itself has no
        // ancestor seat in this table to escalate to.
        if !self.escalates_to.is_empty() {
            return invalid(format!(
                "seat {} is the root; it has no ancestor seat to escalate to",
                self.seat
            ));
        }
        for (name, seat) in &self.table {
            for target in &seat.escalates_to {
                // The immediate parent is always allowed and is not what
                // escalates_to is for; only a seat further up counts.
                let mut ancestors = BTreeSet::new();
                let mut walk = parents
                    .get(name.as_str())
                    .and_then(|p| parents.get(*p))
                    .copied();
                while let Some(next) = walk {
                    ancestors.insert(next);
                    walk = parents.get(next).copied();
                }
                if !ancestors.contains(target.as_str()) {
                    return invalid(format!(
                        "seat {name} escalates to {target}, which is not an ancestor seat \
                         beyond its parent"
                    ));
                }
            }
        }
        // Every seat has one parent and is reachable only if there is no
        // cycle; walk down to be sure.
        let mut seen = BTreeSet::new();
        let mut queue: Vec<&str> = self.delegates_to.iter().map(String::as_str).collect();
        while let Some(name) = queue.pop() {
            if !seen.insert(name) {
                return invalid(format!("seat {name} is below itself"));
            }
            queue.extend(self.table[name].delegates_to.iter().map(String::as_str));
        }
        if seen.len() != self.table.len() {
            return invalid("the seats' delegates_to edges form a cycle".into());
        }
        for (name, seat) in &self.table {
            harness::select(Some(&seat.harness))?;
            if seat.instances == 0 {
                return invalid(format!("seat {name}'s instances must be at least 1"));
            }
            if let Some(usd) = seat.budget.max_usd.filter(|v| !(v.is_finite() && *v > 0.0)) {
                return invalid(format!("seat {name}'s max_usd must be positive, not {usd}"));
            }
            if seat.budget.max_turns == Some(0) {
                return invalid(format!("seat {name}'s max_turns must be positive"));
            }
            if let Some(m) = seat
                .budget
                .max_minutes
                .filter(|v| !(v.is_finite() && *v > 0.0))
            {
                return invalid(format!(
                    "seat {name}'s max_minutes must be positive, not {m}"
                ));
            }
        }
        Ok(())
    }

    /// Check each seat's provisioning as its spawn will: a child has a
    /// private home when its parent does, when its seat is isolated, or in
    /// a sandbox.
    pub(crate) fn check_provisioning(&self, private: bool) -> Result<(), Error> {
        let mut queue: Vec<(&str, bool)> = self
            .delegates_to
            .iter()
            .map(|name| (name.as_str(), private))
            .collect();
        while let Some((name, private)) = queue.pop() {
            let Some(seat) = self.table.get(name) else {
                continue;
            };
            let private = private || seat.isolated;
            crate::provisioning::check(seat.provision.as_ref(), private).map_err(|error| {
                Error::Unsupported(format!("rig {} seat {name}: {error}", self.rig))
            })?;
            queue.extend(seat.delegates_to.iter().map(|n| (n.as_str(), private)));
        }
        Ok(())
    }

    /// The seats a child in `seat` gets: its own edges and the seats below
    /// it.
    pub(crate) fn below(&self, seat: &str) -> Seats {
        let mut table = BTreeMap::new();
        let mut queue: Vec<&str> = self.table[seat]
            .delegates_to
            .iter()
            .map(String::as_str)
            .collect();
        while let Some(name) = queue.pop() {
            if let Some(found) = self.table.get(name) {
                if table.insert(name.to_owned(), found.clone()).is_none() {
                    queue.extend(found.delegates_to.iter().map(String::as_str));
                }
            }
        }
        Seats {
            rig: self.rig.clone(),
            seat: seat.to_owned(),
            delegates_to: self.table[seat].delegates_to.clone(),
            escalates_to: self.table[seat].escalates_to.clone(),
            table,
        }
    }

    /// The envelope these seats need: as deep as the tallest chain below,
    /// as wide as the seats' instances, and the harnesses they run.
    pub fn envelope(&self) -> Envelope {
        let mut harnesses: Vec<String> = Vec::new();
        for seat in self.table.values() {
            if !harnesses.contains(&seat.harness) {
                harnesses.push(seat.harness.clone());
            }
        }
        harnesses.sort();
        Envelope {
            max_depth: self.height(&self.delegates_to, 0),
            max_children: self
                .delegates_to
                .iter()
                .filter_map(|name| self.table.get(name))
                .map(|seat| seat.instances)
                .sum(),
            harnesses,
        }
    }

    fn height(&self, below: &[String], guard: usize) -> u32 {
        if guard > self.table.len() {
            return 0;
        }
        below
            .iter()
            .filter_map(|name| self.table.get(name))
            .map(|seat| 1 + self.height(&seat.delegates_to, guard + 1))
            .max()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat(harness: &str, below: &[&str]) -> Seat {
        Seat {
            harness: harness.into(),
            budget: ChildBudget::default(),
            check: None,
            deny: Vec::new(),
            isolated: false,
            provision: None,
            delegates_to: below.iter().map(|s| (*s).to_owned()).collect(),
            escalates_to: Vec::new(),
            instances: 1,
        }
    }

    fn seats(root: &[&str], table: &[(&str, Seat)]) -> Seats {
        Seats {
            rig: "r".into(),
            seat: "lead".into(),
            delegates_to: root.iter().map(|s| (*s).to_owned()).collect(),
            escalates_to: Vec::new(),
            table: table
                .iter()
                .map(|(n, s)| ((*n).to_owned(), s.clone()))
                .collect(),
        }
    }

    #[test]
    fn a_tree_of_seats_gives_its_envelope_and_subtrees() {
        let mut two = seat("codex", &["tester"]);
        two.instances = 2;
        let s = seats(
            &["impl", "review"],
            &[
                ("impl", two),
                ("review", seat("claude-code", &[])),
                ("tester", seat("gemini-cli", &[])),
            ],
        );
        s.validate().unwrap();
        assert_eq!(
            s.envelope(),
            Envelope {
                max_depth: 2,
                max_children: 3,
                harnesses: vec!["claude-code".into(), "codex".into(), "gemini-cli".into()],
            }
        );
        let below = s.below("impl");
        assert_eq!(below.seat, "impl");
        assert_eq!(below.delegates_to, ["tester"]);
        assert_eq!(below.table.keys().collect::<Vec<_>>(), ["tester"]);
        assert_eq!(below.envelope().max_depth, 1);
        let leaf = s.below("review");
        assert!(leaf.table.is_empty());
        assert_eq!(leaf.envelope().max_depth, 0);
    }

    #[test]
    fn seats_that_are_not_a_tree_are_refused() {
        let refused = |s: Seats, needle: &str| match s.validate() {
            Err(Error::Unsupported(why)) if why.contains(needle) => {}
            other => panic!("expected {needle:?}, got {other:?}"),
        };
        refused(seats(&["x"], &[]), "delegates to x, which is not a seat");
        refused(
            seats(
                &["a", "b"],
                &[
                    ("a", seat("codex", &["c"])),
                    ("b", seat("codex", &["c"])),
                    ("c", seat("codex", &[])),
                ],
            ),
            "below both",
        );
        refused(
            seats(
                &["a"],
                &[("a", seat("codex", &[])), ("loose", seat("codex", &[]))],
            ),
            "loose is not below",
        );
        refused(
            seats(
                &["a"],
                &[
                    ("a", seat("codex", &[])),
                    ("b", seat("codex", &["c"])),
                    ("c", seat("codex", &["b"])),
                ],
            ),
            "cycle",
        );
        match seats(&["a"], &[("a", seat("no-such-harness", &[]))]).validate() {
            Err(Error::UnknownHarness(id)) => assert_eq!(id, "no-such-harness"),
            other => panic!("{other:?}"),
        }
    }
}
