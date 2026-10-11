//! Egress policy: which hosts a branch's harness may reach over the network,
//! and, for a host it may reach, which requests (`docs/egress.md`).
//!
//! A policy is `open` (no restriction, the default), `none` (nothing), or
//! an allowlist of rules. Branchyard stores it with the branch, adds the
//! connector gateway's host for a turn with a grant, narrows it for
//! delegated children with [`narrow`], and enforces it with an allowlisting
//! proxy. Nothing here does I/O.
//!
//! A rule is
//!
//! ```text
//! [METHOD|METHOD... ]HOST[:PORT][/PATH]
//! ```
//!
//! where `HOST` is a DNS name (`github.com`), a wildcard over its
//! subdomains (`*.npmjs.org`, which does not match `npmjs.org` itself), an
//! IPv4 address, or an IPv6 address in brackets (`[::1]`). Without a port,
//! every port of the host is allowed.
//!
//! A rule with methods (`GET|HEAD api.github.com`) or a path
//! (`api.github.com/repos/*`) *inspects*: the proxy looks inside the
//! connection, terminating TLS with a certificate authority the branch's
//! harness is told to trust, and allows each request only if its method
//! and path match. A path pattern is `/`-separated segments; a segment `*`
//! matches any one segment, and a trailing `*` also matches everything
//! below it (`/repos/*` matches `/repos/x` and `/repos/x/issues`, not
//! `/repos`); the query string is never matched. A host-only rule stays a
//! tunnel the proxy does not look inside.

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

/// A path pattern of a rule: `/`-separated segments, each exact or `*`
/// (any one segment); a trailing `*` also matches every segment below it.
/// `/` alone matches only the root. The query string is never matched.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PathPattern {
    segments: Vec<String>,
}

impl PathPattern {
    /// Parse `/a/*/b` or `/a/*`; see the module documentation.
    pub fn parse(text: &str) -> Result<PathPattern, String> {
        let Some(rest) = text.strip_prefix('/') else {
            return Err("a path must start with /".into());
        };
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        let mut segments = Vec::new();
        for segment in rest.split('/') {
            if segment.is_empty() {
                if rest.is_empty() {
                    break;
                }
                return Err("a path must not hold an empty segment (//)".into());
            }
            if segment.contains('*') && segment != "*" {
                return Err(format!(
                    "{segment:?}: a wildcard is a whole segment, *, not part of one"
                ));
            }
            if segment == "." || segment == ".." {
                return Err(format!("{segment:?} is not a path segment"));
            }
            let ok = segment.bytes().all(|b| {
                b.is_ascii_alphanumeric()
                    || matches!(
                        b,
                        b'-' | b'_'
                            | b'.'
                            | b'~'
                            | b'%'
                            | b'*'
                            | b'@'
                            | b':'
                            | b'+'
                            | b'='
                            | b'!'
                            | b'$'
                            | b'&'
                            | b'\''
                            | b'('
                            | b')'
                            | b';'
                    )
            });
            if !ok {
                return Err(format!(
                    "{segment:?} holds a character a path segment may not (no spaces, commas, \
                     question marks or #)"
                ));
            }
            segments.push(segment.to_owned());
        }
        Ok(PathPattern { segments })
    }

    /// Whether `path` (origin form, a query string allowed) matches.
    pub fn matches(&self, path: &str) -> bool {
        let path = path.split(['?', '#']).next().unwrap_or_default();
        let path = path.strip_prefix('/').unwrap_or(path);
        let path = path.strip_suffix('/').unwrap_or(path);
        let segments: Vec<&str> = match path.is_empty() {
            true => Vec::new(),
            false => path.split('/').collect(),
        };
        if self.segments.len() > segments.len() {
            return false;
        }
        for (pattern, segment) in self.segments.iter().zip(&segments) {
            if pattern != "*" && pattern != segment {
                return false;
            }
        }
        segments.len() == self.segments.len()
            || self.segments.last().is_some_and(|last| last == "*")
    }

    /// Whether every path `other` matches, this pattern matches too.
    pub fn covers(&self, other: &PathPattern) -> bool {
        let mine = &self.segments;
        let theirs = &other.segments;
        for (i, pattern) in mine.iter().enumerate() {
            let trailing = i + 1 == mine.len() && pattern == "*";
            if trailing {
                return theirs.len() >= mine.len();
            }
            let Some(segment) = theirs.get(i) else {
                return false;
            };
            if pattern != "*" && pattern != segment {
                return false;
            }
        }
        theirs.len() == mine.len()
    }
}

impl fmt::Display for PathPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "/{}", self.segments.join("/"))
    }
}

/// One allowed destination: a host pattern, optionally one port, and
/// optionally the methods and path pattern its requests must match.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HostRule {
    pub host: Host,
    /// `None`: every port.
    pub port: Option<u16>,
    /// The methods allowed, upper-case and sorted; empty: every method.
    pub methods: Vec<String>,
    /// The path pattern requests must match; `None`: every path.
    pub path: Option<PathPattern>,
}

impl HostRule {
    /// Parse `[METHOD|METHOD... ]HOST[:PORT][/PATH]`; see the module
    /// documentation.
    pub fn parse(text: &str) -> Result<HostRule, String> {
        let bad = |why: &str| format!("egress rule {text:?}: {why}");
        if text.is_empty() {
            return Err(bad("is empty"));
        }
        if text.contains("://") {
            return Err(bad("give a host and optional port, not a URL"));
        }
        if text.contains(',') {
            return Err(bad(
                "must not hold commas (separate methods with |, rules with commas)",
            ));
        }
        if text.chars().any(|c| c.is_control()) {
            return Err(bad("must not hold control characters"));
        }
        let (methods, rest) = match text.split_once(' ') {
            None => (Vec::new(), text),
            Some((methods, rest)) => {
                let not_methods = methods.is_empty() || methods.contains(['/', ':', '[']);
                if rest.contains(' ') || not_methods {
                    return Err(bad(
                        "must not hold spaces, except one between the methods and the host",
                    ));
                }
                (parse_methods(methods).map_err(|why| bad(&why))?, rest)
            }
        };
        let (target, path) = match rest.find('/') {
            Some(at) => (
                &rest[..at],
                Some(PathPattern::parse(&rest[at..]).map_err(|why| bad(&why))?),
            ),
            None => (rest, None),
        };
        if target.is_empty() {
            return Err(bad("needs a host before the path"));
        }
        let (host, port) = if let Some(rest) = target.strip_prefix('[') {
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
            let (host, port) = match target.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (target, None),
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
        Ok(HostRule {
            host,
            port,
            methods,
            path,
        })
    }

    /// Whether this rule allows `host` (as a client names it: a DNS name,
    /// an IP address, an IPv6 address with or without brackets) on `port`,
    /// for some request at least.
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

    /// Whether this rule allows one request: the host and port, and the
    /// method and path when the rule names any.
    pub fn allows_request(&self, host: &str, port: u16, method: &str, path: &str) -> bool {
        self.allows(host, port)
            && (self.methods.is_empty()
                || self.methods.iter().any(|m| m.eq_ignore_ascii_case(method)))
            && self.path.as_ref().is_none_or(|p| p.matches(path))
    }

    /// Whether the rule names methods or a path, so the proxy must look
    /// inside the connection to apply it.
    pub fn inspects(&self) -> bool {
        !self.methods.is_empty() || self.path.is_some()
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
        let methods = self.methods.is_empty()
            || (!other.methods.is_empty()
                && other.methods.iter().all(|m| self.methods.contains(m)));
        let path = match (&self.path, &other.path) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(mine), Some(theirs)) => mine.covers(theirs),
        };
        port && host && methods && path
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

/// `GET|HEAD`: HTTP methods separated by `|`, upper-cased, sorted, once
/// each.
fn parse_methods(text: &str) -> Result<Vec<String>, String> {
    let mut methods = Vec::new();
    for method in text.split('|') {
        if method.is_empty() {
            return Err("methods are separated by |, as GET|HEAD, with none empty".into());
        }
        if method == "*" {
            return Err("to allow every method, drop the methods".into());
        }
        let ok = method
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
        if !ok {
            return Err(format!(
                "{method:?} is not an HTTP method (letters, digits, '-', '_' and '.')"
            ));
        }
        let method = method.to_ascii_uppercase();
        if !methods.contains(&method) {
            methods.push(method);
        }
    }
    methods.sort();
    Ok(methods)
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
        if !self.methods.is_empty() {
            write!(f, "{} ", self.methods.join("|"))?;
        }
        match &self.host {
            Host::Exact(name) if name.contains(':') => write!(f, "[{name}]")?,
            Host::Exact(name) => f.write_str(name)?,
            Host::Subdomains(suffix) => write!(f, "*.{suffix}")?,
        }
        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }
        if let Some(path) = &self.path {
            write!(f, "{path}")?;
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

    /// The first rule that allows `host` on `port`, for some request at
    /// least; `None` when no rule does. Not meaningful for an open policy,
    /// which allows everything.
    pub fn rule_for(&self, host: &str, port: u16) -> Option<&HostRule> {
        self.allow.as_ref()?.iter().find(|r| r.allows(host, port))
    }

    /// The first rule that allows the request; `None` when no rule does.
    pub fn rule_for_request(
        &self,
        host: &str,
        port: u16,
        method: &str,
        path: &str,
    ) -> Option<&HostRule> {
        self.allow
            .as_ref()?
            .iter()
            .find(|r| r.allows_request(host, port, method, path))
    }

    /// Whether a connection to `host` on `port` must be looked inside:
    /// some rule allows it, and every rule that does names methods or a
    /// path. A host-only rule keeps the connection a tunnel.
    pub fn inspects(&self, host: &str, port: u16) -> bool {
        let Some(rules) = &self.allow else {
            return false;
        };
        let mut allowing = rules.iter().filter(|r| r.allows(host, port)).peekable();
        allowing.peek().is_some() && allowing.all(HostRule::inspects)
    }

    /// Whether any rule inspects, so the turn needs a certificate authority
    /// its harness trusts.
    pub fn has_inspected_rules(&self) -> bool {
        self.allow.iter().flatten().any(HostRule::inspects)
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

/// A policy held within a person's `ceiling`, by intersection rather than
/// refusal: an open ceiling leaves `requested` as it is; an open or
/// missing request becomes the ceiling; otherwise each rule of one side
/// that the other covers is kept, so the result is never wider than
/// either. The stricter enforcement applies.
pub fn intersect(requested: Option<&Network>, ceiling: &Network) -> Option<Network> {
    let Some(held) = ceiling.allow.as_ref() else {
        return requested.cloned();
    };
    let Some(asked) = requested.and_then(|r| r.allow.as_ref()) else {
        let enforce = requested.map_or(ceiling.enforce, |r| r.enforce.max(ceiling.enforce));
        return Some(ceiling.clone().with_enforce(enforce));
    };
    let mut allow: Vec<HostRule> = Vec::new();
    for rule in asked
        .iter()
        .filter(|r| held.iter().any(|c| c.covers(r)))
        .chain(held.iter().filter(|c| asked.iter().any(|r| r.covers(c))))
    {
        if !allow.contains(rule) {
            allow.push(rule.clone());
        }
    }
    let enforce = requested.map_or(ceiling.enforce, |r| r.enforce.max(ceiling.enforce));
    Some(Network {
        allow: Some(allow),
        enforce,
    })
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
            "description": "Which hosts, and which requests, the harness may reach \
                            (docs/egress.md): \"open\" (the default), \"none\", or allow rules \
                            [METHOD|METHOD ]HOST[:PORT][/PATH] such as github.com, \
                            *.npmjs.org:443 or GET|HEAD api.github.com/repos/*, with enforce \
                            best_effort (the default) or required.",
            "anyOf": [
                { "type": "string", "enum": ["open", "none"] },
                {
                    "type": "object",
                    "properties": {
                        "allow": {
                            "type": "array",
                            "items": {
                                "type": "string",
                                "pattern": "^([A-Za-z0-9_.\\-]+(\\|[A-Za-z0-9_.\\-]+)* )?((\\*\\.)?[A-Za-z0-9_.\\-]+|\\[[0-9A-Fa-f:.]+\\])(:[0-9]{1,5})?(/[^\\s,?#]*)?$"
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
            "api.github.com/repos/*",
            "GET|HEAD api.github.com:443/repos/*/issues",
            "POST api.github.com",
            "[::1]:8080/v1/*",
            "github.com/",
        ] {
            assert_eq!(rule(text).to_string(), text);
        }
        assert_eq!(rule("GitHub.COM.").to_string(), "github.com");
        assert_eq!(
            rule("head|get|GET x.com/a/").to_string(),
            "GET|HEAD x.com/a",
            "methods upper-cased, once each, sorted; a trailing slash dropped"
        );
        for (text, why) in [
            ("", "empty"),
            ("https://github.com", "not a URL"),
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
            ("a b c", "spaces"),
            ("x.com/a b", "spaces"),
            ("x.com:1 b", "spaces"),
            ("GET a b", "spaces"),
            (" github.com", "spaces"),
            ("-bad.com", "label"),
            ("bad..com", "label"),
            ("*.10.0.0.1", "IP address"),
            ("GET,POST x.com", "commas"),
            ("x.com/a,b", "commas"),
            ("* x.com", "drop the methods"),
            ("GET| x.com", "none empty"),
            ("G€T x.com", "not an HTTP method"),
            ("x.com//a", "empty segment"),
            ("x.com/a*", "whole segment"),
            ("x.com/../a", "not a path segment"),
            ("x.com/a?b", "may not"),
            ("/a", "host before the path"),
            ("GET /a", "host before the path"),
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
    fn rules_match_methods_and_paths() {
        let host_only = rule("api.github.com");
        assert!(!host_only.inspects());
        assert!(host_only.allows_request("api.github.com", 443, "DELETE", "/anything"));

        let get = rule("GET|HEAD api.github.com");
        assert!(get.inspects());
        assert!(get.allows_request("api.github.com", 443, "GET", "/repos"));
        assert!(get.allows_request("api.github.com", 443, "head", "/"));
        assert!(!get.allows_request("api.github.com", 443, "POST", "/repos"));
        assert!(!get.allows_request("github.com", 443, "GET", "/repos"));

        let below = rule("api.github.com/repos/*");
        for ok in [
            "/repos/x",
            "/repos/x/issues",
            "/repos/x/issues/1?page=2",
            "/repos/x/",
        ] {
            assert!(
                below.allows_request("api.github.com", 443, "GET", ok),
                "{ok}"
            );
        }
        for no in ["/repos", "/repos/", "/repositories/x", "/", "/x/repos/y"] {
            assert!(
                !below.allows_request("api.github.com", 443, "GET", no),
                "{no}"
            );
        }

        let one = rule("api.github.com/repos/*/issues");
        assert!(one.allows_request("api.github.com", 443, "GET", "/repos/x/issues"));
        assert!(one.allows_request("api.github.com", 443, "GET", "/repos/x/issues?state=open"));
        assert!(!one.allows_request("api.github.com", 443, "GET", "/repos/x/issues/1"));
        assert!(!one.allows_request("api.github.com", 443, "GET", "/repos/x"));

        let exact = rule("x.com/v1/models");
        assert!(exact.allows_request("x.com", 443, "GET", "/v1/models"));
        assert!(exact.allows_request("x.com", 443, "GET", "/v1/models/"));
        assert!(!exact.allows_request("x.com", 443, "GET", "/v1/models/gpt"));
        assert!(!exact.allows_request("x.com", 443, "GET", "/v1/Models"));

        let root = rule("x.com/");
        assert!(root.allows_request("x.com", 443, "GET", "/"));
        assert!(root.allows_request("x.com", 443, "GET", "/?q"));
        assert!(!root.allows_request("x.com", 443, "GET", "/a"));

        let both = rule("POST x.com:8443/v1/*");
        assert!(both.allows_request("x.com", 8443, "POST", "/v1/messages"));
        assert!(!both.allows_request("x.com", 8443, "GET", "/v1/messages"));
        assert!(!both.allows_request("x.com", 443, "POST", "/v1/messages"));
        assert!(!both.allows_request("x.com", 8443, "POST", "/v2/messages"));
    }

    #[test]
    fn a_policy_inspects_a_host_only_when_every_allowing_rule_does() {
        let policy = Network::parse_flag(
            "GET api.github.com/repos/*, api.github.com:22, github.com, POST x.com",
        )
        .unwrap();
        assert!(policy.inspects("api.github.com", 443));
        assert!(
            !policy.inspects("api.github.com", 22),
            "a host-only rule allows the port"
        );
        assert!(!policy.inspects("github.com", 443), "host-only");
        assert!(!policy.inspects("nobody.com", 443), "nothing allows it");
        assert!(policy.inspects("x.com", 80));
        assert!(policy.has_inspected_rules());
        assert!(!Network::parse_flag("github.com")
            .unwrap()
            .has_inspected_rules());
        assert!(!Network::open().has_inspected_rules());
        // A broad and a narrow rule on one host: the broad one tunnels.
        let mixed = Network::parse_flag("api.github.com, GET api.github.com/x").unwrap();
        assert!(!mixed.inspects("api.github.com", 443));
        assert_eq!(
            policy
                .rule_for_request("api.github.com", 443, "GET", "/repos/a")
                .map(ToString::to_string)
                .as_deref(),
            Some("GET api.github.com/repos/*")
        );
        assert!(policy
            .rule_for_request("api.github.com", 443, "DELETE", "/repos/a")
            .is_none());
        assert!(policy
            .rule_for_request("api.github.com", 443, "GET", "/user")
            .is_none());
        assert_eq!(
            policy
                .rule_for("api.github.com", 443)
                .map(ToString::to_string)
                .as_deref(),
            Some("GET api.github.com/repos/*")
        );
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
                r#"{"allow":["GET|HEAD api.github.com/repos/*"]}"#,
                Network::allow(vec![rule("GET|HEAD api.github.com/repos/*")]),
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
            (r#"{"allow":["x.com/a b"]}"#, "spaces"),
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
        let inspected = Network::parse_flag("GET|HEAD api.github.com/repos/*,github.com").unwrap();
        assert_eq!(
            inspected.to_string(),
            "GET|HEAD api.github.com/repos/*, github.com"
        );
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
            &[
                "*.example.com".into(),
                "github.com:443".into(),
                "GET|POST api.x.com/v1/*".into(),
            ],
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
        // Within: narrower hosts, ports and subdomains; fewer methods, a
        // path below the parent's.
        for within in [
            "api.example.com",
            "*.api.example.com:443",
            "*.example.com",
            "github.com:443",
            "GET github.com:443/repos/*",
            "GET api.x.com/v1/*",
            "GET|POST api.x.com/v1/messages",
            "POST api.x.com:443/v1/a/b",
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
            ("api.x.com", "api.x.com"),
            ("DELETE api.x.com/v1/*", "DELETE api.x.com/v1/*"),
            ("GET api.x.com/v2/*", "GET api.x.com/v2/*"),
            ("GET api.x.com/v1", "GET api.x.com/v1"),
            ("GET api.x.com/*", "GET api.x.com/*"),
            ("api.x.com/v1/*", "api.x.com/v1/*"),
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

    #[test]
    fn path_patterns_cover_only_what_they_match() {
        let covers = |a: &str, b: &str| {
            PathPattern::parse(a)
                .unwrap()
                .covers(&PathPattern::parse(b).unwrap())
        };
        assert!(covers("/a/*", "/a/b"));
        assert!(covers("/a/*", "/a/b/c"));
        assert!(covers("/a/*", "/a/*/c"));
        assert!(covers("/a/*", "/a/*"));
        assert!(covers("/*/b", "/a/b"));
        assert!(covers("/*/b", "/*/b"));
        assert!(covers("/a/b", "/a/b"));
        assert!(covers("/", "/"));
        assert!(!covers("/a/*", "/a"));
        assert!(!covers("/a/*", "/b/c"));
        assert!(!covers("/a/b", "/a/*"));
        assert!(!covers("/a/b", "/a/b/c"));
        assert!(!covers("/*/b", "/a/*"));
        assert!(!covers("/", "/a"));
        assert!(!covers("/a", "/"));
    }

    #[test]
    fn a_ceiling_intersects_rather_than_refuses() {
        let ceiling = Network::parse_flag("*.example.com,github.com:443").unwrap();
        // Open or missing: the ceiling.
        assert_eq!(intersect(None, &ceiling), Some(ceiling.clone()));
        assert_eq!(
            intersect(Some(&Network::open()), &ceiling),
            Some(ceiling.clone())
        );
        // An open ceiling changes nothing.
        let asked = Network::parse_flag("pypi.org").unwrap();
        assert_eq!(
            intersect(Some(&asked), &Network::open()),
            Some(asked.clone())
        );
        assert_eq!(intersect(None, &Network::open()), None);
        // Rules: what one side covers of the other.
        let asked = Network::parse_flag("api.example.com,github.com,pypi.org").unwrap();
        let both = intersect(Some(&asked), &ceiling).unwrap();
        assert_eq!(both.rules(), ["api.example.com", "github.com:443"]);
        // An inspected request within a host-only ceiling is kept as asked.
        let asked = Network::parse_flag("GET api.example.com/v1/*").unwrap();
        let both = intersect(Some(&asked), &ceiling).unwrap();
        assert_eq!(both.rules(), ["GET api.example.com/v1/*"]);
        let required = Network::none().with_enforce(Enforce::Required);
        let both = intersect(Some(&required), &ceiling).unwrap();
        assert_eq!(both, Network::none().with_enforce(Enforce::Required));
    }
}
