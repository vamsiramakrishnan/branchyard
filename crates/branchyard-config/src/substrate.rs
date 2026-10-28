//! Saved Agent Substrate connections and explicit actor egress policy.

use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{resolve_path, ConfigError};

/// The destinations actors may reach through Substrate's egress gateway.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "mode",
    content = "hosts",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SubstrateEgress {
    /// Deny outgoing connections.
    #[default]
    Deny,
    /// Allow DNS names, optionally beginning with `*.` for one subdomain label.
    AllowHosts(Vec<String>),
    /// Allow all destinations, selected explicitly by the operator.
    Unrestricted,
}

/// `[substrate]`: a remote actor environment, with credential paths only.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Substrate {
    /// The Control API URL.
    pub endpoint: String,
    /// The router base URL; actors are addressed by the routing header.
    pub router: String,
    /// The actor template, as a name or `atespace/name`.
    pub template: String,
    /// Path to the bridge signing key, relative to this configuration file.
    pub key: String,
    /// Explicit outgoing network policy for actors.
    pub egress: SubstrateEgress,
    /// The actor namespace. Empty selects `default`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub atespace: String,
    /// Absolute worktree directory in each actor. Empty selects `/workspace`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workdir: String,
    /// Absolute harness home in each actor. Empty selects `/branchyard/home`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub home: String,
    /// Environment variable names to copy into actors.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pass_env: Vec<String>,
    /// Path to the Control bearer token file, reread for each RPC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_token_file: Option<String>,
    /// Verified TLS service name, including when Control is port-forwarded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    /// PEM authorities for Control and, unless overridden, the router.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca: Option<String>,
    /// PEM client certificate for Control mutual TLS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert: Option<String>,
    /// PEM private key for `client_cert`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key: Option<String>,
    /// PEM authorities for the router.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub router_ca: Option<String>,
    /// Allow unencrypted connections beyond loopback.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub insecure: bool,
    /// Actor behavior after a turn, `destroy` or `pause`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "keep_sandbox"
    )]
    pub keep: Option<String>,
    /// Number of checkpoints retaining a provider snapshot.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "sandbox_snapshots"
    )]
    pub snapshots: Option<u32>,
    /// Maximum kept actors per repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub max_paused: Option<u32>,
    /// Maximum idle minutes before a kept actor is destroyed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub max_kept_minutes: Option<u32>,
}

impl Substrate {
    pub(crate) fn check(&self) -> Result<(), ConfigError> {
        let fail = |field: &str, why: &str| ConfigError(format!("substrate.{field}: {why}"));
        for (field, value) in [
            ("endpoint", &self.endpoint),
            ("router", &self.router),
            ("template", &self.template),
            ("key", &self.key),
        ] {
            if value.trim().is_empty() {
                return Err(fail(field, "must not be empty"));
            }
        }
        for (field, value, schemes) in [
            (
                "endpoint",
                self.endpoint.as_str(),
                &["https://", "http://"][..],
            ),
            (
                "router",
                self.router.as_str(),
                &["https://", "http://", "wss://", "ws://"][..],
            ),
        ] {
            if !schemes.iter().any(|s| value.starts_with(s)) {
                return Err(fail(field, "needs an HTTP or TLS URL"));
            }
        }
        for (field, path) in [("workdir", &self.workdir), ("home", &self.home)] {
            if !path.is_empty() && !Path::new(path).is_absolute() {
                return Err(fail(field, "must be an absolute path in the actor"));
            }
        }
        for (field, value) in [
            ("api_token_file", &self.api_token_file),
            ("server_name", &self.server_name),
            ("ca", &self.ca),
            ("client_cert", &self.client_cert),
            ("client_key", &self.client_key),
            ("router_ca", &self.router_ca),
        ] {
            if value.as_ref().is_some_and(|v| v.trim().is_empty()) {
                return Err(fail(field, "must not be empty"));
            }
        }
        if self.client_cert.is_some() != self.client_key.is_some() {
            return Err(fail(
                "client_cert",
                "client_cert and client_key must be set together",
            ));
        }
        for name in &self.pass_env {
            branchyard_provision::check_variable_name(name).map_err(|why| fail("pass_env", why))?;
        }
        if self
            .keep
            .as_deref()
            .is_some_and(|k| !matches!(k, "pause" | "destroy"))
        {
            return Err(fail("keep", "must be pause or destroy"));
        }
        if self.max_paused == Some(0) {
            return Err(fail("max_paused", "must be at least 1"));
        }
        if self.max_kept_minutes == Some(0) {
            return Err(fail("max_kept_minutes", "must be at least 1"));
        }
        if let SubstrateEgress::AllowHosts(hosts) = &self.egress {
            if hosts.is_empty() || hosts.len() > 256 {
                return Err(fail("egress.hosts", "needs 1 to 256 DNS host patterns"));
            }
            for host in hosts {
                let rule = branchyard_provision::network::HostRule::parse(host)
                    .map_err(|_| fail("egress.hosts", "needs DNS names or leading *. patterns"))?;
                let name = host.strip_prefix("*.").unwrap_or(host);
                if rule.port.is_some()
                    || rule.path.is_some()
                    || !rule.methods.is_empty()
                    || name.parse::<std::net::IpAddr>().is_ok()
                    || name.ends_with('.')
                    || name.bytes().any(|b| b.is_ascii_uppercase() || b == b'_')
                {
                    return Err(fail(
                        "egress.hosts",
                        "needs lowercase DNS names, without ports, IP addresses or paths",
                    ));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn resolve_paths(&mut self, dir: &Path, home: Option<&Path>) {
        self.key = resolve_path(&self.key, dir, home).display().to_string();
        for path in [
            &mut self.api_token_file,
            &mut self.ca,
            &mut self.client_cert,
            &mut self.client_key,
            &mut self.router_ca,
        ]
        .into_iter()
        .flatten()
        {
            *path = resolve_path(path, dir, home).display().to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{merge, parse, render, ProviderKind, Source};

    const CONFIG: &str = r#"
version = 1
[defaults]
provider = "substrate"
[substrate]
endpoint = "https://localhost:9443"
router = "http://localhost:8080"
template = "yard/agy"
key = "bridge.key"
egress = { mode = "allow_hosts", hosts = ["generativelanguage.googleapis.com", "*.googleapis.com"] }
api_token_file = "control.token"
server_name = "ate-api.ate-system.svc"
ca = "ca.pem"
client_cert = "client.pem"
client_key = "client.key"
router_ca = "~/router.pem"
workdir = "/workspace"
home = "/branchyard/home"
pass_env = ["LANG"]
keep = "pause"
snapshots = 2
max_paused = 4
max_kept_minutes = 60
"#;

    #[test]
    fn saved_substrate_round_trips_and_resolves_each_source_before_merge() {
        let mut user = parse(CONFIG).unwrap();
        assert_eq!(parse(&render(&user, "Saved Substrate")).unwrap(), user);
        user.resolve_paths(Path::new("/user-config"), Some(Path::new("/home/me")));
        let mut project = parse(
            &CONFIG
                .replace("control.token", "project.token")
                .replace("ca = \"ca.pem\"\n", ""),
        )
        .unwrap();
        project.resolve_paths(Path::new("/repo"), Some(Path::new("/home/me")));
        let merged = merge(
            &[
                (
                    Source::User {
                        path: "/user-config/config.toml".into(),
                    },
                    user.clone(),
                ),
                (
                    Source::Project {
                        path: "/repo/branchyard.toml".into(),
                    },
                    project,
                ),
            ],
            |_| None,
        )
        .unwrap()
        .config;
        assert_eq!(merged.defaults.provider, Some(ProviderKind::Substrate));
        let saved = merged.substrate.unwrap();
        assert_eq!(saved.key, "/repo/bridge.key");
        assert_eq!(saved.api_token_file.as_deref(), Some("/repo/project.token"));
        assert_eq!(saved.router_ca.as_deref(), Some("/home/me/router.pem"));
        assert_eq!(saved.ca.as_deref(), Some("/user-config/ca.pem"));
        let user = user.substrate.unwrap();
        assert_eq!(user.ca.as_deref(), Some("/user-config/ca.pem"));
        assert_eq!(user.client_cert.as_deref(), Some("/user-config/client.pem"));
        assert_eq!(user.client_key.as_deref(), Some("/user-config/client.key"));
        assert_eq!(user.home, "/branchyard/home");
    }

    #[test]
    fn saved_substrate_refuses_incomplete_or_ambiguous_configuration() {
        assert!(parse("[defaults]\nprovider = 'substrate'")
            .unwrap_err()
            .to_string()
            .contains("[substrate]"));
        for (old, new, field) in [
            ("key = \"bridge.key\"", "key = \"\"", "substrate.key"),
            (
                "workdir = \"/workspace\"",
                "workdir = \"workspace\"",
                "substrate.workdir",
            ),
            ("client_key = \"client.key\"", "", "client_cert"),
            ("max_paused = 4", "max_paused = 0", "max_paused"),
            (
                "max_kept_minutes = 60",
                "max_kept_minutes = 0",
                "max_kept_minutes",
            ),
            ("keep = \"pause\"", "keep = \"forever\"", "substrate.keep"),
            (
                "pass_env = [\"LANG\"]",
                "pass_env = [\"bad=name\"]",
                "pass_env",
            ),
            (
                "generativelanguage.googleapis.com",
                "https://example.com",
                "egress.hosts",
            ),
            (
                "generativelanguage.googleapis.com",
                "example.com:443",
                "egress.hosts",
            ),
            (
                "generativelanguage.googleapis.com",
                "127.0.0.1",
                "egress.hosts",
            ),
        ] {
            let error = parse(&CONFIG.replace(old, new)).unwrap_err().to_string();
            assert!(error.contains(field), "{field}: {error}");
        }
        assert!(parse(&CONFIG.replace("egress = { mode = \"allow_hosts\", hosts = [\"generativelanguage.googleapis.com\", \"*.googleapis.com\"] }", "")).unwrap_err().to_string().contains("egress"));
    }

    #[test]
    fn reading_a_relative_config_path_keeps_absolute_credential_paths() {
        let file = Path::new("tests/fixtures/substrate.toml");
        let saved = crate::read_layer(file).unwrap().substrate.unwrap();
        let parent = std::path::absolute(file)
            .unwrap()
            .parent()
            .unwrap()
            .to_owned();
        assert_eq!(Path::new(&saved.key), parent.join("bridge.key"));
        assert_eq!(
            Path::new(saved.api_token_file.as_deref().unwrap()),
            parent.join("control.token")
        );
    }
}
