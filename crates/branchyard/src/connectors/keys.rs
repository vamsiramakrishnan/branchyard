//! The yard's signing keys and the tokens they sign.
//!
//! A key file is a JSON Web Key Set of Ed25519 private keys (`kty: OKP`,
//! `crv: Ed25519`, with `d`), newest first: the first signs, the rest are
//! kept only so the gateway still accepts tokens they signed until those
//! expire. [`KeyRing::rotate`] puts a new key first. The public set, the
//! same keys without `d`, is what the gateway verifies against: locally the
//! file `.branchyard/gateway/jwks.json`, on a server
//! `GET /.well-known/jwks.json`.
//!
//! A token is a compact JWS (`alg: EdDSA`, `typ: JWT`, `kid`) over
//! [`Claims`], signed with `ring`'s Ed25519.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine as _;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{self, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::GrantEntry;
use crate::Error;

/// The `by_purpose` of a connect token: the person's own token for the
/// gateway's connect routes, which take nothing else and which a turn token
/// (no `by_purpose`) is refused at.
pub const CONNECT_PURPOSE: &str = "connect";

/// A token's claims, as `docs/connectors.md` lists them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    pub iss: String,
    pub aud: String,
    pub sub: String,
    pub iat: u64,
    pub exp: u64,
    pub jti: String,
    pub by_tenant: String,
    pub by_branch: String,
    pub by_turn: String,
    pub by_grants: Vec<GrantEntry>,
    /// [`CONNECT_PURPOSE`] on a connect token; absent on a turn's token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_purpose: Option<String>,
    /// The models the turn may call through the model gateway, as globs
    /// over model ids; absent when its branch is not on the gateway. See
    /// `docs/model-gateway.md#one-scope`. Anvil ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_models: Option<Vec<String>>,
    /// The turn's effective network policy and its digest. Anvil ignores
    /// it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_network: Option<crate::access::NetworkScope>,
    /// What the turn's branch may delegate. Anvil ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_delegation: Option<crate::access::DelegationScope>,
}

/// One Ed25519 key: its id and 32-byte seed.
#[derive(Clone)]
struct Key {
    kid: String,
    seed: [u8; 32],
}

impl Key {
    fn generate() -> Result<Key, Error> {
        let rng = SystemRandom::new();
        let mut seed = [0u8; 32];
        rng.fill(&mut seed)
            .map_err(|_| Error::State("could not generate a signing key".into()))?;
        let mut id = [0u8; 9];
        rng.fill(&mut id)
            .map_err(|_| Error::State("could not generate a key id".into()))?;
        Ok(Key {
            kid: format!("by-{}", B64.encode(id)),
            seed,
        })
    }

    fn pair(&self) -> Result<Ed25519KeyPair, Error> {
        Ed25519KeyPair::from_seed_unchecked(&self.seed)
            .map_err(|_| Error::State(format!("signing key {} is not usable", self.kid)))
    }

    fn public(&self) -> Result<Value, Error> {
        let pair = self.pair()?;
        Ok(json!({
            "kty": "OKP",
            "crv": "Ed25519",
            "kid": self.kid,
            "use": "sig",
            "alg": "EdDSA",
            "x": B64.encode(pair.public_key().as_ref()),
        }))
    }
}

/// The yard's signing keys, newest (the one that signs) first.
#[derive(Clone)]
pub struct KeyRing {
    keys: Vec<Key>,
}

impl std::fmt::Debug for KeyRing {
    /// Key ids only.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyRing")
            .field("kids", &self.kids())
            .finish()
    }
}

impl KeyRing {
    /// A ring with one new key.
    pub fn generate() -> Result<KeyRing, Error> {
        Ok(KeyRing {
            keys: vec![Key::generate()?],
        })
    }

    /// The key ids, the signing one first.
    pub fn kids(&self) -> Vec<String> {
        self.keys.iter().map(|k| k.kid.clone()).collect()
    }

    /// Read a key file.
    pub fn load(path: &Path) -> Result<KeyRing, Error> {
        let text = fs::read_to_string(path).map_err(|e| {
            Error::State(format!(
                "could not read the signing key {}: {e}",
                path.display()
            ))
        })?;
        KeyRing::parse(&text)
            .map_err(|e| Error::State(format!("signing key {}: {e}", path.display())))
    }

    fn parse(text: &str) -> Result<KeyRing, String> {
        let set: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let keys = set
            .get("keys")
            .and_then(Value::as_array)
            .ok_or("not a JSON Web Key Set (no \"keys\")")?;
        let mut ring = Vec::new();
        for key in keys {
            let field = |name: &str| key.get(name).and_then(Value::as_str);
            if field("kty") != Some("OKP") || field("crv") != Some("Ed25519") {
                return Err("every key must be an Ed25519 key (kty OKP, crv Ed25519)".into());
            }
            let kid = field("kid")
                .filter(|k| !k.is_empty())
                .ok_or("a key has no kid")?;
            let d = field("d").ok_or_else(|| format!("key {kid} has no private part (d)"))?;
            let seed: [u8; 32] = B64
                .decode(d)
                .ok()
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or_else(|| format!("key {kid}: d is not a 32-byte base64url seed"))?;
            let key = Key {
                kid: kid.to_owned(),
                seed,
            };
            key.pair().map_err(|e| e.to_string())?;
            if ring.iter().any(|k: &Key| k.kid == key.kid) {
                return Err(format!("key id {kid} appears twice"));
            }
            ring.push(key);
        }
        if ring.is_empty() {
            return Err("the key set is empty".into());
        }
        Ok(KeyRing { keys: ring })
    }

    /// The private key set, for the key file.
    fn private(&self) -> Result<Value, Error> {
        let mut keys = Vec::new();
        for key in &self.keys {
            let mut jwk = key.public()?;
            jwk["d"] = json!(B64.encode(key.seed));
            keys.push(jwk);
        }
        Ok(json!({ "keys": keys }))
    }

    /// The public key set the gateway verifies against.
    pub fn jwks(&self) -> Result<Value, Error> {
        let keys = self
            .keys
            .iter()
            .map(Key::public)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json!({ "keys": keys }))
    }

    /// Put a new signing key first and keep at most `keep` older ones.
    pub fn rotate(&mut self, keep: usize) -> Result<String, Error> {
        let key = Key::generate()?;
        let kid = key.kid.clone();
        self.keys.insert(0, key);
        self.keys.truncate(keep + 1);
        Ok(kid)
    }

    /// Write the key file (0600, its directory 0700) and, when given, the
    /// public set beside it.
    pub fn save(&self, path: &Path, jwks: Option<&Path>) -> Result<(), Error> {
        let text = serde_json::to_string_pretty(&self.private()?)
            .map_err(|e| Error::State(e.to_string()))?;
        write_private(path, text.as_bytes())?;
        if let Some(jwks) = jwks {
            let public = serde_json::to_string_pretty(&self.jwks()?)
                .map_err(|e| Error::State(e.to_string()))?;
            write_atomic(jwks, public.as_bytes(), 0o644)?;
        }
        Ok(())
    }

    /// The key file at `path`, made with one new key if there is none, and
    /// the public set at `jwks` written whenever it is missing or stale.
    pub fn load_or_create(path: &Path, jwks: Option<&Path>) -> Result<KeyRing, Error> {
        if !path.exists() {
            let ring = KeyRing::generate()?;
            ring.save(path, jwks)?;
            return Ok(ring);
        }
        let ring = KeyRing::load(path)?;
        if let Some(jwks) = jwks {
            let want = ring.jwks()?;
            let have = fs::read_to_string(jwks)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok());
            if have.as_ref() != Some(&want) {
                let text =
                    serde_json::to_string_pretty(&want).map_err(|e| Error::State(e.to_string()))?;
                write_atomic(jwks, text.as_bytes(), 0o644)?;
            }
        }
        Ok(ring)
    }

    /// Sign `claims` with the first key: a compact JWS.
    pub fn sign(&self, claims: &Claims) -> Result<String, Error> {
        let key = &self.keys[0];
        let header = json!({ "alg": "EdDSA", "typ": "JWT", "kid": key.kid });
        let encode = |value: &Value| B64.encode(value.to_string());
        let claims = serde_json::to_value(claims).map_err(|e| Error::State(e.to_string()))?;
        let input = format!("{}.{}", encode(&header), encode(&claims));
        let signature = key.pair()?.sign(input.as_bytes());
        Ok(format!("{input}.{}", B64.encode(signature.as_ref())))
    }
}

/// Verify a compact JWS against a public key set and return its claims.
/// Checks the header (`alg: EdDSA`, a `kid` in the set) and the signature
/// only; the caller checks `exp`, `aud` and the rest. What the gateway does,
/// for tests and `by gateway token --verify`.
pub fn verify(jwks: &Value, token: &str) -> Result<Value, String> {
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(sig), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("not a compact JWS".into());
    };
    let decode = |part: &str| -> Result<Value, String> {
        let bytes = B64.decode(part).map_err(|e| e.to_string())?;
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())
    };
    let head = decode(header)?;
    if head.get("alg").and_then(Value::as_str) != Some("EdDSA") {
        return Err("alg is not EdDSA".into());
    }
    let kid = head
        .get("kid")
        .and_then(Value::as_str)
        .ok_or("no kid in the header")?;
    let key = jwks
        .get("keys")
        .and_then(Value::as_array)
        .and_then(|keys| {
            keys.iter()
                .find(|k| k.get("kid").and_then(Value::as_str) == Some(kid))
        })
        .ok_or_else(|| format!("no key {kid} in the key set"))?;
    let x = key
        .get("x")
        .and_then(Value::as_str)
        .and_then(|x| B64.decode(x).ok())
        .ok_or("the key has no x")?;
    let sig = B64.decode(sig).map_err(|e| e.to_string())?;
    UnparsedPublicKey::new(&signature::ED25519, x)
        .verify(format!("{header}.{payload}").as_bytes(), &sig)
        .map_err(|_| "bad signature".to_owned())?;
    decode(payload)
}

/// A fresh random identifier (`jti`, a yard id): 16 bytes, base64url.
pub fn random_id() -> Result<String, Error> {
    let mut bytes = [0u8; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| Error::State("could not generate a random id".into()))?;
    Ok(B64.encode(bytes))
}

/// Write `bytes` to `path` with mode 0600, through a temporary file and a
/// rename, making its directory (0700) if needed.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    if let Some(dir) = path.parent() {
        if !dir.exists() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|e| Error::State(format!("create {}: {e}", dir.display())))?;
        }
    }
    write_atomic(path, bytes, 0o600)
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<(), Error> {
    let fail = |e: std::io::Error| Error::State(format!("write {}: {e}", path.display()));
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(fail)?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", random_id()?));
    let written = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
        fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written.map_err(fail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims() -> Claims {
        Claims {
            iss: "branchyard:local:y".into(),
            aud: "http://127.0.0.1:8931/mcp".into(),
            sub: "local:me".into(),
            iat: 1,
            exp: 2,
            jti: "j".into(),
            by_tenant: "local".into(),
            by_branch: "b".into(),
            by_turn: "1".into(),
            by_grants: vec![GrantEntry::read("github")],
            by_purpose: None,
            by_models: None,
            by_network: None,
            by_delegation: None,
        }
    }

    #[test]
    fn the_new_scopes_leave_the_contract_claims_as_they_were() {
        // Without the new scopes, a token's claims are exactly the
        // contract's (what Anvil parses), field for field.
        let wire = serde_json::to_value(claims()).unwrap();
        let mut keys: Vec<&str> = wire
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "aud",
                "by_branch",
                "by_grants",
                "by_tenant",
                "by_turn",
                "exp",
                "iat",
                "iss",
                "jti",
                "sub"
            ]
        );
        // Claims written before them still parse.
        let old = r#"{"iss":"i","aud":"a","sub":"s","iat":1,"exp":2,"jti":"j","by_tenant":"t",
            "by_branch":"b","by_turn":"1","by_grants":[]}"#;
        let parsed: Claims = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.by_models, None);
        // With them, the contract's claims are unchanged and the new ones
        // are extra keys of plain JSON types, which Anvil's verifier
        // ignores (it reads only the contract's claims, and refuses only a
        // malformed `scope`, `scp`, `aud`, `exp` or `nbf`).
        let mut scoped = claims();
        scoped.by_models = Some(vec!["claude-*".into()]);
        scoped.by_network = Some(crate::access::NetworkScope::of(None));
        scoped.by_delegation = Some(crate::access::DelegationScope {
            depth: 0,
            max_depth: 1,
            max_children: 4,
            harnesses: Vec::new(),
        });
        let with = serde_json::to_value(&scoped).unwrap();
        for key in keys {
            assert_eq!(with[key], wire[key], "{key}");
        }
        assert_eq!(with["by_models"], json!(["claude-*"]));
        assert_eq!(with["by_network"]["policy"], "open");
        assert_eq!(with["by_delegation"]["max_depth"], 1);
        for key in ["scope", "scp", "nbf"] {
            assert!(with.get(key).is_none(), "{key}");
        }
        let ring = KeyRing::generate().unwrap();
        let verified = verify(&ring.jwks().unwrap(), &ring.sign(&scoped).unwrap()).unwrap();
        assert_eq!(serde_json::from_value::<Claims>(verified).unwrap(), scoped);
    }

    #[test]
    fn tokens_verify_against_the_public_set_and_nothing_else() {
        let ring = KeyRing::generate().unwrap();
        let token = ring.sign(&claims()).unwrap();
        let jwks = ring.jwks().unwrap();
        let verified = verify(&jwks, &token).unwrap();
        assert_eq!(verified["by_grants"][0]["connector"], "github");
        assert_eq!(verified["aud"], "http://127.0.0.1:8931/mcp");
        // The public set holds no private part.
        assert!(!jwks.to_string().contains("\"d\""));
        // Another key's set refuses it; so does a changed payload.
        let other = KeyRing::generate().unwrap().jwks().unwrap();
        assert!(verify(&other, &token).is_err());
        let parts: Vec<&str> = token.split('.').collect();
        let forged = format!(
            "{}.{}.{}",
            parts[0],
            B64.encode(r#"{"sub":"someone-else"}"#),
            parts[2]
        );
        assert!(verify(&jwks, &forged).unwrap_err().contains("signature"));
        let header: Value = serde_json::from_slice(&B64.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["alg"], "EdDSA");
        assert_eq!(header["typ"], "JWT");
        assert_eq!(header["kid"], ring.kids()[0].as_str());
    }

    #[test]
    fn rotation_keeps_old_tokens_verifiable_until_the_old_key_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let (key, jwks_path) = (dir.path().join("gw/key"), dir.path().join("gw/jwks.json"));
        let mut ring = KeyRing::load_or_create(&key, Some(&jwks_path)).unwrap();
        let mode = fs::metadata(&key).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = fs::metadata(key.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        let old = ring.sign(&claims()).unwrap();
        let first = ring.kids()[0].clone();
        let second = ring.rotate(1).unwrap();
        ring.save(&key, Some(&jwks_path)).unwrap();
        let reread = KeyRing::load_or_create(&key, Some(&jwks_path)).unwrap();
        assert_eq!(reread.kids(), [second.clone(), first]);
        let jwks: Value = serde_json::from_str(&fs::read_to_string(&jwks_path).unwrap()).unwrap();
        assert!(verify(&jwks, &old).is_ok());
        let new = reread.sign(&claims()).unwrap();
        assert!(verify(&jwks, &new).is_ok());
        // A second rotation with keep 1 drops the first key.
        let mut ring = reread;
        ring.rotate(1).unwrap();
        ring.save(&key, Some(&jwks_path)).unwrap();
        let jwks: Value = serde_json::from_str(&fs::read_to_string(&jwks_path).unwrap()).unwrap();
        assert!(verify(&jwks, &old).is_err());
        assert!(verify(&jwks, &new).is_ok());
        assert_eq!(ring.kids()[1], second);
    }

    #[test]
    fn a_bad_key_file_is_refused_by_reason() {
        for (text, why) in [
            ("{}", "no \"keys\""),
            (r#"{"keys":[]}"#, "empty"),
            (r#"{"keys":[{"kty":"RSA"}]}"#, "Ed25519"),
            (
                r#"{"keys":[{"kty":"OKP","crv":"Ed25519","kid":"a"}]}"#,
                "private",
            ),
            (
                r#"{"keys":[{"kty":"OKP","crv":"Ed25519","kid":"a","d":"AA"}]}"#,
                "32-byte",
            ),
        ] {
            let error = KeyRing::parse(text).unwrap_err();
            assert!(error.contains(why), "{text}: {error}");
        }
        assert!(!format!("{:?}", KeyRing::generate().unwrap()).contains("seed"));
    }
}
