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
//! A rule that names methods or a path (`GET api.github.com/repos/*`) is
//! held to inside the connection: the proxy terminates TLS to that host
//! with a certificate signed by the branch's own authority, kept under
//! `.branchyard/egress/<branch>/`, and the harness is told to trust it
//! through the variables the common TLS stacks read (`SSL_CERT_FILE`,
//! `NODE_EXTRA_CA_CERTS`, ...), pointing at a bundle of the system's roots
//! and that authority. A host-only rule stays a tunnel the proxy does not
//! look inside.
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

use branchyard_support::best_effort;
use std::net::TcpListener;
use std::path::{Path, PathBuf};

use branchyard_provision::network::{Enforce, HostRule, Network};
use branchyard_runtime::egress::{Authority, Decision, Interceptor, Proxy, Verdict};
use branchyard_runtime::LocalProvider;
use serde::{Deserialize, Serialize};

use crate::state::{Record, Store};
use crate::{Activity, Provider, RecordedEvent, Yard};
use branchyard_support::time::now_ms;

/// The port the proxy's listener takes inside a confined harness's own
/// network namespace.
pub const CONFINED_PORT: u16 = 3128;

/// The variables that make the tools a harness runs trust the branch's
/// certificate authority, set only when its policy inspects a host:
/// OpenSSL's and Go's, curl's, Python requests', Node's, Deno's, cargo's
/// and git's. Each names the bundle of the system's roots and the
/// authority, so nothing the harness trusted before stops being trusted.
pub const TRUST_VARIABLES: [&str; 7] = [
    "SSL_CERT_FILE",
    "CURL_CA_BUNDLE",
    "REQUESTS_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
    "DENO_CERT",
    "CARGO_HTTP_CAINFO",
    "GIT_SSL_CAINFO",
];

/// Where a system keeps its root certificates, by distribution; the first
/// that exists is the bundle's base, after the engine's own
/// `SSL_CERT_FILE`.
const SYSTEM_ROOTS: [&str; 6] = [
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/ca-bundle.pem",
    "/etc/pki/tls/cacert.pem",
    "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
    "/etc/ssl/cert.pem",
];

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
        /// The certificate bundle the harness is told to trust, when a
        /// rule inspects a host: the system's roots and the branch's own
        /// authority, under `.branchyard/egress/<branch>/`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ca_bundle: Option<String>,
    },
    /// One destination or request the proxy decided.
    Decision {
        /// `CONNECT`, or the method of a request the proxy saw: a plain
        /// HTTP request, or one inside an inspected connection.
        method: String,
        host: String,
        port: u16,
        /// The request's path and query, when the proxy saw one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        allowed: bool,
        /// The rule that allowed it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rule: Option<String>,
        /// Why it was refused, or why an allowed one failed; `inspected`
        /// for a `CONNECT` the proxy looked inside.
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
                path,
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
                let path = path.as_deref().unwrap_or_default();
                let mut text = format!("egress {verdict}: {method} {host}:{port}{path}");
                if let (true, Some(rule)) = (allowed, rule) {
                    text.push_str(&format!(" by {rule}"));
                }
                if let Some(reason) = reason {
                    text.push_str(&format!(" ({reason})"));
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
    let kind = crate::providers::of(provider);
    let why = match kind.confines_egress() {
        true => LocalProvider::confinement()
            .err()
            .map(|why| format!("this host cannot confine a local harness: {why}")),
        false => Some(format!(
            "the {} provider cannot confine its sandboxes",
            kind.name()
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
    let applied = |enforcement: Enforcement, reason: Option<String>, ca_bundle: Option<PathBuf>| {
        EgressActivity::Applied {
            policy: policy.clone(),
            allow: network.rules(),
            enforcement,
            reason,
            ca_bundle: ca_bundle.map(|p| p.display().to_string()),
        }
    };
    let kind = crate::providers::of(record.provider.as_ref());
    if !kind.confines_egress() {
        let provider = kind.name();
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
            applied: applied(Enforcement::NotApplied, Some(why), None),
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
    // A rule that inspects needs the branch's authority, and the harness
    // told to trust it.
    let mut ca_bundle = None;
    let mut trust = Vec::new();
    if network.has_inspected_rules() {
        let (authority, bundle) = authority(&yard.store(), branch)?;
        let interceptor = Interceptor::new(authority, system_roots().as_deref())
            .map_err(|e| format!("could not prepare TLS interception: {e}"))?;
        proxy
            .intercept(interceptor)
            .map_err(|e| format!("could not prepare TLS interception: {e}"))?;
        trust = trust_env(&bundle);
        ca_bundle = Some(bundle);
    }
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
    let env = env.into_iter().chain(trust).collect();
    Ok(Some(Egress {
        proxy: Some(proxy),
        confined,
        env,
        applied: applied(enforcement, reason, ca_bundle),
        _service: service,
    }))
}

/// The branch's certificate authority, made on first use and kept under
/// `.branchyard/egress/<branch>/` (`ca.key`, readable by its owner only,
/// and `ca.pem`), and its bundle `ca-bundle.pem`, rewritten each turn: the
/// system's roots, then the authority. An authority whose files cannot be
/// read back is made anew.
fn authority(store: &Store, branch: &str) -> Result<(Authority, PathBuf), String> {
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
    let dir = store.egress(branch);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let key_path = dir.join("ca.key");
    let cert_path = dir.join("ca.pem");
    let kept = match (
        std::fs::read_to_string(&cert_path),
        std::fs::read_to_string(&key_path),
    ) {
        (Ok(cert), Ok(key)) => Authority::from_pem(branch, &cert, &key).ok(),
        _ => None,
    };
    let authority = match kept {
        Some(authority) => authority,
        None => {
            let authority = Authority::generate(branch)?;
            let mut key = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&key_path)
                .map_err(|e| format!("could not write {}: {e}", key_path.display()))?;
            std::io::Write::write_all(&mut key, authority.key_pem().as_bytes())
                .map_err(|e| format!("could not write {}: {e}", key_path.display()))?;
            std::fs::write(&cert_path, authority.cert_pem())
                .map_err(|e| format!("could not write {}: {e}", cert_path.display()))?;
            authority
        }
    };
    let bundle = dir.join("ca-bundle.pem");
    let mut text = system_roots().unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(authority.cert_pem());
    std::fs::write(&bundle, text)
        .map_err(|e| format!("could not write {}: {e}", bundle.display()))?;
    Ok((authority, bundle))
}

/// The system's root certificates, PEM: the engine's own `SSL_CERT_FILE`
/// when it names a readable file, else the first of the usual bundles
/// present. `None` on a system with neither; the harness then trusts the
/// authority alone through the variables, and the proxy the Mozilla roots.
fn system_roots() -> Option<String> {
    std::env::var_os("SSL_CERT_FILE")
        .map(PathBuf::from)
        .into_iter()
        .chain(SYSTEM_ROOTS.iter().map(PathBuf::from))
        .find_map(|path| std::fs::read_to_string(path).ok())
}

/// The trust variables, each naming `bundle`.
fn trust_env(bundle: &Path) -> Vec<(String, String)> {
    let value = bundle.display().to_string();
    TRUST_VARIABLES
        .iter()
        .map(|name| ((*name).to_owned(), value.clone()))
        .collect()
}

/// A proxy deciding by `network` and recording each decision on `branch`.
fn proxy(yard: &Yard, branch: &str, network: Network) -> Proxy {
    let yard = yard.clone();
    let branch = branch.to_owned();
    Proxy::new(
        move |host, port, request| {
            // A request the proxy sees is held to the whole rule; a CONNECT
            // it has not looked inside is allowed by any rule for the host,
            // and looked inside when every such rule asks for it.
            let rule = match request {
                Some((method, path)) => network.rule_for_request(host, port, method, path),
                None => network.rule_for(host, port),
            };
            match rule {
                Some(rule) => Verdict {
                    allowed: true,
                    rule: Some(rule.to_string()),
                    loopback: rule.names_address(),
                    inspect: request.is_none() && network.inspects(host, port),
                },
                None => Verdict::default(),
            }
        },
        move |decision: &Decision| {
            let event = RecordedEvent {
                at_ms: now_ms(),
                activity: Activity::Egress(Box::new(EgressActivity::Decision {
                    method: decision.method.clone(),
                    host: decision.host.clone(),
                    port: decision.port,
                    path: decision.path.clone(),
                    allowed: decision.allowed,
                    rule: decision.rule.clone(),
                    reason: decision.reason.clone(),
                })),
            };
            best_effort(
                "append the event to the branch's log",
                yard.store().append(&branch, &event, None),
            );
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
        let microsandbox: Provider =
            serde_json::from_value(serde_json::json!({ "kind": "microsandbox", "image": "img" }))
                .unwrap();
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
            path: None,
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
            path: None,
            allowed: false,
            rule: None,
            reason: Some("no rule allows it".into()),
        };
        assert_eq!(
            denied.describe(),
            "egress denied: GET [::1]:80 (no rule allows it)"
        );
        let inspected = EgressActivity::Decision {
            method: "CONNECT".into(),
            host: "api.github.com".into(),
            port: 443,
            path: None,
            allowed: true,
            rule: Some("GET api.github.com/repos/*".into()),
            reason: Some("inspected".into()),
        };
        assert_eq!(
            inspected.describe(),
            "egress allowed: CONNECT api.github.com:443 by GET api.github.com/repos/* (inspected)"
        );
        let inside = EgressActivity::Decision {
            method: "POST".into(),
            host: "api.github.com".into(),
            port: 443,
            path: Some("/repos/x/issues?a=1".into()),
            allowed: false,
            rule: None,
            reason: Some("no rule allows it".into()),
        };
        assert_eq!(
            inside.describe(),
            "egress denied: POST api.github.com:443/repos/x/issues?a=1 (no rule allows it)"
        );
        let applied = EgressActivity::Applied {
            policy: "none".into(),
            allow: Vec::new(),
            enforcement: Enforcement::Advisory,
            reason: Some("macOS".into()),
            ca_bundle: None,
        };
        assert_eq!(applied.describe(), "egress advisory: none (macOS)");
    }

    #[test]
    fn the_trust_variables_name_the_bundle_and_the_decision_follows_the_rule() {
        let env = trust_env(Path::new("/r/.branchyard/egress/b/ca-bundle.pem"));
        let names: Vec<_> = env.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, TRUST_VARIABLES);
        assert!(env
            .iter()
            .all(|(_, v)| v == "/r/.branchyard/egress/b/ca-bundle.pem"));
        // What the proxy's decision says, by the policy alone.
        let network =
            Network::parse_flag("GET api.github.com/repos/*, github.com, 127.0.0.1:9").unwrap();
        let decide = |host: &str, port: u16, request: Option<(&str, &str)>| {
            let rule = match request {
                Some((method, path)) => network.rule_for_request(host, port, method, path),
                None => network.rule_for(host, port),
            };
            rule.map(|rule| {
                (
                    rule.to_string(),
                    rule.names_address(),
                    request.is_none() && network.inspects(host, port),
                )
            })
        };
        assert_eq!(
            decide("api.github.com", 443, None),
            Some(("GET api.github.com/repos/*".to_owned(), false, true))
        );
        assert_eq!(
            decide("api.github.com", 443, Some(("GET", "/repos/a/b"))),
            Some(("GET api.github.com/repos/*".to_owned(), false, false))
        );
        assert_eq!(
            decide("api.github.com", 443, Some(("POST", "/repos/a"))),
            None
        );
        assert_eq!(decide("api.github.com", 443, Some(("GET", "/user"))), None);
        assert_eq!(
            decide("github.com", 443, None),
            Some(("github.com".to_owned(), false, false))
        );
        assert_eq!(
            decide("127.0.0.1", 9, Some(("DELETE", "/x"))),
            Some(("127.0.0.1:9".to_owned(), true, false))
        );
        assert_eq!(decide("nobody.example", 443, None), None);
    }
}
