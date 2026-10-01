//! The connector gateway on a server: each served repository's yard gets
//! the gateway (signing with the server's key, its branches named
//! `<repo>/<branch>` in tokens and the audit log), the public keys are
//! served at `GET /.well-known/jwks.json`, and, with `run_gateway`, Anvil's
//! gateway runs beside the server, supervised. See `docs/connectors.md`.

use std::path::PathBuf;
use std::sync::Arc;

use branchyard::connectors::gateway::{self, GatewayCommand, Supervisor};
use branchyard::connectors::{AnvilPackager, Gateway, KeyRing, MAX_TTL};

use crate::config::{Config, ConnectorsConfig, DEFAULT_TENANT};

fn dir(config: &Config) -> PathBuf {
    config.data_dir.join("gateway")
}

/// The signing key file.
pub fn key_file(config: &Config, c: &ConnectorsConfig) -> PathBuf {
    c.signing_key
        .clone()
        .unwrap_or_else(|| dir(config).join("key"))
}

/// The public keys, kept in step beside the data directory's gateway files
/// for a gateway run here (a `file:` URL).
pub fn jwks_file(config: &Config) -> PathBuf {
    dir(config).join("jwks.json")
}

fn audit_file(config: &Config, c: &ConnectorsConfig) -> PathBuf {
    c.audit_file
        .clone()
        .unwrap_or_else(|| dir(config).join("audit.jsonl"))
}

/// Every token's issuer: the configured one, else this server's URL.
pub fn issuer(config: &Config, c: &ConnectorsConfig) -> String {
    c.issuer.clone().unwrap_or_else(|| {
        let scheme = match config.tls {
            Some(_) => "https",
            None => "http",
        };
        format!("{scheme}://{}", config.listen)
    })
}

/// The gateway for the served repository `repo`, if connectors are on.
pub fn gateway_for(config: &Config, repo: &str) -> Option<Gateway> {
    let c = config.connectors.as_ref()?;
    Some(Gateway {
        url: c.gateway.clone(),
        sandbox_url: c.sandbox_gateway.clone(),
        issuer: issuer(config, c),
        key_file: key_file(config, c),
        jwks_file: Some(jwks_file(config)),
        audit_file: Some(audit_file(config, c)),
        // Every branch records the principal that created it; these are
        // for one that does not.
        subject: "server".to_owned(),
        tenant: DEFAULT_TENANT.to_owned(),
        branch_scope: Some(repo.to_owned()),
        max_ttl: MAX_TTL,
        packager: Arc::new(AnvilPackager {
            command: c.anvil.clone(),
            root: c.bundles.clone(),
        }),
    })
}

/// The public key set, made with the key on first use.
pub fn jwks(config: &Config) -> Result<serde_json::Value, String> {
    let c = config
        .connectors
        .as_ref()
        .ok_or("this server has no connector gateway")?;
    KeyRing::load_or_create(&key_file(config, c), Some(&jwks_file(config)))
        .and_then(|ring| ring.jwks())
        .map_err(|e| e.to_string())
}

/// Start the gateway beside the server when `run_gateway` asks for it.
pub fn start(config: &Config) -> Result<Option<Supervisor>, String> {
    let Some(c) = config.connectors.as_ref().filter(|c| c.run_gateway) else {
        return Ok(None);
    };
    let dir = dir(config);
    // The keys and the public set the gateway reads.
    jwks(config)?;
    let vault_key = c.vault_key.clone().unwrap_or_else(|| dir.join("vault.key"));
    gateway::ensure_vault_key(&vault_key).map_err(|e| e.to_string())?;
    let (host, port) = gateway::host_port(&c.gateway)
        .ok_or_else(|| format!("connectors.gateway {} has no host and port", c.gateway))?;
    let command = GatewayCommand {
        anvil: c.anvil.clone(),
        bundles: c.bundles.clone(),
        port,
        host: c.listen.clone().or_else(|| {
            (!matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1")).then_some(host)
        }),
        issuer: issuer(config, c),
        audience: c.gateway.clone(),
        jwks_uri: format!("file://{}", jwks_file(config).display()),
        audit_file: audit_file(config, c),
        vault_key_file: vault_key,
        vault_dir: Some(dir.join("vault")),
        public_url: None,
    };
    let log = dir.join("gateway.log");
    tracing::info!(gateway = %c.gateway, log = %log.display(), "running the connector gateway");
    // The audit log is read by each repository's poller.
    Supervisor::start(command, log, Box::new(|| {}))
        .map(Some)
        .map_err(|e| e.to_string())
}
