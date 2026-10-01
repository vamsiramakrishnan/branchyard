//! Bearer-token authentication. A presented token is hashed and every
//! configured credential's hash is compared on every request in time
//! independent of where they differ; the verifier holds only hashes, never
//! a plaintext token, and nothing here logs or echoes one.

use crate::config::{sha256_hex, Credential, Principal};

pub struct Credentials {
    credentials: Vec<Credential>,
}

impl Credentials {
    pub fn new(credentials: Vec<Credential>) -> Credentials {
        Credentials { credentials }
    }

    /// The principal of the configured credential whose token hashes to
    /// `token_sha256`: for a push subscription bound to that credential.
    pub fn principal_for_hash(&self, token_sha256: &str) -> Option<&Principal> {
        self.credentials
            .iter()
            .find(|c| constant_time_eq(c.token_sha256.as_bytes(), token_sha256.as_bytes()))
            .map(|c| &c.principal)
    }

    /// The principal an `Authorization` header's bearer token verifies as.
    pub fn verify(&self, header: Option<&str>) -> Option<&Principal> {
        let presented = header?.strip_prefix("Bearer ")?.trim();
        if presented.is_empty() {
            return None;
        }
        let digest = sha256_hex(presented.as_bytes());
        let mut found = None;
        // No early exit: the comparison takes as long whichever credential
        // (if any) matches.
        for credential in &self.credentials {
            if constant_time_eq(digest.as_bytes(), credential.token_sha256.as_bytes()) {
                found = Some(&credential.principal);
            }
        }
        found
    }
}

/// Equality whose timing depends on the lengths only.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u64;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= u64::from(x ^ y);
    }
    std::hint::black_box(diff) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Principal;

    fn credential(name: &str, secret: &str) -> Credential {
        Credential {
            token_sha256: sha256_hex(secret.as_bytes()),
            principal: Principal::default_for(name),
        }
    }

    #[test]
    fn verifies_by_principal_and_refuses_everything_else() {
        let credentials = Credentials::new(vec![
            credential("a", "0123456789abcdef"),
            credential("b", "fedcba9876543210"),
        ]);
        assert_eq!(
            credentials
                .verify(Some("Bearer fedcba9876543210"))
                .map(|p| p.name.as_str()),
            Some("b")
        );
        for bad in [
            None,
            Some(""),
            Some("Bearer"),
            Some("Bearer "),
            Some("Basic 0123456789abcdef"),
            Some("Bearer 0123456789abcdeF"),
            Some("Bearer 0123456789abcdef0"),
            Some("Bearer 0123456789abcde"),
        ] {
            assert!(credentials.verify(bad).is_none(), "{bad:?}");
        }
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"a", b""));
    }

    #[test]
    fn the_verifier_never_holds_a_plaintext_token() {
        let credentials = Credentials::new(vec![credential("a", "0123456789abcdef")]);
        assert!(!format!("{:?}", credentials.credentials[0]).contains("0123456789abcdef"));
    }
}
