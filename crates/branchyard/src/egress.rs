//! A turn's egress: the branch's network policy, applied through an
//! allowlisting proxy (`docs/egress.md`).
//!
//! A branch whose [`Provisioning::network`](crate::Provisioning) is
//! restricted gets, for each turn, a [`Proxy`] that allows only its rules
//! (plus the connector gateway's host when the turn has a grant, the model
//! gateway's when it is on it, and its provider's hosts when its harness
//! calls the provider directly), and the
//! proxy's address in `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY` (and
//! their lower-case forms), with `NO_PROXY` empty so loopback goes through
//! it too. Every destination the proxy decides is recorded on the branch as
//! an [`EgressActivity::Decision`].
//!
//! How much that holds depends on where the harness runs, and the turn
//! records which it got ([`EgressActivity::Applied`]):
//!
//! - [`Enforcement::Enforced`]: a local harness on Linux where unprivileged
//!   user and network namespaces are allowed runs in its own network
//!   namespace whose only way out is the proxy's listener, so a tool that
//!   ignores the variables reaches nothing.
//! - [`Enforcement::Advisory`]: elsewhere locally (macOS, a kernel or
//!   security module that forbids the namespaces), the proxy listens on
//!   this host's loopback and only proxy-aware tools are held to the
//!   policy.
//! - [`Enforcement::NotApplied`]: a sandbox provider that cannot confine a
//!   process ([`branchyard_sandbox::Capabilities::egress`]) gets no proxy,
//!   which its guest could not reach anyway.
//!
//! With `enforce = "required"`, anything but `Enforced` refuses the turn
//! before the harness starts.

use std::net::TcpListener;

use branchyard_provision::network::{Enforce, HostRule, Network};
use branchyard_runtime::egress::{Decision, Proxy, Verdict};
use branchyard_runtime::LocalProvider;
use serde::{Deserialize, Serialize};

use crate::state::{now_ms, Record};
use crate::{Activity, Provider, RecordedEvent, Yard};

/// The port the proxy's listener takes inside a confined harness's own
/// network namespace.
pub const CONFINED_PORT: u16 = 3128;

/// How a turn's egress policy was applied.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Enforcement {
    /// The harness's only network is the proxy.
    Enforced,
    /// The harness was given the proxy's variables; a tool that ignores
    /// them is not held to the policy.
    Advisory,
    /// Nothing was applied: the provider cannot confine its sandboxes or
    /// route them to the proxy.
    NotApplied,
}

impl Enforcement {
    pub fn as_str(&self) -> &'static str {
        match self {
            Enforcement::Enforced => "enforced",
            Enforcement::Advisory => "advisory",
            Enforcement::NotApplied => "not applied",
        }
    }
}

/// What the egress proxy did, recorded on the branch.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EgressActivity {
    /// The turn's policy and how it was applied; once per turn.
    Applied {
        /// The policy in one line (`none`, or its rules).
        policy: String,
        /// The rules in force this turn, the gateway's included.
        allow: Vec<String>,
        enforcement: Enforcement,
        /// Why it is not enforced, when it is not.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// One destination the proxy decided.
    Decision {
        /// `CONNECT`, or the plain HTTP request's method.
        method: String,
        host: String,
        port: u16,
        allowed: bool,
        /// The rule that allowed it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rule: Option<String>,
        /// Why it was refused, or why an allowed one failed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

impl EgressActivity {
    /// One line for people, as `by log` prints it.
    pub fn describe(&self) -> String {
        match self {
            EgressActivity::Applied {
                policy,
                enforcement,
                reason,
                ..
            } => {
                let mut text = format!("egress {}: {policy}", enforcement.as_str());
                if let Some(reason) = reason {
                    text.push_str(&format!(" ({reason})"));
                }
                text
            }
            EgressActivity::Decision {
                method,
                host,
                port,
                allowed,
                rule,
                reason,
            } => {
                let verdict = match allowed {
                    true => "allowed",
                    false => "denied",
                };
                let host = match host.contains(':') {
                    true => format!("[{host}]"),
                    false => host.clone(),
                };
                let mut text = format!("egress {verdict}: {method} {host}:{port}");
                match (allowed, rule, reason) {
                    (true, Some(rule), None) => text.push_str(&format!(" by {rule}")),
                    (_, _, Some(reason)) => text.push_str(&format!(" ({reason})")),
                    _ => {}
                }
                text
            }
        }
    }
}

/// A turn's egress, from its preparation until the turn ends.
pub(crate) struct Egress {
    proxy: Option<Proxy>,
    confined: bool,
    env: Vec<(String, String)>,
    applied: EgressActivity,
    /// The proxy's record in the repository's service registry, while the
    /// turn runs. The proxy lives in this process, so there is nothing to
    /// reclaim: the record only says it is there, and for whom.
    _service: Option<crate::services::Registration>,
}

impl Egress {
    /// What to record before the harness starts.
    pub fn applied(&self) -> Activity {
        Activity::Egress(Box::new(self.applied.clone()))
    }

    /// The variables the harness gets.
    pub fn env(&self) -> &[(String, String)] {
        &self.env
    }

    /// Whether the harness must be started confined, its listener then
    /// given to [`Egress::serve`].
    pub fn confined(&self) -> bool {
        self.confined
    }

    /// Serve a confined harness's listener.
    pub fn serve(&self, listener: TcpListener) -> std::io::Result<()> {
        match &self.proxy {
            Some(proxy) => proxy.serve(listener),
            None => Ok(()),
        }
    }
}

/// The variables that point a harness at the proxy at `url`. `NO_PROXY`
/// is empty: loopback goes through the proxy like any other host.
fn proxy_env(url: &str) -> Vec<(String, String)> {
    let mut env = Vec::new();
    for name in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
        env.push((name.to_owned(), url.to_owned()));
        env.push((name.to_ascii_lowercase(), url.to_owned()));
    }
    env.push(("NO_PROXY".to_owned(), String::new()));
    env.push(("no_proxy".to_owned(), String::new()));
    env
}

/// The gateway's `host:port` as a rule, from its URL.
pub(crate) fn gateway_rule(url: &str) -> Option<HostRule> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let default = match scheme.to_ascii_lowercase().as_str() {
        "https" => 443,
        "http" => 80,
        _ => return None,
    };
    let has_port = match authority.strip_prefix('[') {
        Some(rest) => rest
            .split_once(']')
            .is_some_and(|(_, p)| p.starts_with(':')),
        None => authority.contains(':'),
    };
    let text = match has_port {
        true => authority.to_owned(),
        false => format!("{authority}:{default}"),
    };
    HostRule::parse(&text).ok()
}

/// Refuse, before anything is created, a network policy that means
/// nothing, or that must be enforced where it cannot be: under a sandbox
/// provider, or locally on a host that cannot confine a harness.
pub(crate) fn check(
    provision: Option<&crate::Provisioning>,
    provider: Option<&Provider>,
) -> Result<(), crate::Error> {
    let Some(network) = provision.and_then(|p| p.network.as_ref()) else {
        return Ok(());
    };
    network.check().map_err(crate::Error::Unsupported)?;
    if network.is_open() || network.enforce != Enforce::Required {
        return Ok(());
    }
    let why = match provider {
        None | Some(Provider::Local) => LocalProvider::confinement()
            .err()
            .map(|why| format!("this host cannot confine a local harness: {why}")),
        Some(other) => Some(format!(
            "the {} provider cannot confine its sandboxes",
            crate::snapshots::provider_name(other)
        )),
    };
    match why {
        None => Ok(()),
        Some(why) => Err(crate::Error::Unsupported(format!(
            "the network policy must be enforced, but {why} (docs/egress.md)"
        ))),
    }
}

/// The turn's egress for `record`: `None` when its policy is open. A
/// failure is the turn's: a policy that must be enforced where it cannot
/// be, or a proxy that could not listen.
pub(crate) fn prepare(
    yard: &Yard,
    record: &Record,
    extra: Vec<HostRule>,
) -> Result<Option<Egress>, String> {
    let Some(network) = record
        .provision
        .as_ref()
        .and_then(|p| p.network.clone())
        .filter(|n| !n.is_open())
    else {
        return Ok(None);
    };
    // The gateways the turn is given (connectors, models), and the hosts
    // a harness calling its provider directly needs, for this turn only.
    let network = extra
        .into_iter()
        .fold(network, |network, rule| network.with_rule(rule));
    let policy = network.to_string();
    let required = network.enforce == Enforce::Required;
    let applied = |enforcement: Enforcement, reason: Option<String>| EgressActivity::Applied {
        policy: policy.clone(),
        allow: network.rules(),
        enforcement,
        reason,
    };
    let provider = match &record.provider {
        None | Some(Provider::Local) => None,
        Some(other) => Some(crate::snapshots::provider_name(other)),
    };
    if let Some(provider) = provider {
        let why = format!(
            "the {provider} provider cannot confine its sandboxes or route them to the egress \
             proxy, so the policy is not applied"
        );
        if required {
            return Err(format!(
                "its network policy must be enforced, but {why} (docs/egress.md)"
            ));
        }
        return Ok(Some(Egress {
            proxy: None,
            confined: false,
            env: Vec::new(),
            applied: applied(Enforcement::NotApplied, Some(why)),
            _service: None,
        }));
    }
    let confinement = LocalProvider::confinement();
    if let (Err(why), true) = (&confinement, required) {
        return Err(format!(
            "its network policy must be enforced, but this host cannot confine a local \
             harness: {why} (docs/egress.md)"
        ));
    }
    let proxy = proxy(yard, &record.info.name, network.clone());
    let branch = &record.info.name;
    let (confined, env, enforcement, reason, endpoint) = match confinement {
        Ok(()) => (
            true,
            proxy_env(&format!("http://127.0.0.1:{CONFINED_PORT}")),
            Enforcement::Enforced,
            None,
            crate::services::Endpoint::InProcess {
                name: format!("netns:{branch}:{CONFINED_PORT}"),
            },
        ),
        Err(why) => {
            let address = proxy
                .listen_loopback()
                .map_err(|e| format!("could not start the egress proxy: {e}"))?;
            (
                false,
                proxy_env(&format!("http://{address}")),
                Enforcement::Advisory,
                Some(format!(
                    "only tools that honor the proxy variables are held to it: {why}"
                )),
                crate::services::Endpoint::url(format!("http://{address}")),
            )
        }
    };
    let service = yard
        .register_service(
            crate::services::Service::new(
                crate::services::KIND_EGRESS_PROXY,
                crate::services::ServiceOwner::this_process().for_branch(branch),
            )
            .with("branch", branch.as_str())
            .with("enforcement", enforcement.as_str())
            .with("allow", network.rules())
            .with_endpoint(endpoint),
            crate::services::DEFAULT_TTL,
        )
        .ok();
    Ok(Some(Egress {
        proxy: Some(proxy),
        confined,
        env,
        applied: applied(enforcement, reason),
        _service: service,
    }))
}

/// A proxy deciding by `network` and recording each decision on `branch`.
fn proxy(yard: &Yard, branch: &str, network: Network) -> Proxy {
    let yard = yard.clone();
    let branch = branch.to_owned();
    Proxy::new(
        move |host, port| match network.rule_for(host, port) {
            Some(rule) => Verdict {
                allowed: true,
                rule: Some(rule.to_string()),
                loopback: rule.names_address(),
            },
            None => Verdict::default(),
        },
        move |decision: &Decision| {
            let event = RecordedEvent {
                at_ms: now_ms(),
                activity: Activity::Egress(Box::new(EgressActivity::Decision {
                    method: decision.method.clone(),
                    host: decision.host.clone(),
                    port: decision.port,
                    allowed: decision.allowed,
                    rule: decision.rule.clone(),
                    reason: decision.reason.clone(),
                })),
            };
            let _ = yard.store().append(&branch, &event, None);
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gateway_url_becomes_a_rule_for_its_host_and_port() {
        let rule = |url: &str| gateway_rule(url).map(|r| r.to_string());
        assert_eq!(
            rule("http://127.0.0.1:8931/mcp").as_deref(),
            Some("127.0.0.1:8931")
        );
        assert_eq!(
            rule("https://gw.example.com/mcp").as_deref(),
            Some("gw.example.com:443")
        );
        assert_eq!(
            rule("http://GW.example.com").as_deref(),
            Some("gw.example.com:80")
        );
        assert_eq!(rule("http://[::1]:9/mcp").as_deref(), Some("[::1]:9"));
        assert_eq!(
            rule("http://u:p@gw.example.com:81/").as_deref(),
            Some("gw.example.com:81")
        );
        assert_eq!(rule("ftp://x/"), None);
        assert_eq!(rule("not a url"), None);
    }

    #[test]
    fn the_variables_cover_both_cases_and_leave_nothing_out() {
        let env = proxy_env("http://127.0.0.1:3128");
        let names: Vec<_> = env.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "HTTP_PROXY",
                "http_proxy",
                "HTTPS_PROXY",
                "https_proxy",
                "ALL_PROXY",
                "all_proxy",
                "NO_PROXY",
                "no_proxy"
            ]
        );
        assert!(env[..6].iter().all(|(_, v)| v == "http://127.0.0.1:3128"));
        assert!(env[6..].iter().all(|(_, v)| v.is_empty()));
    }

    #[test]
    fn a_sandbox_provider_cannot_take_a_required_policy() {
        let required = crate::Provisioning {
            network: Some(Network::none().with_enforce(Enforce::Required)),
            ..crate::Provisioning::default()
        };
        let best_effort = crate::Provisioning {
            network: Some(Network::none()),
            ..crate::Provisioning::default()
        };
        let microsandbox = Provider::Microsandbox(crate::SandboxOptions {
            image: "img".into(),
            ..crate::SandboxOptions::default()
        });
        match check(Some(&required), Some(&microsandbox)) {
            Err(crate::Error::Unsupported(why)) => {
                assert!(
                    why.contains("microsandbox provider cannot confine"),
                    "{why}"
                )
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        check(Some(&best_effort), Some(&microsandbox)).unwrap();
        check(None, Some(&microsandbox)).unwrap();
    }

    #[test]
    fn decisions_describe_themselves() {
        let allowed = EgressActivity::Decision {
            method: "CONNECT".into(),
            host: "github.com".into(),
            port: 443,
            allowed: true,
            rule: Some("github.com".into()),
            reason: None,
        };
        assert_eq!(
            allowed.describe(),
            "egress allowed: CONNECT github.com:443 by github.com"
        );
        let denied = EgressActivity::Decision {
            method: "GET".into(),
            host: "::1".into(),
            port: 80,
            allowed: false,
            rule: None,
            reason: Some("no rule allows it".into()),
        };
        assert_eq!(
            denied.describe(),
            "egress denied: GET [::1]:80 (no rule allows it)"
        );
        let applied = EgressActivity::Applied {
            policy: "none".into(),
            allow: Vec::new(),
            enforcement: Enforcement::Advisory,
            reason: Some("macOS".into()),
        };
        assert_eq!(applied.describe(), "egress advisory: none (macOS)");
    }
}
