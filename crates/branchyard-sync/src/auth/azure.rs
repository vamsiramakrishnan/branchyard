//! Azure Storage authorization: Shared Key (the account key, signing each
//! request with HMAC-SHA256 over the documented string-to-sign), a SAS
//! token appended to each URL, or a managed identity's bearer token (the
//! App Service `IDENTITY_ENDPOINT`/`IDENTITY_HEADER`, else the instance
//! metadata service). The same token flow serves Key Vault with another
//! resource.

use branchyard_support::LockExt as _;
use std::sync::Mutex;

use ring::hmac;

use crate::error::{Error, Result};
use crate::http::{send, Request, Url};
use crate::util::{b64, unb64, uri_decode, uri_encode};

/// The Storage service version every request names.
pub const VERSION: &str = "2021-08-06";

/// The canonical string Shared Key signs, for an account.
pub fn string_to_sign(request: &Request, account: &str) -> String {
    let header = |name: &str| request.find(name).unwrap_or("").to_owned();
    let length = match request.body.as_ref().map(Vec::len) {
        Some(n) if n > 0 => n.to_string(),
        _ => String::new(),
    };
    let mut ms: Vec<(String, String)> = request
        .headers
        .iter()
        .filter(|(n, _)| n.to_ascii_lowercase().starts_with("x-ms-"))
        .map(|(n, v)| (n.to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    ms.sort();
    let headers: String = ms.iter().map(|(n, v)| format!("{n}:{v}\n")).collect();
    let mut resource = format!("/{account}{}", request.url.path);
    let mut params: Vec<(String, Vec<String>)> = Vec::new();
    for pair in request.url.query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let k = uri_decode(k, false).to_ascii_lowercase();
        let v = uri_decode(v, false);
        match params.iter_mut().find(|(name, _)| *name == k) {
            Some((_, values)) => values.push(v),
            None => params.push((k, vec![v])),
        }
    }
    params.sort();
    for (k, mut values) in params {
        values.sort();
        resource.push_str(&format!("\n{k}:{}", values.join(",")));
    }
    format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}{}",
        request.method,
        header("content-encoding"),
        header("content-language"),
        length,
        header("content-md5"),
        header("content-type"),
        header("date"),
        header("if-modified-since"),
        header("if-match"),
        header("if-none-match"),
        header("if-unmodified-since"),
        header("range"),
        headers,
        resource
    )
}

/// The `Authorization` value for `request` under `key` (the account key,
/// base64 as the portal shows it).
pub fn shared_key(request: &Request, account: &str, key: &str) -> Result<String> {
    let secret = unb64(key).map_err(|_| Error::config("the Azure account key is not base64"))?;
    let sig = hmac::sign(
        &hmac::Key::new(hmac::HMAC_SHA256, &secret),
        string_to_sign(request, account).as_bytes(),
    );
    Ok(format!("SharedKey {account}:{}", b64(sig.as_ref())))
}

/// Check a received request's Shared Key signature (for the stand-in).
pub fn verify(request: &Request, account: &str, key: &str) -> std::result::Result<(), String> {
    let given = request.find("authorization").ok_or("no Authorization")?;
    let mut copy = request.clone();
    copy.headers
        .retain(|(n, _)| !n.eq_ignore_ascii_case("authorization"));
    let want = shared_key(&copy, account, key).map_err(|e| e.to_string())?;
    match want == given {
        true => Ok(()),
        false => Err(format!(
            "signature mismatch for:\n{}",
            string_to_sign(&copy, account)
        )),
    }
}

/// How requests are authorized.
pub enum Credential {
    SharedKey { key: String },
    Sas { token: String },
    Identity(ManagedIdentity),
    Anonymous,
}

impl Credential {
    /// From the environment: `AZURE_STORAGE_KEY`, `AZURE_STORAGE_SAS_TOKEN`,
    /// else a managed identity.
    pub fn from_env() -> Credential {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        if let Some(key) = var("AZURE_STORAGE_KEY") {
            Credential::SharedKey { key }
        } else if let Some(token) = var("AZURE_STORAGE_SAS_TOKEN") {
            Credential::Sas {
                token: token.trim_start_matches('?').to_owned(),
            }
        } else {
            Credential::Identity(ManagedIdentity::new("https://storage.azure.com/"))
        }
    }
}

/// A managed identity's tokens for one resource, cached.
pub struct ManagedIdentity {
    resource: String,
    endpoint: Option<(String, String)>,
    cached: Mutex<Option<(String, u64)>>,
}

impl ManagedIdentity {
    pub fn new(resource: &str) -> ManagedIdentity {
        ManagedIdentity {
            resource: resource.to_owned(),
            endpoint: None,
            cached: Mutex::new(None),
        }
    }

    /// Ask the identity endpoint at `endpoint` with `header` as its
    /// secret, instead of reading `IDENTITY_ENDPOINT`/`IDENTITY_HEADER`.
    pub fn with_endpoint(mut self, endpoint: &str, header: &str) -> ManagedIdentity {
        self.endpoint = Some((endpoint.to_owned(), header.to_owned()));
        self
    }

    pub fn token(&self, now_ms: u64) -> Result<String> {
        let mut cached = self.cached.lock_recovering("cached");
        if let Some((token, expires)) = cached.as_ref() {
            if *expires > now_ms + 60_000 {
                return Ok(token.clone());
            }
        }
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let resource = uri_encode(&self.resource, false);
        let configured = self
            .endpoint
            .clone()
            .map(|(e, h)| (Some(e), Some(h)))
            .unwrap_or_else(|| (var("IDENTITY_ENDPOINT"), var("IDENTITY_HEADER")));
        let request = match configured {
            (Some(endpoint), Some(header)) => Request::new(
                "GET",
                Url::parse(&format!(
                    "{endpoint}?api-version=2019-08-01&resource={resource}"
                ))?,
            )
            .header("X-IDENTITY-HEADER", header),
            _ => Request::new(
                "GET",
                Url::parse(&format!(
                    "http://169.254.169.254/metadata/identity/oauth2/token?api-version=2018-02-01&resource={resource}"
                ))?,
            )
            .header("Metadata", "true"),
        };
        let response = send(&request).map_err(|e| {
            Error::config(format!(
                "no Azure credentials (set AZURE_STORAGE_KEY or AZURE_STORAGE_SAS_TOKEN, or run \
                 with a managed identity): {e}"
            ))
        })?;
        if !response.ok() {
            return Err(response.error("managed identity token"));
        }
        let value = response.json()?;
        let token = value
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::refused("the identity response has no access_token"))?
            .to_owned();
        // `expires_on` is seconds since the epoch, as a number or text.
        let expires = value
            .get("expires_on")
            .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
            .map(|s| s * 1000)
            .unwrap_or(now_ms + 300_000);
        *cached = Some((token.clone(), expires));
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request signed here matches the signature an independent
    /// implementation of the documented string-to-sign (Python's `hmac`
    /// over the same fields) produced.
    #[test]
    fn shared_key_matches_an_independent_signer() {
        let request = Request::new(
            "PUT",
            Url::parse(
                "http://127.0.0.1:10000/devstoreaccount1/sync/tasks/a%20b?comp=block&blockid=QUFB",
            )
            .unwrap(),
        )
        .header("x-ms-version", VERSION)
        .header("x-ms-date", "Sun, 30 Aug 2015 12:36:00 GMT")
        .header("x-ms-blob-type", "BlockBlob")
        .header("If-None-Match", "*")
        .header("Content-Type", "application/octet-stream")
        .body(b"hello".to_vec());
        let sts = string_to_sign(&request, "devstoreaccount1");
        assert_eq!(
            sts,
            "PUT\n\n\n5\n\napplication/octet-stream\n\n\n\n*\n\n\nx-ms-blob-type:BlockBlob\n\
             x-ms-date:Sun, 30 Aug 2015 12:36:00 GMT\nx-ms-version:2021-08-06\n\
             /devstoreaccount1/devstoreaccount1/sync/tasks/a%20b\nblockid:QUFB\ncomp:block"
        );
        let key = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
        assert_eq!(
            shared_key(&request, "devstoreaccount1", key).unwrap(),
            AZURITE_SIGNATURE
        );
        let mut signed = request.clone();
        signed
            .headers
            .push(("Authorization".into(), AZURITE_SIGNATURE.into()));
        verify(&signed, "devstoreaccount1", key).unwrap();
        assert!(verify(&signed, "devstoreaccount1", "AAAA").is_err());
    }

    const AZURITE_SIGNATURE: &str =
        "SharedKey devstoreaccount1:4oG4c5fHDBJRQPKFLlNwnES7uBbpcjzq1lfS/ve/Bo4=";
}
