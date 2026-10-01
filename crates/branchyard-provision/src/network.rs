//! Egress policy: which hosts a branch's harness may reach over the network
//! (`docs/egress.md`).
//!
//! A policy is `open` (no restriction, the default), `none` (nothing), or
//! an allowlist of host rules. Branchyard stores it with the branch, adds
//! the connector gateway's host for a turn with a grant, narrows it for
//! delegated children with [`narrow`], and enforces it with an allowlisting
//! proxy. Nothing here does I/O.
//!
//! A rule is
//!
//! ```text
//! HOST[:PORT]
//! ```
//!
//! where `HOST` is a DNS name (`github.com`), a wildcard over its
//! subdomains (`*.npmjs.org`, which does not match `npmjs.org` itself), an
//! IPv4 address, or an IPv6 address in brackets (`[::1]`). Without a port,
//! every port of the host is allowed.

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Whether a turn may run when its policy cannot be enforced.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Enforce {
    /// Run anyway, with the proxy's variables only, and say so: the
    /// harness's own network is not restricted.
    #[default]
    BestEffort,
    /// Refuse to start the turn.
    Required,
}

impl Enforce {
    pub fn parse(text: &str) -> Result<Enforce, String> {
        match text {
            "best_effort" | "best-effort" => Ok(Enforce::BestEffort),
            "required" => Ok(Enforce::Required),
            other => Err(format!(
                "egress enforcement {other:?} is not best_effort or required"
            )),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Enforce::BestEffort => "best_effort",
            Enforce::Required => "required",
        }
    }
}

/// A host pattern of a rule.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Host {
    /// A DNS name or an IP address, lower-case, IPv6 without brackets.
    Exact(String),
    /// `*.SUFFIX`: any name ending in `.SUFFIX`, not `SUFFIX` itself.
    Subdomains(String),
}

/// One allowed destination: a host pattern and, optionally, one port.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HostRule {
    pub host: Host,
    /// `None`: every port.
    pub port: Option<u16>,
}

impl HostRule {
    /// Parse `HOST[:PORT]`; see the module documentation.
    pub fn parse(text: &str) -> Result<HostRule, String> {
        let bad = |why: &str| format!("egress rule {text:?}: {why}");
        if text.is_empty() {
            return Err(bad("is empty"));
        }
        if text.contains("://") || text.contains('/') {
            return Err(bad("give a host and optional port, not a URL"));
        }
        if text.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(bad("must not hold spaces"));
        }
        let (host, port) = if let Some(rest) = text.strip_prefix('[') {
            let Some((inside, after)) = rest.split_once(']') else {
                return Err(bad("an IPv6 address needs a closing ]"));
            };
            let port = match after {
                "" => None,
                p => match p.strip_prefix(':') {
                    Some(p) => Some(p),
                    None => return Err(bad("only :PORT may follow an IPv6 address")),
                },
            };
            if inside.parse::<Ipv6Addr>().is_err() {
                return Err(bad("is not an IPv6 address"));
            }
            (Host::Exact(inside.to_ascii_lowercase()), port)
        } else {
            let (host, port) = match text.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (text, None),
            };
            if host.contains(':') {
                return Err(bad("put an IPv6 address in brackets, as [::1]:443"));
            }
            let host = host.to_ascii_lowercase();
            let host = host.strip_suffix('.').unwrap_or(&host).to_owned();
            match host.strip_prefix("*.") {
                Some(suffix) => {
                    check_name(suffix).map_err(|why| bad(&why))?;
                    if !suffix.contains('.') {
                        return Err(bad("a wildcard must cover a name of at least two labels, \
                                    such as *.example.com"));
                    }
                    if suffix.parse::<Ipv4Addr>().is_ok() {
                        return Err(bad("a wildcard cannot cover an IP address"));
                    }
                    (Host::Subdomains(suffix.to_owned()), port)
                }
                None if host == "*" => {
                    return Err(bad("to allow every host, use the policy \"open\""));
                }
                None if host.contains('*') => {
                    return Err(bad("a wildcard may only be a leading *. label"));
                }
                None => {
                    check_name(&host).map_err(|why| bad(&why))?;
                    (Host::Exact(host), port)
                }
            }
        };
        let port = match port {
            None => None,
            Some(p) => match p.parse::<u16>() {
                Ok(n) if n > 0 && !p.starts_with('+') => Some(n),
                _ => return Err(bad("the port must be a number from 1 to 65535")),
            },
        };
        Ok(HostRule { host, port })
    }

    /// Whether this rule allows `host` (as a client names it: a DNS name,
    /// an IP address, an IPv6 address with or without brackets) on `port`.
    pub fn allows(&self, host: &str, port: u16) -> bool {
        let Some(host) = normalize(host) else {
            return false;
        };
        if self.port.is_some_and(|p| p != port) {
            return false;
        }
        match &self.host {
            Host::Exact(name) => *name == host,
            Host::Subdomains(suffix) => host
                .strip_suffix(suffix.as_str())
                .is_some_and(|head| head.len() > 1 && head.ends_with('.')),
        }
    }

    /// Whether everything `other` allows, this rule allows too.
    pub fn covers(&self, other: &HostRule) -> bool {
        let port = match (self.port, other.port) {
            (None, _) => true,
            (Some(a), Some(b)) => a == b,
            (Some(_), None) => false,
        };
        let host = match (&self.host, &other.host) {
            (Host::Exact(a), Host::Exact(b)) => a == b,
            (Host::Exact(_), Host::Subdomains(_)) => false,
            (Host::Subdomains(suffix), Host::Exact(name)) => name.ends_with(&format!(".{suffix}")),
            (Host::Subdomains(a), Host::Subdomains(b)) => a == b || b.ends_with(&format!(".{a}")),
        };
        port && host
    }

    /// Whether the rule names an IP address, or `localhost`, by itself:
    /// such a rule may reach a loopback address, which a rule naming
    /// another host may not (a name that resolves to loopback is refused).
    pub fn names_address(&self) -> bool {
        match &self.host {
            Host::Exact(name) => {
                name == "localhost"
                    || name.parse::<Ipv4Addr>().is_ok()
                    || name.parse::<Ipv6Addr>().is_ok()
            }
            Host::Subdomains(_) => false,
        }
    }
}

/// A client's host lower-cased, without a trailing dot or IPv6 brackets;
/// `None` when it holds characters no host name has.
pub fn normalize(host: &str) -> Option<String> {
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    let ok = !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'_'));
    ok.then_some(host)
}

/// A DNS name or IPv4 address: labels of letters, digits, `-` and `_`,
/// each 1 to 63 characters, not starting or ending with `-`; at most 253
/// in all.
fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 253 {
        return Err("needs a host name of 1 to 253 characters".into());
    }
    for label in name.split('.') {
        let ok = !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !ok {
            return Err(format!(
                "{label:?} is not a host name label (letters, digits, '-' and '_')"
            ));
        }
    }
    Ok(())
}

impl fmt::Display for HostRule {
    /// The form [`HostRule::parse`] reads back.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.host {
            Host::Exact(name) if name.contains(':') => write!(f, "[{name}]")?,
            Host::Exact(name) => f.write_str(name)?,
            Host::Subdomains(suffix) => write!(f, "*.{suffix}")?,
        }
        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }
        Ok(())
    }
}

/// A branch's egress policy. On the wire and in files: `"open"`, `"none"`,
/// or `{"allow": ["github.com", "*.npmjs.org:443"], "enforce": "required"}`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Network {
    /// `None`: open, every host. `Some`: only these.
    pub allow: Option<Vec<HostRule>>,
    pub enforce: Enforce,
}

impl Network {
    /// No restriction.
    pub fn open() -> Network {
        Network::default()
    }

    /// No host at all.
    pub fn none() -> Network {
        Network {
            allow: Some(Vec::new()),
            enforce: Enforce::BestEffort,
        }
    }

    /// Only the hosts of `rules`.
    pub fn allow(rules: Vec<HostRule>) -> Network {
        Network {
            allow: Some(rules),
            enforce: Enforce::BestEffort,
        }
    }

    pub fn with_enforce(mut self, enforce: Enforce) -> Network {
        self.enforce = enforce;
        self
    }

    /// Parse the `--network` form: `open`, `none`, or rules separated by
    /// commas, such as `github.com,*.npmjs.org:443`.
    pub fn parse_flag(text: &str) -> Result<Network, String> {
        match text.trim() {
            "open" => Ok(Network::open()),
            "none" => Ok(Network::none()),
            "" => Err("give open, none, or hosts such as github.com,*.npmjs.org:443".into()),
            list => list
                .split(',')
                .map(|rule| HostRule::parse(rule.trim()))
                .collect::<Result<Vec<_>, _>>()
                .map(Network::allow),
        }
    }

    /// From a list of rules in their text form.
    pub fn from_rules(rules: &[String], enforce: Enforce) -> Result<Network, String> {
        let allow = rules
            .iter()
            .map(|rule| HostRule::parse(rule))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Network {
            allow: Some(allow),
            enforce,
        })
    }

    /// Whether every host is allowed.
    pub fn is_open(&self) -> bool {
        self.allow.is_none()
    }

    /// The first rule that allows `host` on `port`; `None` when no rule
    /// does. Not meaningful for an open policy, which allows everything.
    pub fn rule_for(&self, host: &str, port: u16) -> Option<&HostRule> {
        self.allow.as_ref()?.iter().find(|r| r.allows(host, port))
    }

    /// Add a rule to a restricted policy (the connector gateway's host);
    /// an open policy stays open.
    pub fn with_rule(mut self, rule: HostRule) -> Network {
        if let Some(allow) = &mut self.allow {
            if !allow.iter().any(|r| r.covers(&rule)) {
                allow.push(rule);
            }
        }
        self
    }

    /// Refuse a policy that cannot mean anything: `open` that must be
    /// enforced.
    pub fn check(&self) -> Result<(), String> {
        if self.is_open() && self.enforce == Enforce::Required {
            return Err(
                "an open network policy has nothing to enforce; drop enforce or give \
                        allow"
                    .into(),
            );
        }
        Ok(())
    }

    /// The rules in their text form; empty for `none` and for `open`.
    pub fn rules(&self) -> Vec<String> {
        self.allow
            .iter()
            .flatten()
            .map(ToString::to_string)
            .collect()
    }
}

impl fmt::Display for Network {
    /// One line for people: `open`, `none`, or the rules, with
    /// `(required)` when the policy must be enforced.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.allow {
            None => f.write_str("open")?,
            Some(rules) if rules.is_empty() => f.write_str("none")?,
            Some(_) => f.write_str(&self.rules().join(", "))?,
        }
        if self.enforce == Enforce::Required {
            f.write_str(" (required)")?;
        }
        Ok(())
    }
}

/// A delegated child's policy: `requested` (or, without one, the parent's)
/// held within `parent`. An open parent allows anything. Under a
/// restricted parent, an open request and a rule the parent's rules do not
/// cover are refused; the stricter enforcement of the two applies.
pub fn narrow(
    requested: Option<&Network>,
    parent: Option<&Network>,
) -> Result<Option<Network>, String> {
    let Some(parent) = parent.filter(|p| !p.is_open()) else {
        return Ok(requested.cloned().or_else(|| parent.cloned()));
    };
    let Some(requested) = requested else {
        return Ok(Some(parent.clone()));
    };
    let Some(asked) = &requested.allow else {
        return Err(format!(
            "an open network is wider than the parent's ({parent})"
        ));
    };
    let held = parent.allow.as_deref().unwrap_or_default();
    if let Some(wide) = asked
        .iter()
        .find(|rule| !held.iter().any(|p| p.covers(rule)))
    {
        return Err(format!(
            "network rule {wide} is not within the parent's ({parent})"
        ));
    }
    Ok(Some(Network {
        allow: Some(asked.clone()),
        enforce: requested.enforce.max(parent.enforce),
    }))
}

impl Serialize for Network {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match (&self.allow, self.enforce) {
            (None, Enforce::BestEffort) => serializer.serialize_str("open"),
            (Some(rules), Enforce::BestEffort) if rules.is_empty() => {
                serializer.serialize_str("none")
            }
            _ => {
                let mut map = serializer.serialize_map(None)?;
                if self.allow.is_some() {
                    map.serialize_entry("allow", &self.rules())?;
                }
                if self.enforce != Enforce::BestEffort {
                    map.serialize_entry("enforce", &self.enforce)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Network {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Network, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Table {
            allow: Option<Vec<String>>,
            #[serde(default)]
            enforce: Enforce,
        }

        struct Shape;

        impl<'de> Visitor<'de> for Shape {
            type Value = Network;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("\"open\", \"none\", or a table with allow and enforce")
            }

            fn visit_str<E: de::Error>(self, text: &str) -> Result<Network, E> {
                match text {
                    "open" => Ok(Network::open()),
                    "none" => Ok(Network::none()),
                    other => Err(E::custom(format!(
                        "network {other:?} is not \"open\", \"none\" or a table with allow"
                    ))),
                }
            }

            fn visit_map<M: MapAccess<'de>>(self, map: M) -> Result<Network, M::Error> {
                let table = Table::deserialize(de::value::MapAccessDeserializer::new(map))?;
                let network = match table.allow {
                    None => Network {
                        allow: None,
                        enforce: table.enforce,
                    },
                    Some(rules) => {
                        Network::from_rules(&rules, table.enforce).map_err(de::Error::custom)?
                    }
                };
                network.check().map_err(de::Error::custom)?;
                Ok(network)
            }
        }

        deserializer.deserialize_any(Shape)
    }
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for Network {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Network".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "description": "Which hosts the harness may reach (docs/egress.md): \"open\" (the \
                            default), \"none\", or allow rules HOST[:PORT] such as github.com or \
                            *.npmjs.org:443, with enforce best_effort (the default) or required.",
            "anyOf": [
                { "type": "string", "enum": ["open", "none"] },
                {
                    "type": "object",
                    "properties": {
                        "allow": {
                            "type": "array",
                            "items": {
                                "type": "string",
                                "pattern": "^(\\*\\.)?[A-Za-z0-9_.\\-]+(:[0-9]{1,5})?$|^\\[[0-9A-Fa-f:.]+\\](:[0-9]{1,5})?$"
                            }
                        },
                        "enforce": { "type": "string", "enum": ["best_effort", "required"] }
                    },
                    "additionalProperties": false
                }
            ]
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(text: &str) -> HostRule {
        HostRule::parse(text).unwrap()
    }

    #[test]
    fn rules_parse_strictly_and_print_back() {
        for text in [
            "github.com",
            "*.npmjs.org:443",
            "127.0.0.1:8931",
            "[::1]:443",
            "[2001:db8::1]",
            "localhost",
            "registry_1.internal:5000",
        ] {
            assert_eq!(rule(text).to_string(), text);
        }
        assert_eq!(rule("GitHub.COM.").to_string(), "github.com");
        for (text, why) in [
            ("", "empty"),
            ("https://github.com", "not a URL"),
            ("github.com/path", "not a URL"),
            ("*", "\"open\""),
            ("*.com", "two labels"),
            ("a.*.com", "leading *."),
            ("git*hub.com", "leading *."),
            ("github.com:0", "1 to 65535"),
            ("github.com:65536", "1 to 65535"),
            ("github.com:+1", "1 to 65535"),
            ("github.com:", "1 to 65535"),
            ("::1", "brackets"),
            ("[::1", "closing"),
            ("[nothost]:1", "IPv6"),
            ("[::1]x", ":PORT"),
            ("a b", "spaces"),
            ("-bad.com", "label"),
            ("bad..com", "label"),
            ("*.10.0.0.1", "IP address"),
        ] {
            let error = HostRule::parse(text).unwrap_err();
            assert!(error.contains(why), "{text}: {error}");
        }
    }

    #[test]
    fn rules_match_hosts_ports_and_subdomains() {
        let any_port = rule("github.com");
        assert!(any_port.allows("github.com", 443));
        assert!(any_port.allows("GITHUB.com.", 22));
        assert!(!any_port.allows("api.github.com", 443));
        assert!(!any_port.allows("github.com.evil.net", 443));
        let wild = rule("*.npmjs.org:443");
        assert!(wild.allows("registry.npmjs.org", 443));
        assert!(wild.allows("a.b.npmjs.org", 443));
        assert!(!wild.allows("npmjs.org", 443), "the suffix itself");
        assert!(!wild.allows("evilnpmjs.org", 443));
        assert!(!wild.allows("registry.npmjs.org", 80));
        let v6 = rule("[::1]:8080");
        assert!(v6.allows("[::1]", 8080));
        assert!(v6.allows("::1", 8080));
        assert!(!rule("github.com").allows("github.com\r\nx", 443));
        assert!(rule("127.0.0.1").names_address());
        assert!(rule("localhost:80").names_address());
        assert!(!rule("github.com").names_address());
    }

    #[test]
    fn the_wire_form_round_trips_and_refuses_what_is_not_a_policy() {
        for (json, network) in [
            ("\"open\"", Network::open()),
            ("\"none\"", Network::none()),
            (
                r#"{"allow":["github.com","*.npmjs.org:443"]}"#,
                Network::allow(vec![rule("github.com"), rule("*.npmjs.org:443")]),
            ),
            (
                r#"{"allow":[],"enforce":"required"}"#,
                Network::none().with_enforce(Enforce::Required),
            ),
        ] {
            let parsed: Network = serde_json::from_str(json).unwrap();
            assert_eq!(parsed, network, "{json}");
            assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
        }
        for (json, why) in [
            ("\"closed\"", "is not \"open\""),
            (r#"{"allow":["https://x.com"]}"#, "not a URL"),
            (r#"{"allow":["x.com"],"ports":[1]}"#, "unknown field"),
            (r#"{"enforce":"required"}"#, "nothing to enforce"),
            (r#"{"allow":[],"enforce":"always"}"#, "unknown variant"),
            ("3", "\"open\", \"none\""),
        ] {
            let error = serde_json::from_str::<Network>(json)
                .unwrap_err()
                .to_string();
            assert!(error.contains(why), "{json}: {error}");
        }
    }

    #[test]
    fn the_flag_form() {
        assert_eq!(Network::parse_flag("open").unwrap(), Network::open());
        assert_eq!(Network::parse_flag("none").unwrap(), Network::none());
        let two = Network::parse_flag("github.com, *.npmjs.org:443").unwrap();
        assert_eq!(two.rules(), ["github.com", "*.npmjs.org:443"]);
        assert_eq!(two.to_string(), "github.com, *.npmjs.org:443");
        assert!(Network::parse_flag("").is_err());
        assert!(Network::parse_flag("github.com,").is_err());
        let required = Network::none().with_enforce(Enforce::Required);
        assert_eq!(required.to_string(), "none (required)");
    }

    #[test]
    fn a_rule_is_added_to_a_restricted_policy_only() {
        let gateway = rule("127.0.0.1:8931");
        assert!(Network::open().with_rule(gateway.clone()).is_open());
        let none = Network::none().with_rule(gateway.clone());
        assert_eq!(none.rules(), ["127.0.0.1:8931"]);
        assert_eq!(none.clone().with_rule(gateway).rules().len(), 1);
    }

    #[test]
    fn a_child_is_never_wider_than_its_parent() {
        let parent = Network::from_rules(
            &["*.example.com".into(), "github.com:443".into()],
            Enforce::BestEffort,
        )
        .unwrap();
        // Nothing asked: the parent's.
        assert_eq!(narrow(None, Some(&parent)).unwrap(), Some(parent.clone()));
        // An open parent (or none at all) allows anything.
        let open = Network::open();
        assert_eq!(narrow(Some(&open), None).unwrap(), Some(open.clone()));
        assert_eq!(
            narrow(Some(&parent), Some(&open)).unwrap(),
            Some(parent.clone())
        );
        assert_eq!(narrow(None, None).unwrap(), None);
        // Within: narrower hosts, ports and subdomains.
        for within in [
            "api.example.com",
            "*.api.example.com:443",
            "*.example.com",
            "github.com:443",
        ] {
            let child = Network::parse_flag(within).unwrap();
            assert_eq!(narrow(Some(&child), Some(&parent)).unwrap(), Some(child));
        }
        let none = Network::none();
        assert_eq!(narrow(Some(&none), Some(&parent)).unwrap(), Some(none));
        // Wider: refused, naming the rule.
        for (wider, why) in [
            ("example.com", "example.com"),
            ("github.com", "github.com"),
            ("github.com:80", "github.com:80"),
            ("*.com.example.org", "*.com.example.org"),
            ("open", "open network"),
        ] {
            let child = Network::parse_flag(wider).unwrap();
            let error = narrow(Some(&child), Some(&parent)).unwrap_err();
            assert!(error.contains(why), "{wider}: {error}");
        }
        // The stricter enforcement wins.
        let required = parent.clone().with_enforce(Enforce::Required);
        let child = Network::parse_flag("github.com:443").unwrap();
        assert_eq!(
            narrow(Some(&child), Some(&required))
                .unwrap()
                .unwrap()
                .enforce,
            Enforce::Required
        );
    }
}
