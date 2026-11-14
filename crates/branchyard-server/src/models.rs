//! The model gateway on a server: each served repository's yard gets the
//! gateway `models` configures, signing its turns' tokens with the
//! server's key (the connector gateway's, when there is one), its branches
//! named `<repo>/<branch>` in tokens and usage. A backend's key names an
//! entry of the server's `secrets`, else a variable of the server's own
//! environment. See `docs/model-gateway.md`.

use branchyard::models::{Gateway, KeySource, Signer};
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

/// Whether a model backend of this server is called with a token the
/// server signs (`auth = "turn-token"`): then its public keys are served at
/// `GET /.well-known/jwks.json`, with or without `connectors`.
pub fn signs_for_aiplex(config: &Config) -> bool {
    config.models.as_ref().is_some_and(|m| {
        m.backends
            .values()
            .any(|b| b.auth.as_deref() == Some("turn-token"))
    })
}

/// The issuer of the model gateway's tokens: the connector gateway's when
/// there is one; else, when a backend is called with a signed token, the
/// server's `public_url` (its URL as AIPlex reaches it) when set; else
/// `{scheme}://{listen}`, as it has always been.
pub fn issuer(config: &Config) -> String {
    if let Some(c) = config.connectors.as_ref() {
        return crate::connectors::issuer(config, c);
    }
    let public = config
        .triggers
        .public_url
        .as_deref()
        .filter(|_| signs_for_aiplex(config));
    match public {
        Some(url) => url.trim_end_matches('/').to_owned(),
        None => {
            let scheme = match config.tls {
                Some(_) => "https",
                None => "http",
            };
            format!("{scheme}://{}", config.listen)
        }
    }
}

/// Why this server cannot sign tokens AIPlex verifies, when a backend asks
/// for them: its issuer must be a URL AIPlex can fetch its keys from
/// ([`branchyard::models::federable_issuer`]), and a `listen` port of 0 is
/// chosen only when the server starts, so it cannot be the issuer's.
pub fn check_signed(config: &Config) -> Result<(), String> {
    let Some(models) = config.models.as_ref() else {
        return Ok(());
    };
    let Some(name) = models
        .backends
        .iter()
        .find(|(_, b)| b.auth.as_deref() == Some("turn-token"))
        .map(|(name, _)| name)
    else {
        return Ok(());
    };
    let explicit = config
        .connectors
        .as_ref()
        .is_some_and(|c| c.issuer.is_some())
        || config.triggers.public_url.is_some();
    if !explicit && config.listen.port() == 0 {
        return Err(format!(
            "models.backends.{name}.auth: a token AIPlex verifies names this server's URL as \
             its issuer, and listen names port 0, which is chosen only when the server starts; \
             set public_url (the server's URL as AIPlex reaches it) or listen on a fixed port"
        ));
    }
    branchyard::models::federable_issuer(&issuer(config)).map_err(|why| {
        format!("models.backends.{name}.auth: {why}; set public_url to its https URL")
    })
}

/// The gateway for the served repository `repo`; `None` when `models`
/// names no backend.
pub fn gateway_for(config: &Config, repo: &str) -> Result<Option<Gateway>, String> {
    let Some(models) = config.models.as_ref().filter(|m| !m.backends.is_empty()) else {
        return Ok(None);
    };
    let engine = branchyard::configure::models_config(models).map_err(|e| e.to_string())?;
    let signer = match config.connectors.as_ref() {
        Some(c) => Signer {
            issuer: crate::connectors::issuer(config, c),
            key_file: crate::connectors::key_file(config, c),
            jwks_file: Some(crate::connectors::jwks_file(config)),
            signer: crate::connectors::token_signer(config, c),
            subject: "server".to_owned(),
            tenant: DEFAULT_TENANT.to_owned(),
        },
        None => Signer {
            issuer: issuer(config),
            key_file: config.data_dir.join("gateway").join("key"),
            jwks_file: Some(crate::connectors::jwks_file(config)),
            signer: None,
            subject: "server".to_owned(),
            tenant: DEFAULT_TENANT.to_owned(),
        },
    };
    let mut gateway = Gateway::new(&engine, signer, |name| key_source(config, name))
        .map_err(|e| e.to_string())?;
    gateway.branch_scope = Some(repo.to_owned());
    Ok(Some(gateway))
}
