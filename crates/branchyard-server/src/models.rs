//! The model gateway on a server: each served repository's yard gets the
//! gateway `models` configures, signing its turns' tokens with the
//! server's key (the connector gateway's, when there is one), its branches
//! named `<repo>/<branch>` in tokens and usage. A backend's key names an
//! entry of the server's `secrets`, else a variable of the server's own
//! environment. See `docs/model-gateway.md`.

use branchyard::models::{Config as Engine, Gateway, KeySource, Signer};
use branchyard::SecretFrom;

use crate::config::{Config, DEFAULT_TENANT};

/// Where the secret `name` is on this server.
fn key_source(config: &Config, name: &str) -> KeySource {
    match config.secrets.get(name).and_then(|s| s.from.as_ref()) {
        Some(SecretFrom::Env { var }) => KeySource::Env(var.clone()),
        Some(SecretFrom::File { path }) => KeySource::File(path.clone()),
        None => KeySource::Env(name.to_owned()),
    }
}

/// The gateway for the served repository `repo`; `None` when `models`
/// names no backend.
pub fn gateway_for(config: &Config, repo: &str) -> Result<Option<Gateway>, String> {
    let Some(models) = config.models.as_ref().filter(|m| !m.backends.is_empty()) else {
        return Ok(None);
    };
    let engine: Engine = serde_json::to_value(models)
        .and_then(serde_json::from_value)
        .map_err(|e| format!("models: {e}"))?;
    let signer = match config.connectors.as_ref() {
        Some(c) => Signer {
            issuer: crate::connectors::issuer(config, c),
            key_file: crate::connectors::key_file(config, c),
            jwks_file: Some(crate::connectors::jwks_file(config)),
            subject: "server".to_owned(),
            tenant: DEFAULT_TENANT.to_owned(),
        },
        None => Signer {
            issuer: {
                let scheme = match config.tls {
                    Some(_) => "https",
                    None => "http",
                };
                format!("{scheme}://{}", config.listen)
            },
            key_file: config.data_dir.join("gateway").join("key"),
            jwks_file: Some(crate::connectors::jwks_file(config)),
            subject: "server".to_owned(),
            tenant: DEFAULT_TENANT.to_owned(),
        },
    };
    let mut gateway = Gateway::new(&engine, signer, |name| key_source(config, name))
        .map_err(|e| e.to_string())?;
    gateway.branch_scope = Some(repo.to_owned());
    Ok(Some(gateway))
}
