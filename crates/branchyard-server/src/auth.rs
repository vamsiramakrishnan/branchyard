//! Bearer-token authentication. Every configured token is compared on
//! every request in time independent of where they differ, and a token is
//! never logged or echoed.

use crate::config::Token;

pub struct Tokens {
    tokens: Vec<Token>,
}

impl Tokens {
    pub fn new(tokens: Vec<Token>) -> Tokens {
        Tokens { tokens }
    }

    /// The name of the token an `Authorization` header presents.
    pub fn verify(&self, header: Option<&str>) -> Option<&str> {
        let presented = header?.strip_prefix("Bearer ")?.trim().as_bytes();
        let mut found = None;
        // No early exit: the comparison takes as long whichever token (if
        // any) matches.
        for token in &self.tokens {
            if constant_time_eq(presented, token.secret.as_bytes()) {
                found = Some(token.name.as_str());
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

    #[test]
    fn verifies_by_name_and_refuses_everything_else() {
        let tokens = Tokens::new(vec![
            Token {
                name: "a".into(),
                secret: "0123456789abcdef".into(),
            },
            Token {
                name: "b".into(),
                secret: "fedcba9876543210".into(),
            },
        ]);
        assert_eq!(tokens.verify(Some("Bearer fedcba9876543210")), Some("b"));
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
            assert_eq!(tokens.verify(bad), None, "{bad:?}");
        }
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"a", b""));
    }
}
