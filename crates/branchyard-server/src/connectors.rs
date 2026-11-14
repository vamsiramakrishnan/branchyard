//! The connector gateway on a server: each served repository's yard gets
//! the gateway (signing with the server's key, its branches named
//! `<repo>/<branch>` in tokens and the audit log), the public keys are
//! served at `GET /.well-known/jwks.json`, and, with `run_gateway`, Anvil's
//! gateway runs beside the server, supervised. See `docs/connectors.md`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use branchyard_support::LockExt as _;

use branchyard::connectors::gateway::{self, GatewayCommand, Supervisor};
use branchyard::connectors::{
    AnvilPackager, Gateway, KeyFileSigner, KeyRing, TokenSigner, MAX_TTL,
};

use crate::config::{Config, ConnectorsConfig, KmsSigning, DEFAULT_TENANT};

fn dir(config: &Config) -> PathBuf {
    config.data_dir.join("gateway")
}

/// The signing key file.
pub fn key_file(config: &Config, c: &ConnectorsConfig) -> PathBuf {
    c.signing_key
        .clone()
        .unwrap_or_else(|| dir(config).join("key"))
}

/// What signs for the server's gateway, or `None` for the key file made
/// on first use: Cloud KMS when `signing` names it (one signer per
/// configuration, so its public keys are fetched once), and for a worker
/// the existing key file, which it never makes.
pub fn token_signer(config: &Config, c: &ConnectorsConfig) -> Option<Arc<dyn TokenSigner>> {
    if let Some(kms) = &c.signing_kms {
        return kms_signer(kms).map(|s| s as Arc<dyn TokenSigner>);
    }
    config
        .worker_only
        .then(|| Arc::new(KeyFileSigner::existing(key_file(config, c))) as Arc<dyn TokenSigner>)
}

type KmsKey = (Vec<String>, Option<String>);

fn kms_signer(kms: &KmsSigning) -> Option<Arc<crate::kms::KmsSigner>> {
    static SIGNERS: OnceLock<Mutex<BTreeMap<KmsKey, Arc<crate::kms::KmsSigner>>>> = OnceLock::new();
    let mut signers = SIGNERS
        .get_or_init(Mutex::default)
        .lock_recovering("kms signers");
    let key = (kms.key_versions.clone(), kms.base_url.clone());
    if let Some(signer) = signers.get(&key) {
        return Some(Arc::clone(signer));
    }
    let signer = Arc::new(
        crate::kms::KmsSigner::with_metadata_token(kms.base_url.as_deref(), &kms.key_versions)
            .inspect_err(|e| tracing::error!(error = %e, "connectors.signing is not usable"))
            .ok()?,
    );
    signers.insert(key, Arc::clone(&signer));
    Some(signer)
}

/// Refuse to start a worker whose signing key file is missing: it reads
/// the server's key and never makes one. Nothing to check elsewhere (a
/// server makes its key on first use; KMS keys are checked on first use).
pub fn check_signer(config: &Config) -> Result<(), String> {
    let Some(c) = config.connectors.as_ref().filter(|_| config.worker_only) else {
        return Ok(());
    };
    match token_signer(config, c) {
        Some(signer) if c.signing_kms.is_none() => signer
            .kid()
            .map(drop)
            .map_err(|e| format!("this worker cannot sign gateway tokens: {e}")),
        _ => Ok(()),
    }
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

/// Every token's issuer: the configured one, else this server's URL (a
/// configuration with a worker or a gateway off loopback must set one;
/// [`Config::validate`] refuses it otherwise).
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
        signer: token_signer(config, c),
        audit_file: Some(audit_file(config, c)),
        // Every branch records the principal that created it; these are
        // for one that does not.
        subject: "server".to_owned(),
        tenant: DEFAULT_TENANT.to_owned(),
        branch_scope: Some(repo.to_owned()),
        max_ttl: MAX_TTL,
        token_ttl: branchyard::connectors::RENEWED_TTL,
        packager: Arc::new(AnvilPackager {
            command: c.anvil.clone(),
            root: c.bundles.clone(),
        }),
        effects: branchyard::connectors::EffectsProxy {
            enabled: c.effects_proxy.unwrap_or(true),
            listen: c
                .effects_listen
                .unwrap_or(std::net::Ipv4Addr::LOCALHOST.into()),
            sandbox_host: c.effects_sandbox_host.clone(),
        },
        credential_proxy: credential_proxy(config, c),
    })
}

/// How sandboxes hold the gateway's credentials: through a proxy in the
/// sandbox that fetches tokens from this server, when `sandbox_server`
/// names it.
fn credential_proxy(
    config: &Config,
    c: &ConnectorsConfig,
) -> branchyard::connectors::CredentialProxy {
    let defaults = branchyard::connectors::CredentialProxy::default();
    branchyard::connectors::CredentialProxy {
        server: c.sandbox_server.clone(),
        audience: sandbox_audience(config, c),
        guest_command: c.sandbox_by.clone().unwrap_or(defaults.guest_command),
        sidecar_image: c.sandbox_proxy_image.clone(),
        ..defaults
    }
}

/// The audience a sandbox's Google ID token is for: the configured one,
/// else the issuer.
pub fn sandbox_audience(config: &Config, c: &ConnectorsConfig) -> String {
    c.sandbox_audience
        .clone()
        .unwrap_or_else(|| issuer(config, c))
}

/// The public key set, made with the key on first use.
pub fn jwks(config: &Config) -> Result<serde_json::Value, String> {
    let Some(c) = config.connectors.as_ref() else {
        // Without connectors, the model gateway's key file, which signs the
        // tokens an AIPlex backend is called with.
        if !crate::models::signs_for_aiplex(config) {
            return Err("this server has no connector gateway".into());
        }
        return KeyRing::load_or_create(&dir(config).join("key"), Some(&jwks_file(config)))
            .and_then(|ring| ring.jwks())
            .map_err(|e| e.to_string());
    };
    match token_signer(config, c) {
        Some(signer) => signer.jwks(),
        None => KeyRing::load_or_create(&key_file(config, c), Some(&jwks_file(config)))
            .and_then(|ring| ring.jwks()),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard::connectors::keys::verify;

    fn worker(dir: &std::path::Path, key: Option<PathBuf>) -> Config {
        let mut config = Config::new(dir.join("data"));
        config.worker_only = true;
        config.connectors = Some(ConnectorsConfig {
            gateway: "https://gateway.example/mcp".into(),
            sandbox_gateway: None,
            issuer: Some("https://by.example".into()),
            signing_key: key,
            signing_kms: None,
            audit_file: None,
            bundles: dir.join("bundles"),
            anvil: vec!["anvil".into()],
            run_gateway: false,
            listen: None,
            vault_key: None,
            effects_proxy: None,
            effects_listen: None,
            effects_sandbox_host: None,
            sandbox_server: None,
            sandbox_audience: None,
            sandbox_identities: Vec::new(),
            sandbox_jwks_url: None,
            sandbox_proxy_image: None,
            sandbox_by: None,
        });
        config
    }

    #[test]
    fn a_worker_reads_the_servers_key_file_and_never_makes_one() {
        let temp = tempfile::Builder::new()
            .prefix("branchyard-connectors-")
            .tempdir()
            .unwrap();
        let key = temp.path().join("shared.key");
        let config = worker(temp.path(), Some(key.clone()));
        // No file: the worker refuses to start, and nothing is made.
        let error = check_signer(&config).unwrap_err();
        assert!(error.contains("cannot sign gateway tokens"), "{error}");
        assert!(jwks(&config).is_err());
        let gateway = gateway_for(&config, "app").unwrap();
        assert!(gateway
            .connect_token(None, std::time::Duration::from_secs(60))
            .is_err());
        assert!(!key.exists());
        assert!(!temp.path().join("data").exists());

        // The server's file: the worker signs with it, and the token
        // verifies against the server's public set.
        let ring = KeyRing::load_or_create(&key, None).unwrap();
        assert_eq!(check_signer(&config), Ok(()));
        let gateway = gateway_for(&config, "app").unwrap();
        let token = gateway
            .connect_token(None, std::time::Duration::from_secs(60))
            .unwrap();
        assert!(verify(&ring.jwks().unwrap(), &token).is_ok());
        assert_eq!(jwks(&config).unwrap(), ring.jwks().unwrap());
    }

    #[test]
    fn a_server_still_makes_its_key_on_first_use() {
        let temp = tempfile::Builder::new()
            .prefix("branchyard-connectors-")
            .tempdir()
            .unwrap();
        let mut config = worker(temp.path(), None);
        config.worker_only = false;
        assert!(token_signer(&config, config.connectors.as_ref().unwrap()).is_none());
        let set = jwks(&config).unwrap();
        assert_eq!(set["keys"].as_array().unwrap().len(), 1);
        assert!(temp.path().join("data/gateway/key").exists());
    }
}
