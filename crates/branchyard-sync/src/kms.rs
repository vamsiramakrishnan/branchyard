//! Tenant key wrapping: what seals the secrets a remote's keyring holds.
//!
//! - `passphrase`: a key derived from a passphrase with PBKDF2-HMAC-SHA256
//!   (600,000 iterations by default, the OWASP 2023 figure), sealing with
//!   AES-256-GCM. Argon2id, which the design names, is not in
//!   `Cargo.lock`; the wrapped form records its algorithm and parameters,
//!   so Argon2id can be added beside it and keys rewrapped.
//! - `kms://gcp/projects/P/locations/L/keyRings/R/cryptoKeys/K`: Cloud KMS
//!   `encrypt` and `decrypt`.
//! - `kms://aws/<key ID, alias/NAME or ARN>?region=R`: AWS KMS `Encrypt`
//!   and `Decrypt` (JSON 1.1, SigV4).
//! - `kms://azure/<vault host>/keys/<name>[/<version>]`: Key Vault
//!   `wrapkey` and `unwrapkey` with RSA-OAEP-256.
//!
//! Each takes `?endpoint=` for another API root (the stand-ins in tests).

use std::num::NonZeroU32;
use std::sync::Arc;

use ring::aead;

use crate::auth::aws::AwsCredentials;
use crate::auth::azure::ManagedIdentity;
use crate::auth::google::GoogleAuth;
use crate::auth::sigv4::{self, Credentials};
use crate::error::{Error, Result};
use crate::http::{send, Request, Url};
use crate::util::{b64, b64url, random_bytes, unb64, unb64url};
use branchyard::services::Clock;

/// PBKDF2 iterations for a new passphrase-wrapped key.
pub const PBKDF2_ITERATIONS: u32 = 600_000;

/// Seals and opens a tenant's key secrets.
pub trait Wrapper: Send + Sync {
    /// `passphrase` or the KMS URL, for the keyring and for people.
    fn describe(&self) -> String;
    fn wrap(&self, secret: &[u8]) -> Result<String>;
    fn unwrap(&self, wrapped: &str) -> Result<Vec<u8>>;
}

/// AES-256-GCM with a random nonce, `nonce || ciphertext`.
pub(crate) fn aead_seal(key: &[u8], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_256_GCM, key).map_err(|_| Error::local("bad key"))?,
    );
    let nonce_bytes = random_bytes(12)?;
    let nonce = aead::Nonce::try_assume_unique_for_key(&nonce_bytes)
        .map_err(|_| Error::local("bad nonce"))?;
    let mut buf = plaintext.to_vec();
    key.seal_in_place_append_tag(nonce, aead::Aad::from(aad), &mut buf)
        .map_err(|_| Error::local("sealing failed"))?;
    let mut out = nonce_bytes;
    out.extend_from_slice(&buf);
    Ok(out)
}

pub(crate) fn aead_open(key: &[u8], aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
    if sealed.len() < 12 + 16 {
        return Err(Error::corrupt("sealed data too short"));
    }
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_256_GCM, key).map_err(|_| Error::local("bad key"))?,
    );
    let nonce = aead::Nonce::try_assume_unique_for_key(&sealed[..12])
        .map_err(|_| Error::corrupt("bad nonce"))?;
    let mut buf = sealed[12..].to_vec();
    let plain = key
        .open_in_place(nonce, aead::Aad::from(aad), &mut buf)
        .map_err(|_| Error::refused("the key does not open this: wrong key, or tampered"))?;
    Ok(plain.to_vec())
}

/// A passphrase-derived key.
pub struct Passphrase {
    passphrase: String,
    iterations: u32,
}

impl Passphrase {
    pub fn new(passphrase: &str) -> Result<Passphrase> {
        if passphrase.chars().count() < 8 {
            return Err(Error::config(
                "a sync passphrase needs at least 8 characters",
            ));
        }
        Ok(Passphrase {
            passphrase: passphrase.to_owned(),
            iterations: PBKDF2_ITERATIONS,
        })
    }

    /// Fewer iterations for new wraps: tests only.
    pub fn with_iterations(mut self, iterations: u32) -> Passphrase {
        self.iterations = iterations.max(1);
        self
    }

    fn derive(&self, salt: &[u8], iterations: u32) -> Result<[u8; 32]> {
        let mut key = [0u8; 32];
        ring::pbkdf2::derive(
            ring::pbkdf2::PBKDF2_HMAC_SHA256,
            NonZeroU32::new(iterations).ok_or_else(|| Error::corrupt("zero iterations"))?,
            salt,
            self.passphrase.as_bytes(),
            &mut key,
        );
        Ok(key)
    }
}

impl Wrapper for Passphrase {
    fn describe(&self) -> String {
        "passphrase".into()
    }

    fn wrap(&self, secret: &[u8]) -> Result<String> {
        let salt = random_bytes(16)?;
        let key = self.derive(&salt, self.iterations)?;
        let sealed = aead_seal(&key, b"branchyard-sync passphrase", secret)?;
        Ok(format!(
            "pbkdf2-sha256${}${}${}",
            self.iterations,
            b64(&salt),
            b64(&sealed)
        ))
    }

    fn unwrap(&self, wrapped: &str) -> Result<Vec<u8>> {
        let parts: Vec<&str> = wrapped.split('$').collect();
        if parts.len() != 4 || parts[0] != "pbkdf2-sha256" {
            return Err(Error::config(
                "this key was not wrapped with a passphrase (or with an algorithm this build lacks)",
            ));
        }
        let iterations: u32 = parts[1]
            .parse()
            .map_err(|_| Error::corrupt("bad iteration count"))?;
        let key = self.derive(&unb64(parts[2])?, iterations)?;
        aead_open(&key, b"branchyard-sync passphrase", &unb64(parts[3])?)
            .map_err(|_| Error::refused("the passphrase does not open this remote's keys"))
    }
}

/// Google Cloud KMS.
pub struct GcpKms {
    name: String,
    endpoint: Url,
    auth: Arc<GoogleAuth>,
    clock: Clock,
}

impl GcpKms {
    pub fn new(name: &str, endpoint: Option<&str>, auth: GoogleAuth) -> Result<GcpKms> {
        Ok(GcpKms {
            name: name.trim_matches('/').to_owned(),
            endpoint: Url::parse(endpoint.unwrap_or("https://cloudkms.googleapis.com"))?,
            auth: Arc::new(auth),
            clock: Clock::system(),
        })
    }

    fn call(&self, verb: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        let url = self.endpoint.with_path(
            &format!(
                "{}/v1/{}:{verb}",
                self.endpoint.path.trim_end_matches('/'),
                self.name
            ),
            "",
        );
        let mut request = Request::new("POST", url)
            .header("Content-Type", "application/json")
            .body(serde_json::to_vec(&body)?);
        if let Some(h) = self.auth.header(self.clock.now())? {
            request = request.header("Authorization", h);
        }
        let response = send(&request)?;
        if !response.ok() {
            return Err(response.error(&format!("Cloud KMS {verb}")));
        }
        response.json()
    }
}

impl Wrapper for GcpKms {
    fn describe(&self) -> String {
        format!("kms://gcp/{}", self.name)
    }

    fn wrap(&self, secret: &[u8]) -> Result<String> {
        let out = self.call("encrypt", serde_json::json!({"plaintext": b64(secret)}))?;
        out.get("ciphertext")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .ok_or_else(|| Error::refused("Cloud KMS returned no ciphertext"))
    }

    fn unwrap(&self, wrapped: &str) -> Result<Vec<u8>> {
        let out = self.call("decrypt", serde_json::json!({"ciphertext": wrapped}))?;
        unb64(
            out.get("plaintext")
                .and_then(|v| v.as_str())
                .ok_or_else(|| Error::refused("Cloud KMS returned no plaintext"))?,
        )
    }
}

enum AwsCreds {
    Fixed(Credentials),
    Chain(AwsCredentials),
}

/// AWS KMS.
pub struct AwsKms {
    key_id: String,
    region: String,
    endpoint: Url,
    creds: AwsCreds,
    clock: Clock,
}

impl AwsKms {
    pub fn new(key_id: &str, region: &str, endpoint: Option<&str>) -> Result<AwsKms> {
        let endpoint = match endpoint {
            Some(e) => e.to_owned(),
            None => format!("https://kms.{region}.amazonaws.com"),
        };
        Ok(AwsKms {
            key_id: key_id.to_owned(),
            region: region.to_owned(),
            endpoint: Url::parse(&endpoint)?,
            creds: AwsCreds::Chain(AwsCredentials::new(None)),
            clock: Clock::system(),
        })
    }

    pub fn with_credentials(mut self, credentials: Credentials) -> AwsKms {
        self.creds = AwsCreds::Fixed(credentials);
        self
    }

    fn call(&self, target: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        let now = self.clock.now();
        let creds = match &self.creds {
            AwsCreds::Fixed(c) => c.clone(),
            AwsCreds::Chain(chain) => chain.get(now)?,
        };
        let body = serde_json::to_vec(&body)?;
        let hash = sigv4::sha256_hex(&body);
        let path = match self.endpoint.path.as_str() {
            "" => "/",
            p => p,
        };
        let mut request = Request::new("POST", self.endpoint.with_path(path, ""))
            .header("Content-Type", "application/x-amz-json-1.1")
            .header("X-Amz-Target", format!("TrentService.{target}"))
            .body(body);
        sigv4::sign(&mut request, &creds, &self.region, "kms", now, &hash, false);
        let response = send(&request)?;
        if !response.ok() {
            return Err(response.error(&format!("AWS KMS {target}")));
        }
        response.json()
    }
}

impl Wrapper for AwsKms {
    fn describe(&self) -> String {
        format!("kms://aws/{}?region={}", self.key_id, self.region)
    }

    fn wrap(&self, secret: &[u8]) -> Result<String> {
        let out = self.call(
            "Encrypt",
            serde_json::json!({"KeyId": self.key_id, "Plaintext": b64(secret)}),
        )?;
        out.get("CiphertextBlob")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .ok_or_else(|| Error::refused("AWS KMS returned no CiphertextBlob"))
    }

    fn unwrap(&self, wrapped: &str) -> Result<Vec<u8>> {
        let out = self.call(
            "Decrypt",
            serde_json::json!({"KeyId": self.key_id, "CiphertextBlob": wrapped}),
        )?;
        unb64(
            out.get("Plaintext")
                .and_then(|v| v.as_str())
                .ok_or_else(|| Error::refused("AWS KMS returned no Plaintext"))?,
        )
    }
}

enum VaultToken {
    Fixed(String),
    Identity(ManagedIdentity),
}

/// Azure Key Vault.
pub struct AzureKeyVault {
    base: Url,
    key: String,
    version: Option<String>,
    token: VaultToken,
    clock: Clock,
}

impl AzureKeyVault {
    /// `vault` is the vault's host (`name.vault.azure.net`).
    pub fn new(
        vault: &str,
        key: &str,
        version: Option<&str>,
        endpoint: Option<&str>,
    ) -> Result<AzureKeyVault> {
        let base = match endpoint {
            Some(e) => e.to_owned(),
            None => format!("https://{vault}"),
        };
        let token = match std::env::var("AZURE_KEYVAULT_TOKEN")
            .ok()
            .filter(|v| !v.trim().is_empty())
        {
            Some(t) => VaultToken::Fixed(t),
            None => VaultToken::Identity(ManagedIdentity::new("https://vault.azure.net")),
        };
        Ok(AzureKeyVault {
            base: Url::parse(base.trim_end_matches('/'))?,
            key: key.to_owned(),
            version: version.map(str::to_owned),
            token,
            clock: Clock::system(),
        })
    }

    /// Tokens from this managed identity.
    pub fn with_identity(mut self, identity: ManagedIdentity) -> AzureKeyVault {
        self.token = VaultToken::Identity(identity);
        self
    }

    pub fn with_token(mut self, token: &str) -> AzureKeyVault {
        self.token = VaultToken::Fixed(token.to_owned());
        self
    }

    fn call(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        let token = match &self.token {
            VaultToken::Fixed(t) => t.clone(),
            VaultToken::Identity(identity) => identity.token(self.clock.now())?,
        };
        let request = Request::new(
            "POST",
            self.base.with_path(
                &format!("{}{path}", self.base.path.trim_end_matches('/')),
                "api-version=7.4",
            ),
        )
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .body(serde_json::to_vec(&body)?);
        let response = send(&request)?;
        if !response.ok() {
            return Err(response.error("Key Vault"));
        }
        response.json()
    }
}

impl Wrapper for AzureKeyVault {
    fn describe(&self) -> String {
        let mut url = format!("kms://azure/{}/keys/{}", self.base.host, self.key);
        if let Some(v) = &self.version {
            url.push('/');
            url.push_str(v);
        }
        url
    }

    fn wrap(&self, secret: &[u8]) -> Result<String> {
        let path = match &self.version {
            Some(v) => format!("/keys/{}/{v}/wrapkey", self.key),
            None => format!("/keys/{}/wrapkey", self.key),
        };
        let out = self.call(
            &path,
            serde_json::json!({"alg": "RSA-OAEP-256", "value": b64url(secret)}),
        )?;
        let kid = out.get("kid").and_then(|v| v.as_str()).unwrap_or("");
        let value = out
            .get("value")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::refused("Key Vault returned no value"))?;
        // The key's version, from its ID, so unwrapping uses the same one.
        let version = kid.rsplit('/').next().unwrap_or("");
        Ok(format!("{version}|{value}"))
    }

    fn unwrap(&self, wrapped: &str) -> Result<Vec<u8>> {
        let (version, value) = wrapped
            .split_once('|')
            .ok_or_else(|| Error::corrupt("a Key Vault wrapped key needs its version"))?;
        let path = match version.is_empty() {
            true => format!("/keys/{}/unwrapkey", self.key),
            false => format!("/keys/{}/{version}/unwrapkey", self.key),
        };
        let out = self.call(
            &path,
            serde_json::json!({"alg": "RSA-OAEP-256", "value": value}),
        )?;
        unb64url(
            out.get("value")
                .and_then(|v| v.as_str())
                .ok_or_else(|| Error::refused("Key Vault returned no value"))?,
        )
    }
}

/// The wrapper a `kms://` URL names.
pub fn from_url(url: &str) -> Result<Box<dyn Wrapper>> {
    let rest = url
        .strip_prefix("kms://")
        .ok_or_else(|| Error::config(format!("{url:?} is not a kms:// URL")))?;
    let (path, query) = match rest.split_once('?') {
        Some((p, q)) => (p, crate::util::query_pairs(q)),
        None => (rest, Vec::new()),
    };
    let get = |k: &str| {
        query
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
    };
    let endpoint = get("endpoint");
    let (provider, name) = path
        .split_once('/')
        .ok_or_else(|| Error::config(format!("{url:?} names no key")))?;
    match provider {
        "gcp" => {
            if !name.starts_with("projects/") || !name.contains("/cryptoKeys/") {
                return Err(Error::config(format!(
                    "{url:?}: use kms://gcp/projects/P/locations/L/keyRings/R/cryptoKeys/K"
                )));
            }
            Ok(Box::new(GcpKms::new(
                name,
                endpoint.as_deref(),
                GoogleAuth::from_env()?,
            )?))
        }
        "aws" => {
            let region = get("region")
                .or_else(|| std::env::var("AWS_REGION").ok())
                .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok())
                .ok_or_else(|| Error::config(format!("{url:?} needs ?region=")))?;
            Ok(Box::new(AwsKms::new(name, &region, endpoint.as_deref())?))
        }
        "azure" => {
            let parts: Vec<&str> = name.split('/').collect();
            if parts.len() < 3 || parts[1] != "keys" {
                return Err(Error::config(format!(
                    "{url:?}: use kms://azure/<vault>.vault.azure.net/keys/<name>[/<version>]"
                )));
            }
            Ok(Box::new(AzureKeyVault::new(
                parts[0],
                parts[2],
                parts.get(3).copied(),
                endpoint.as_deref(),
            )?))
        }
        other => Err(Error::config(format!(
            "{url:?}: {other:?} is not gcp, aws or azure"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passphrases_wrap_and_refuse_the_wrong_one() {
        let right = Passphrase::new("correct horse battery")
            .unwrap()
            .with_iterations(10);
        let wrapped = right.wrap(b"tenant secret").unwrap();
        assert!(wrapped.starts_with("pbkdf2-sha256$10$"));
        assert_eq!(right.unwrap(&wrapped).unwrap(), b"tenant secret");
        let wrong = Passphrase::new("incorrect horse").unwrap();
        assert_eq!(
            wrong.unwrap(&wrapped).unwrap_err().kind,
            crate::Kind::Refused
        );
        assert!(Passphrase::new("short").is_err());
    }

    #[test]
    fn kms_urls_parse() {
        assert!(
            from_url("kms://gcp/projects/p/locations/l/keyRings/r/cryptoKeys/k").is_ok()
                || std::env::var("GOOGLE_APPLICATION_CREDENTIALS").is_ok()
        );
        assert!(from_url("kms://aws/alias/sync?region=us-east-1").is_ok());
        assert!(from_url("kms://azure/v.vault.azure.net/keys/k/1").is_ok());
        assert!(from_url("kms://gcp/nope").is_err());
        assert!(from_url("kms://azure/v/k").is_err());
        assert!(from_url("kms://other/x").is_err());
        assert_eq!(
            from_url("kms://azure/v.vault.azure.net/keys/k/1")
                .unwrap()
                .describe(),
            "kms://azure/v.vault.azure.net/keys/k/1"
        );
    }
}
