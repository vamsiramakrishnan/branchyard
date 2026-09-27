//! Per-attempt credentials for bridge connections.
//!
//! The host holds an Ed25519 signing key; the bridge holds only its public
//! half ([`KEY_ENV`]), so nothing inside the sandbox can mint a credential.
//! A credential names one actor by atespace, name and UID, one attempt by
//! label and sequence number, and an expiry. The bridge accepts it only for
//! its own identity, before the expiry, and only while its attempt is the
//! newest one seen and has not been ended (see [`Attempts`]).
//!
//! Encoding: `byb1.<hex payload>.<hex signature>`, where the payload is the
//! UTF-8 text `branchyard-bridge/1` followed by one `key=value` line per
//! claim, in the order of [`Claims`]' fields. Values cannot contain line
//! breaks.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};

/// The environment variable holding the bridge's verifying key, as 64 hex
/// digits. It is public: seeing it does not let anyone mint a credential.
pub const KEY_ENV: &str = "BRANCHYARD_BRIDGE_KEY";
const PREFIX: &str = "byb1";
const HEADER: &str = "branchyard-bridge/1";

/// What a credential asserts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claims {
    pub atespace: String,
    pub actor: String,
    /// The actor's UID. A branched or recreated actor has a new one, so a
    /// credential never carries over to it.
    pub uid: String,
    /// A label for the attempt, for diagnostics.
    pub attempt: String,
    /// Orders attempts: a larger number supersedes every smaller one.
    pub seq: u64,
    /// Unix seconds after which the credential is refused.
    pub expires: u64,
}

/// Why a credential was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    Missing,
    Malformed(String),
    BadSignature,
    Expired,
    /// Issued for another actor, or for an earlier actor with this name.
    WrongActor,
    /// A newer attempt has been seen.
    Superseded,
    /// Its attempt was ended.
    Ended,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Missing => f.write_str("no bridge credential"),
            Refusal::Malformed(why) => write!(f, "malformed bridge credential: {why}"),
            Refusal::BadSignature => f.write_str("the bridge credential's signature is invalid"),
            Refusal::Expired => f.write_str("the bridge credential has expired"),
            Refusal::WrongActor => f.write_str("the bridge credential is for another actor"),
            Refusal::Superseded => {
                f.write_str("the bridge credential's attempt was superseded by a newer one")
            }
            Refusal::Ended => f.write_str("the bridge credential's attempt has ended"),
        }
    }
}

impl std::error::Error for Refusal {}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

impl Claims {
    fn payload(&self) -> Result<String, Refusal> {
        let fields = [
            ("atespace", self.atespace.as_str()),
            ("actor", &self.actor),
            ("uid", &self.uid),
            ("attempt", &self.attempt),
        ];
        let mut text = String::from(HEADER);
        for (key, value) in fields {
            if value.contains(['\n', '\r']) {
                return Err(Refusal::Malformed(format!("{key} contains a line break")));
            }
            text.push_str(&format!("\n{key}={value}"));
        }
        text.push_str(&format!("\nseq={}\nexpires={}", self.seq, self.expires));
        Ok(text)
    }

    fn parse(payload: &str) -> Result<Claims, Refusal> {
        let malformed = |why: &str| Refusal::Malformed(why.to_owned());
        let mut lines = payload.split('\n');
        if lines.next() != Some(HEADER) {
            return Err(malformed("unknown credential version"));
        }
        let mut field = |key: &str| -> Result<String, Refusal> {
            let line = lines.next().ok_or_else(|| malformed("missing a claim"))?;
            line.strip_prefix(key)
                .and_then(|rest| rest.strip_prefix('='))
                .map(str::to_owned)
                .ok_or_else(|| Refusal::Malformed(format!("expected the {key} claim")))
        };
        let claims = Claims {
            atespace: field("atespace")?,
            actor: field("actor")?,
            uid: field("uid")?,
            attempt: field("attempt")?,
            seq: field("seq")?.parse().map_err(|_| malformed("bad seq"))?,
            expires: field("expires")?
                .parse()
                .map_err(|_| malformed("bad expires"))?,
        };
        if lines.next().is_some() {
            return Err(malformed("unexpected trailing claims"));
        }
        Ok(claims)
    }
}

/// The host's signing key.
pub struct Signer {
    pair: Ed25519KeyPair,
}

impl fmt::Debug for Signer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Signer")
            .field("public_key", &self.public_key())
            .finish_non_exhaustive()
    }
}

impl Signer {
    /// A new key, and its PKCS#8 document to store.
    pub fn generate() -> io::Result<(Signer, Vec<u8>)> {
        let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .map_err(|_| io::Error::other("could not generate an Ed25519 key"))?;
        let bytes = document.as_ref().to_vec();
        Ok((Signer::from_pkcs8(&bytes)?, bytes))
    }

    pub fn from_pkcs8(document: &[u8]) -> io::Result<Signer> {
        let pair = Ed25519KeyPair::from_pkcs8(document).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("not an Ed25519 PKCS#8 key: {e}"),
            )
        })?;
        Ok(Signer { pair })
    }

    /// Read a key written by [`Signer::write`] or `branchyard-bridge keygen`.
    pub fn read(path: &Path) -> io::Result<Signer> {
        let document = fs::read(path).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("could not read the bridge key {}: {e}", path.display()),
            )
        })?;
        Signer::from_pkcs8(&document)
            .map_err(|e| io::Error::new(e.kind(), format!("bridge key {}: {e}", path.display())))
    }

    /// Generate a key, write it to `path` readable by its owner only (never
    /// replacing a file), and return it.
    pub fn write(path: &Path) -> io::Result<Signer> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let (signer, document) = Signer::generate()?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(&document)?;
        file.sync_all()?;
        Ok(signer)
    }

    /// The verifying key, as the bridge reads it from [`KEY_ENV`].
    pub fn public_key(&self) -> String {
        hex(self.pair.public_key().as_ref())
    }

    pub fn sign(&self, claims: &Claims) -> Result<String, Refusal> {
        let payload = claims.payload()?;
        let signature = self.pair.sign(payload.as_bytes());
        Ok(format!(
            "{PREFIX}.{}.{}",
            hex(payload.as_bytes()),
            hex(signature.as_ref())
        ))
    }
}

/// The bridge's verifying key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verifier {
    key: Vec<u8>,
}

impl Verifier {
    pub fn from_hex(text: &str) -> io::Result<Verifier> {
        match unhex(text.trim()) {
            Some(key) if key.len() == 32 => Ok(Verifier { key }),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{KEY_ENV} must be 64 hex digits (an Ed25519 public key)"),
            )),
        }
    }

    /// The claims of a correctly signed, unexpired credential. Whether it
    /// is for this actor and a current attempt is [`Attempts::admit`]'s
    /// job.
    pub fn verify(&self, token: &str, now: u64) -> Result<Claims, Refusal> {
        let mut parts = token.trim().split('.');
        let (Some(PREFIX), Some(payload), Some(signature), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(Refusal::Malformed("not a byb1 credential".into()));
        };
        let payload = unhex(payload).ok_or_else(|| Refusal::Malformed("bad payload".into()))?;
        let signature =
            unhex(signature).ok_or_else(|| Refusal::Malformed("bad signature encoding".into()))?;
        UnparsedPublicKey::new(&ED25519, &self.key)
            .verify(&payload, &signature)
            .map_err(|_| Refusal::BadSignature)?;
        let payload = String::from_utf8(payload)
            .map_err(|_| Refusal::Malformed("payload is not UTF-8".into()))?;
        let claims = Claims::parse(&payload)?;
        if claims.expires <= now {
            return Err(Refusal::Expired);
        }
        Ok(claims)
    }
}

/// Who the bridge is: the identity Substrate projects into the actor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub atespace: String,
    pub actor: String,
    pub uid: String,
}

/// The attempts a bridge has seen: the newest, and the sequence number at
/// or below which every attempt has ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attempts {
    pub current: u64,
    pub ended_through: u64,
}

impl Attempts {
    /// Admit `claims` for `identity`. A newer attempt supersedes the
    /// current one; returns the sequence number at or below which attempts
    /// are now dead, if that changed, so their processes can be torn down.
    pub fn admit(&mut self, claims: &Claims, identity: &Identity) -> Result<Option<u64>, Refusal> {
        if claims.atespace != identity.atespace
            || claims.actor != identity.actor
            || claims.uid != identity.uid
        {
            return Err(Refusal::WrongActor);
        }
        if claims.seq <= self.ended_through {
            return Err(Refusal::Ended);
        }
        if claims.seq < self.current {
            return Err(Refusal::Superseded);
        }
        if claims.seq > self.current {
            let dead = claims.seq - 1;
            self.current = claims.seq;
            if dead > self.ended_through {
                self.ended_through = dead;
                return Ok(Some(dead));
            }
        }
        Ok(None)
    }

    /// End the attempt `seq` and every earlier one.
    pub fn end(&mut self, seq: u64) {
        self.ended_through = self.ended_through.max(seq);
        self.current = self.current.max(seq);
    }

    /// `current <n>\nended <n>\n`.
    pub fn encode(&self) -> String {
        format!("current {}\nended {}\n", self.current, self.ended_through)
    }

    pub fn decode(text: &str) -> Option<Attempts> {
        let mut attempts = Attempts::default();
        for line in text.lines() {
            let (key, value) = line.split_once(' ')?;
            let value = value.trim().parse().ok()?;
            match key {
                "current" => attempts.current = value,
                "ended" => attempts.ended_through = value,
                _ => return None,
            }
        }
        Some(attempts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> Identity {
        Identity {
            atespace: "tenant".into(),
            actor: "by-x-1".into(),
            uid: "uid-1".into(),
        }
    }

    fn claims(seq: u64) -> Claims {
        Claims {
            atespace: "tenant".into(),
            actor: "by-x-1".into(),
            uid: "uid-1".into(),
            attempt: format!("x#{seq}"),
            seq,
            expires: 2_000,
        }
    }

    #[test]
    fn a_signed_credential_verifies_and_a_tampered_one_does_not() {
        let (signer, document) = Signer::generate().unwrap();
        let verifier = Verifier::from_hex(&signer.public_key()).unwrap();
        let token = signer.sign(&claims(1)).unwrap();
        assert_eq!(verifier.verify(&token, 1_000).unwrap(), claims(1));
        assert_eq!(
            Signer::from_pkcs8(&document).unwrap().public_key(),
            signer.public_key()
        );

        // Change one hex digit of the payload.
        let mut tampered = token.clone().into_bytes();
        tampered[10] = if tampered[10] == b'0' { b'1' } else { b'0' };
        let tampered = String::from_utf8(tampered).unwrap();
        assert_eq!(
            verifier.verify(&tampered, 1_000),
            Err(Refusal::BadSignature)
        );

        let (other, _) = Signer::generate().unwrap();
        let foreign = other.sign(&claims(1)).unwrap();
        assert_eq!(verifier.verify(&foreign, 1_000), Err(Refusal::BadSignature));
        assert_eq!(verifier.verify(&token, 2_000), Err(Refusal::Expired));
        assert!(matches!(
            verifier.verify("bearer nonsense", 0),
            Err(Refusal::Malformed(_))
        ));
        assert!(Verifier::from_hex("abcd").is_err());
    }

    #[test]
    fn line_breaks_cannot_forge_claims() {
        let (signer, _) = Signer::generate().unwrap();
        let mut forged = claims(1);
        forged.attempt = "x\nuid=uid-2".into();
        assert!(matches!(signer.sign(&forged), Err(Refusal::Malformed(_))));
    }

    #[test]
    fn attempts_are_bound_to_the_actor_and_never_revived() {
        let mut attempts = Attempts::default();
        assert_eq!(attempts.admit(&claims(5), &identity()), Ok(Some(4)));
        assert_eq!(attempts.admit(&claims(5), &identity()), Ok(None));

        let mut elsewhere = claims(6);
        elsewhere.uid = "uid-2".into();
        assert_eq!(
            attempts.admit(&elsewhere, &identity()),
            Err(Refusal::WrongActor)
        );

        // A newer attempt supersedes the current one.
        assert_eq!(attempts.admit(&claims(7), &identity()), Ok(Some(6)));
        assert_eq!(attempts.admit(&claims(5), &identity()), Err(Refusal::Ended));

        attempts.end(7);
        assert_eq!(attempts.admit(&claims(7), &identity()), Err(Refusal::Ended));
        assert_eq!(attempts.admit(&claims(8), &identity()), Ok(None));

        let restored = Attempts::decode(&attempts.encode()).unwrap();
        assert_eq!(restored, attempts);
        assert_eq!(Attempts::decode("current x\n"), None);
    }
}
