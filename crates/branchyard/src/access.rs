//! One scope: what a turn may reach, computed once and carried in its
//! token (`docs/model-gateway.md#one-scope`).
//!
//! A turn's access has three sources, and is their intersection:
//!
//! - **The seat's ceiling.** A delegated child's connectors, models and
//!   network are narrowed to its parent's when it is spawned or sent
//!   (`crate::delegation`, `crate::run`), and a rig's seats are checked
//!   against their parent seats when the rig is planned. What is stored
//!   with the branch is already within it.
//! - **The person's ceiling.** A [`Ceiling`] per subject, set on the yard
//!   ([`crate::Yard::use_ceilings`]; a server's `ceilings`), caps every
//!   branch acting for that person, whatever its request asked for.
//! - **What the person approved.** The branch's stored provisioning is the
//!   person's approval for its session: what they asked for with `by run`
//!   or a request, a send that changed it, or a plan they approved.
//!
//! [`scoped`] applies the person's ceiling to the stored request at the
//! start of every turn, so the connector grant, the model access and the
//! network policy the turn runs under, and the claims its token carries,
//! are the same intersection. When the ceiling narrows anything, the turn
//! records an [`AccessActivity`] saying what.

use branchyard_provision::connectors::GrantEntry;
use branchyard_provision::models::ModelAccess;
use branchyard_provision::network::Network;
use serde::{Deserialize, Serialize};

use crate::state::Record;
use crate::Yard;

/// A person's ceiling: the most any branch acting for them may have.
/// Unset parts leave that scope as requested.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ceiling {
    /// The widest connector grant, in the grant's form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connectors: Option<Vec<GrantEntry>>,
    /// The models the person's branches may call through the model
    /// gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<ModelAccess>,
    /// The widest network policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<Network>,
}

/// What a person's ceiling did to a turn's access, recorded when it
/// narrowed something.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccessActivity {
    /// Whose ceiling.
    pub subject: String,
    /// One line per scope it narrowed: what was asked, what remains.
    pub narrowed: Vec<String>,
}

impl AccessActivity {
    /// One line for people, as `by log` prints it.
    pub fn describe(&self) -> String {
        format!(
            "access narrowed by {}'s ceiling: {}",
            self.subject,
            self.narrowed.join("; ")
        )
    }
}

/// `record` with the person's ceiling applied to its provisioning, and
/// what that narrowed, if anything. Without a ceiling for the branch's
/// subject, `record` as it is.
pub(crate) fn scoped(yard: &Yard, record: &Record) -> (Record, Option<AccessActivity>) {
    let subject = match &record.actor {
        Some(actor) => actor.subject.clone(),
        None => match yard.connectors() {
            Some(gateway) => gateway.subject.clone(),
            None => crate::connectors::local_subject(),
        },
    };
    let Some(ceiling) = yard.ceiling(&subject) else {
        return (record.clone(), None);
    };
    let mut scoped = record.clone();
    let asked = record.provision.clone().unwrap_or_default();
    let mut spec = asked.clone();
    let mut narrowed = Vec::new();
    if let Some(widest) = &ceiling.connectors {
        spec.connectors = branchyard_provision::connectors::intersect(&asked.connectors, widest);
        if spec.connectors != asked.connectors {
            narrowed.push(format!(
                "connectors {} to {}",
                describe_grant(&asked.connectors),
                describe_grant(&spec.connectors)
            ));
        }
    }
    if let (Some(widest), Some(models)) = (&ceiling.models, &asked.models) {
        let within = branchyard_provision::models::intersect(models, widest);
        if &within != models {
            narrowed.push(format!("models {models} to {within}"));
        }
        spec.models = Some(within);
    }
    if let Some(widest) = &ceiling.network {
        spec.network = branchyard_provision::network::intersect(asked.network.as_ref(), widest);
        if spec.network != asked.network {
            let show =
                |n: &Option<Network>| n.as_ref().map_or("open".to_owned(), |n| n.to_string());
            narrowed.push(format!(
                "network {} to {}",
                show(&asked.network),
                show(&spec.network)
            ));
        }
    }
    scoped.provision = match spec.is_empty() {
        true => None,
        false => Some(spec),
    };
    let activity = (!narrowed.is_empty()).then_some(AccessActivity { subject, narrowed });
    (scoped, activity)
}

fn describe_grant(grant: &[GrantEntry]) -> String {
    match grant.is_empty() {
        true => "none".to_owned(),
        false => branchyard_provision::connectors::describe(grant),
    }
}

/// The turn's network policy as its token carries it: the policy, and a
/// digest a gateway can compare without parsing it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkScope {
    /// `open`, `none`, or the rules, as `by show` prints them.
    pub policy: String,
    /// The rules; absent for an open policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
    /// `best_effort` or `required`.
    pub enforce: String,
    /// `blake3:<hex>` over the policy's wire form.
    pub digest: String,
}

impl NetworkScope {
    pub fn of(network: Option<&Network>) -> NetworkScope {
        let open = Network::open();
        let network = network.unwrap_or(&open);
        let wire = serde_json::to_string(network).unwrap_or_default();
        NetworkScope {
            policy: match network.is_open() {
                true => "open".to_owned(),
                false => network.to_string(),
            },
            allow: (!network.is_open()).then(|| network.rules()),
            enforce: network.enforce.as_str().to_owned(),
            digest: format!("blake3:{}", blake3::hash(wire.as_bytes()).to_hex()),
        }
    }
}

/// What the turn's branch may delegate, as its token carries it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationScope {
    /// The branch's depth: 0 for one a person started.
    pub depth: u32,
    /// Levels of descendants it may still create; 0 when it may not
    /// delegate.
    pub max_depth: u32,
    pub max_children: u32,
    /// Harnesses its children may run; empty: its own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub harnesses: Vec<String>,
}

/// The scopes a turn's token carries besides its connector grant.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TokenScopes {
    pub models: Option<Vec<String>>,
    pub network: Option<NetworkScope>,
    pub delegation: Option<DelegationScope>,
}

impl TokenScopes {
    /// The scopes of `record`, already [`scoped`].
    pub fn of(record: &Record) -> TokenScopes {
        let spec = record.provision.as_ref();
        TokenScopes {
            models: spec
                .and_then(|p| p.models.as_ref())
                .map(|m| m.allow.clone()),
            network: Some(NetworkScope::of(spec.and_then(|p| p.network.as_ref()))),
            delegation: Some(match &record.grant {
                Some(grant) => DelegationScope {
                    depth: record.info.depth,
                    max_depth: grant.envelope.max_depth,
                    max_children: grant.envelope.max_children,
                    harnesses: grant.envelope.harnesses.clone(),
                },
                None => DelegationScope {
                    depth: record.info.depth,
                    max_depth: 0,
                    max_children: 0,
                    harnesses: Vec::new(),
                },
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_network_digest_follows_the_policy() {
        let open = NetworkScope::of(None);
        assert_eq!(open.policy, "open");
        assert_eq!(open.allow, None);
        assert_eq!(open, NetworkScope::of(Some(&Network::open())));
        let none = NetworkScope::of(Some(&Network::none()));
        assert_eq!(none.policy, "none");
        assert_eq!(none.allow, Some(Vec::new()));
        assert_ne!(open.digest, none.digest);
        let github = Network::parse_flag("github.com:443").unwrap();
        let scope = NetworkScope::of(Some(&github));
        assert_eq!(
            scope.allow.as_deref(),
            Some(&["github.com:443".to_owned()][..])
        );
        assert!(scope.digest.starts_with("blake3:"));
        assert_eq!(scope, NetworkScope::of(Some(&github.clone())));
    }
}
