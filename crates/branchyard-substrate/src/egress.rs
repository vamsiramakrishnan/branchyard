use branchyard_sandbox::ProviderError;

use crate::pb;

/// The explicit outbound network policy applied to every actor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum EgressPolicy {
    /// Deny all outbound destinations.
    #[default]
    Deny,
    /// Permit exact DNS names or a wildcard in the complete leftmost label.
    AllowHosts(Vec<String>),
    /// Permit all outbound destinations.
    Unrestricted,
}

impl EgressPolicy {
    /// Validate hostname patterns before contacting Substrate.
    pub fn check(&self) -> Result<(), ProviderError> {
        if let Self::AllowHosts(hosts) = self {
            if hosts.is_empty() || hosts.len() > 256 || hosts.iter().any(|host| !valid_host(host)) {
                return Err(ProviderError::Invalid(
                    "Substrate egress needs 1-256 lowercase DNS names, optionally prefixed by *."
                        .into(),
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn rules(&self) -> Vec<pb::EgressRule> {
        match self {
            Self::Deny => Vec::new(),
            Self::AllowHosts(hosts) => vec![pb::EgressRule {
                hostnames: Some(pb::HostnameRule {
                    patterns: hosts.clone(),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            Self::Unrestricted => vec![pb::EgressRule {
                all: Some(()),
                ..Default::default()
            }],
        }
    }
}

fn valid_host(host: &str) -> bool {
    let host = host.strip_prefix("*.").unwrap_or(host);
    !host.is_empty()
        && host.len() <= 253
        && host.parse::<std::net::IpAddr>().is_err()
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_rejects_non_dns_destinations() {
        for host in [
            "",
            "HTTPS://api.example",
            "api.example:443",
            "127.0.0.1",
            "a..b",
            "a.*.example",
            "a.example.",
            "-a.example",
        ] {
            assert!(
                EgressPolicy::AllowHosts(vec![host.into()]).check().is_err(),
                "{host}"
            );
        }
        assert!(
            EgressPolicy::AllowHosts(vec!["api.example".into(), "*.example.com".into()])
                .check()
                .is_ok()
        );
        assert!(EgressPolicy::Deny.rules().is_empty());
        assert_eq!(EgressPolicy::Unrestricted.rules()[0].all, Some(()));
    }
}
