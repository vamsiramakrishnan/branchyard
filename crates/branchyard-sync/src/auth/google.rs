//! Google OAuth access tokens for Cloud Storage and Cloud KMS, from the
//! first of: `GOOGLE_OAUTH_ACCESS_TOKEN` (a token minted elsewhere, used as
//! is), a service-account key (`GOOGLE_APPLICATION_CREDENTIALS`, signed
//! into an RS256 JWT and exchanged at its `token_uri`), or the metadata
//! server (`GCE_METADATA_HOST`, else `metadata.google.internal`), which is
//! how workload identity reaches a pod or VM. Tokens are cached until a
//! minute before they run out.

use std::sync::Mutex;

use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};

use crate::error::{Error, Result};
use crate::http::{send, Request, Url};
use crate::util::{b64url, uri_encode};

/// Covers Cloud Storage and Cloud KMS.
pub const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// A service-account key file's fields that matter here.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct ServiceAccount {
    pub client_email: String,
    pub private_key: String,
    #[serde(default = "default_token_uri")]
    pub token_uri: String,
}

fn default_token_uri() -> String {
    "https://oauth2.googleapis.com/token".into()
}

/// DER from the first PEM block in `pem`.
pub fn pem_der(pem: &str) -> Result<Vec<u8>> {
    let body: String = pem
        .lines()
        .skip_while(|l| !l.starts_with("-----BEGIN"))
        .skip(1)
        .take_while(|l| !l.starts_with("-----END"))
        .collect();
    crate::util::unb64(&body).map_err(|_| Error::config("the private key is not PEM"))
}

/// The signed JWT a service account exchanges for a token, issued at
/// `now_s` for an hour.
pub fn assertion(account: &ServiceAccount, scope: &str, now_s: u64) -> Result<String> {
    let header = b64url(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = format!(
        r#"{{"iss":{},"scope":{},"aud":{},"iat":{now_s},"exp":{}}}"#,
        serde_json::Value::from(account.client_email.as_str()),
        serde_json::Value::from(scope),
        serde_json::Value::from(account.token_uri.as_str()),
        now_s + 3600
    );
    let input = format!("{header}.{}", b64url(claims.as_bytes()));
    let der = pem_der(&account.private_key)?;
    let key = RsaKeyPair::from_pkcs8(&der)
        .map_err(|e| Error::config(format!("the service account's private key: {e}")))?;
    let mut sig = vec![0u8; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        input.as_bytes(),
        &mut sig,
    )
    .map_err(|_| Error::local("RSA signing failed"))?;
    Ok(format!("{input}.{}", b64url(&sig)))
}

#[derive(Clone, Debug)]
struct Token {
    value: String,
    expires_ms: Option<u64>,
}

/// Where tokens come from.
#[derive(Clone, Debug)]
enum Source {
    Fixed(String),
    Account(Box<ServiceAccount>),
    Metadata(String),
    None,
}

pub struct GoogleAuth {
    source: Source,
    cached: Mutex<Option<Token>>,
}

impl GoogleAuth {
    /// From the environment, as the module says.
    pub fn from_env() -> Result<GoogleAuth> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let source = if let Some(token) = var("GOOGLE_OAUTH_ACCESS_TOKEN") {
            Source::Fixed(token)
        } else if let Some(path) = var("GOOGLE_APPLICATION_CREDENTIALS") {
            let text = std::fs::read_to_string(&path).map_err(|e| {
                Error::config(format!("GOOGLE_APPLICATION_CREDENTIALS {path}: {e}"))
            })?;
            let account: ServiceAccount = serde_json::from_str(&text).map_err(|e| {
                Error::config(format!(
                    "GOOGLE_APPLICATION_CREDENTIALS {path}: not a service-account key: {e}"
                ))
            })?;
            Source::Account(Box::new(account))
        } else {
            let host =
                var("GCE_METADATA_HOST").unwrap_or_else(|| "metadata.google.internal".into());
            Source::Metadata(host)
        };
        Ok(GoogleAuth {
            source,
            cached: Mutex::new(None),
        })
    }

    /// Tokens from a service account's key.
    pub fn from_account(account: ServiceAccount) -> GoogleAuth {
        GoogleAuth {
            source: Source::Account(Box::new(account)),
            cached: Mutex::new(None),
        }
    }

    /// Tokens from a metadata server at `host` (`host:port`).
    pub fn from_metadata(host: &str) -> GoogleAuth {
        GoogleAuth {
            source: Source::Metadata(host.to_owned()),
            cached: Mutex::new(None),
        }
    }

    /// No credentials: for emulators.
    pub fn anonymous() -> GoogleAuth {
        GoogleAuth {
            source: Source::None,
            cached: Mutex::new(None),
        }
    }

    pub fn fixed(token: &str) -> GoogleAuth {
        GoogleAuth {
            source: Source::Fixed(token.to_owned()),
            cached: Mutex::new(None),
        }
    }

    /// The `Authorization` header value at `now_ms`, or `None` when
    /// anonymous.
    pub fn header(&self, now_ms: u64) -> Result<Option<String>> {
        if let Source::None = self.source {
            return Ok(None);
        }
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = cached.as_ref() {
            if t.expires_ms.is_none_or(|e| e > now_ms + 60_000) {
                return Ok(Some(format!("Bearer {}", t.value)));
            }
        }
        let token = match &self.source {
            Source::Fixed(t) => Token {
                value: t.clone(),
                expires_ms: None,
            },
            Source::Account(account) => {
                let jwt = assertion(account, SCOPE, now_ms / 1000)?;
                let body = format!(
                    "grant_type={}&assertion={}",
                    uri_encode("urn:ietf:params:oauth:grant-type:jwt-bearer", false),
                    jwt
                );
                let response = send(
                    &Request::new("POST", Url::parse(&account.token_uri)?)
                        .header("Content-Type", "application/x-www-form-urlencoded")
                        .body(body.into_bytes()),
                )?;
                if !response.ok() {
                    return Err(response.error("service-account token"));
                }
                token_from(&response.json()?, now_ms)?
            }
            Source::Metadata(host) => {
                let url = format!(
                    "http://{host}/computeMetadata/v1/instance/service-accounts/default/token"
                );
                let response = send(
                    &Request::new("GET", Url::parse(&url)?).header("Metadata-Flavor", "Google"),
                )
                .map_err(|e| {
                    Error::config(format!(
                        "no Google credentials (set GOOGLE_APPLICATION_CREDENTIALS or \
                                 GOOGLE_OAUTH_ACCESS_TOKEN, or run on Google Cloud): {e}"
                    ))
                })?;
                if !response.ok() {
                    return Err(response.error("metadata server token"));
                }
                token_from(&response.json()?, now_ms)?
            }
            Source::None => unreachable!(),
        };
        let header = format!("Bearer {}", token.value);
        *cached = Some(token);
        Ok(Some(header))
    }
}

fn token_from(value: &serde_json::Value, now_ms: u64) -> Result<Token> {
    let access = value
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::refused("the token response has no access_token"))?;
    let expires_in = value.get("expires_in").and_then(|v| v.as_u64());
    Ok(Token {
        value: access.to_owned(),
        expires_ms: expires_in.map(|s| now_ms + s * 1000),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const TEST_KEY: &str = include_str!("testdata/rsa-test-key.pem");

    /// RSASSA-PKCS1-v1_5 is deterministic: the JWT matches the one
    /// `openssl dgst -sha256 -sign` made from the same key and claims.
    #[test]
    fn service_account_jwt_matches_openssl() {
        let account = ServiceAccount {
            client_email: "sync@example.iam.gserviceaccount.com".into(),
            private_key: TEST_KEY.into(),
            token_uri: "https://oauth2.googleapis.com/token".into(),
        };
        let jwt = assertion(&account, SCOPE, 1_440_938_160).unwrap();
        assert_eq!(
            jwt,
            "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzeW5jQGV4YW1wbGUuaWFtLmdzZXJ2aWNlYWNjb3VudC5jb20iLCJzY29wZSI6Imh0dHBzOi8vd3d3Lmdvb2dsZWFwaXMuY29tL2F1dGgvY2xvdWQtcGxhdGZvcm0iLCJhdWQiOiJodHRwczovL29hdXRoMi5nb29nbGVhcGlzLmNvbS90b2tlbiIsImlhdCI6MTQ0MDkzODE2MCwiZXhwIjoxNDQwOTQxNzYwfQ.FovdAypPhGr15LGZXYbS8AqC6OSqf5qqjn4jjwwQXuIQb88RLgdGBKV9uWuhiNuWHrVB_bWWqebJpyD-BMN4vUMOcTiCFSif2Tcife3Y7-Qs0-8af3QsL-fOU9wwOovhA2rtOG6zAAwfxvlq0F4qfpbSPJgQndEH-YWnZSBgOez9RRJ0dPTfwMDQJtOyfmxw-ygk45Lk-VhaFoh-hrwiuriqjbrC8zf24dUEONTTQ8D2FlawnKMvDLbJjQQrDhI60H7K1KQ0K6aEY4WZXEEBegcgVBTNTHJrPPSaB_gqSMLuDoApiADaB2KtGeRPivhPkQvUq5jmSQuqC34n5hPWbQ"
        );
    }
}
