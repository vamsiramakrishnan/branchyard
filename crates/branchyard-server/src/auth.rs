//! Bearer-token authentication. A presented token is hashed and every
//! configured credential's hash is compared on every request in time
//! independent of where they differ; the verifier holds only hashes, never
//! a plaintext token, and nothing here logs or echoes one.

use crate::config::{sha256_hex, AuthenticationOrigin, Credential, GoogleIdTokenConfig, Principal};
use google_cloud_auth::credentials::idtoken::verifier::{Builder, Verifier};
use std::time::Duration;

mod policy;
pub(crate) use policy::{origin_allowed, paired_allowed, recheck_caller};

pub struct Credentials {
    credentials: Vec<Credential>,
    google: Option<GoogleVerifier>,
}

impl Credentials {
    pub fn new(credentials: Vec<Credential>) -> Credentials {
        Credentials {
            credentials,
            google: None,
        }
    }

    /// Add Google-only ID-token verification without changing static credentials.
    /// Select ring only when no process TLS provider has been installed.
    pub fn with_google(mut self, config: Option<GoogleIdTokenConfig>) -> Self {
        self.google = config.map(|config| {
            if rustls::crypto::CryptoProvider::get_default().is_none() {
                branchyard_support::best_effort(
                    "initialize Google ID-token TLS",
                    rustls::crypto::ring::default_provider()
                        .install_default()
                        .map_err(|_| "TLS provider was initialized concurrently"),
                );
            }
            let verifier = Builder::new([&config.audience])
                .with_clock_skew(Duration::from_secs(10))
                .build();
            GoogleVerifier { config, verifier }
        });
        self
    }

    #[cfg(test)]
    pub(crate) fn with_google_jwks(mut self, url: Option<&str>) -> Self {
        if let (Some(google), Some(url)) = (&mut self.google, url) {
            google.verifier = Builder::new([&google.config.audience])
                .with_jwks_url(url)
                .build();
        }
        self
    }

    /// Verify a bounded Google ID token and apply its exact configured binding.
    /// The official library owns signatures and JWKS; rejected credentials are not logged.
    pub async fn verify_google(&self, header: Option<&str>) -> Option<Principal> {
        let google = self.google.as_ref()?;
        let token = header?.strip_prefix("Bearer ")?.trim();
        if token.is_empty() || token.len() > 16 * 1024 {
            return None;
        }
        let claims = tokio::time::timeout(Duration::from_secs(10), google.verifier.verify(token))
            .await
            .ok()?
            .ok()?;
        let now = branchyard_support::time::now_ms() / 1000;
        let issued = claims.get("iat")?.as_u64()?;
        let expires = claims.get("exp")?.as_u64()?;
        if issued > now.saturating_add(10)
            || expires <= now
            || expires < issued
            || claims.get("nbf").is_some_and(|value| {
                value
                    .as_u64()
                    .is_none_or(|time| time > now.saturating_add(10))
            })
        {
            return None;
        }
        let issuer = claims.get("iss")?.as_str()?;
        let subject = claims.get("sub")?.as_str()?;
        let binding = google
            .config
            .bindings
            .iter()
            .find(|binding| binding.issuer == issuer && binding.subject == subject)?;
        let mut principal = binding.principal.clone();
        principal.origin = AuthenticationOrigin::GoogleIdToken {
            issuer: issuer.into(),
            subject: subject.into(),
            audience: google.config.audience.clone(),
        };
        Some(principal)
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

struct GoogleVerifier {
    config: GoogleIdTokenConfig,
    verifier: Verifier,
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
    fn identity_config() -> GoogleIdTokenConfig {
        let mut policy = Principal::default_for("same-display-name");
        policy.tenant = "tenant-a".into();
        policy.scopes = ["read".into()].into_iter().collect();
        GoogleIdTokenConfig {
            audience: "https://branchyard.example".into(),
            bindings: vec![crate::config::GoogleIdentityBinding {
                issuer: "https://accounts.google.com".into(),
                subject: "fake-external-subject".into(),
                principal: policy,
            }],
        }
    }

    fn signed(claims: serde_json::Value) -> String {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = Some("fake-google-key".into());
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(include_bytes!(
            "../../branchyard-sync/src/auth/testdata/rsa-test-key.pem"
        ))
        .unwrap();
        jsonwebtoken::encode(&header, &claims, &key).unwrap()
    }

    fn claims() -> serde_json::Value {
        let now = branchyard_support::time::now_ms() / 1000;
        serde_json::json!({"iss":"https://accounts.google.com", "sub":"fake-external-subject", "aud":"https://branchyard.example", "iat":now, "exp":now+3600, "scope":"admin", "email":"untrusted@example.com"})
    }

    #[test]
    fn google_verification_works_before_server_startup() {
        let scratch = branchyard_testkit::Scratch::new("google-verifier-tls");
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        let output = branchyard_testkit::hermetic(&mut command)
            .current_dir(scratch.path())
            .args([
                "--exact",
                "auth::tests::google_verification_preserves_origin_and_rechecks_exact_binding",
                "--nocapture",
            ])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed; 0 failed"),
            "fresh-process Google verification failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn google_verification_preserves_origin_and_rechecks_exact_binding() {
        let jwks: serde_json::Value =
            serde_json::from_str(include_str!("testdata/google-jwks.json")).unwrap();
        let server = branchyard_testkit::MockHttp::start(move |_| {
            branchyard_testkit::Response::json(200, &jwks)
        });
        let config = identity_config();
        let credentials = Credentials::new(Vec::new())
            .with_google(Some(config.clone()))
            .with_google_jwks(Some(&server.url()));
        let token = signed(claims());
        let admitted = credentials
            .verify_google(Some(&format!("Bearer {token}")))
            .await
            .unwrap();
        assert!(admitted.allows("read"));
        assert!(
            !admitted.allows("admin"),
            "untrusted token scopes must not grant policy"
        );
        assert!(config.still_allows(&admitted));
        assert!(matches!(
            admitted.origin,
            AuthenticationOrigin::GoogleIdToken { .. }
        ));
        assert_ne!(
            admitted.connector_subject(),
            config.bindings[0].principal.connector_subject()
        );
        let serialized = serde_json::to_string(&admitted).unwrap();
        assert!(!serialized.contains(&token));
        let restored: Principal = serde_json::from_str(&serialized).unwrap();
        assert_eq!(restored, admitted);
        let caller = crate::api::Caller::new(restored);
        assert_ne!(
            caller.idempotency_scope(),
            crate::api::Caller::new(config.bindings[0].principal.clone()).idempotency_scope()
        );
        let mut changed = config.clone();
        changed.bindings[0].principal.scopes.insert("admin".into());
        assert!(!changed.still_allows(&admitted));
        changed = config.clone();
        changed.bindings[0].subject = "another-subject".into();
        assert!(!changed.still_allows(&admitted));
        assert!(credentials
            .verify_google(Some("Bearer opaque-access-token"))
            .await
            .is_none());
        for (field, value) in [
            ("iss", serde_json::json!("https://attacker.example")),
            ("aud", serde_json::json!("https://other.example")),
            ("sub", serde_json::json!("not-bound")),
            ("exp", serde_json::json!(1)),
            (
                "iat",
                serde_json::json!(branchyard_support::time::now_ms() / 1000 + 3600),
            ),
            (
                "nbf",
                serde_json::json!(branchyard_support::time::now_ms() / 1000 + 3600),
            ),
        ] {
            let mut invalid = claims();
            invalid[field] = value;
            assert!(
                credentials
                    .verify_google(Some(&format!("Bearer {}", signed(invalid))))
                    .await
                    .is_none(),
                "{field}"
            );
        }
        let mut altered = token.into_bytes();
        let last = altered.len() - 12;
        altered[last] = if altered[last] == b'a' { b'b' } else { b'a' };
        assert!(credentials
            .verify_google(Some(&format!(
                "Bearer {}",
                String::from_utf8(altered).unwrap()
            )))
            .await
            .is_none());
    }

    #[test]
    fn google_binding_config_is_strict_and_legacy_principals_stay_static() {
        let config = identity_config();
        config.validate().unwrap();
        let mut duplicate = config.clone();
        duplicate.bindings.push(duplicate.bindings[0].clone());
        assert!(duplicate.validate().is_err());
        let mut forged = config.clone();
        forged.bindings[0].principal.origin = AuthenticationOrigin::GoogleIdToken {
            issuer: "https://accounts.google.com".into(),
            subject: "forged".into(),
            audience: config.audience,
        };
        assert!(forged.validate().is_err());
        let legacy: Principal = serde_json::from_value(
            serde_json::json!({"name":"legacy", "tenant":"local", "scopes":[]}),
        )
        .unwrap();
        assert_eq!(legacy.origin, AuthenticationOrigin::Configured);
        assert_eq!(legacy.connector_subject(), "legacy");
        let mut colliding = legacy.clone();
        colliding.name = "google:reserved".into();
        assert!(crate::config::check_principal(&colliding).is_err());
        let mut another = legacy.clone();
        another.origin = AuthenticationOrigin::GoogleIdToken {
            issuer: "accounts.google.com".into(),
            subject: "one".into(),
            audience: "server".into(),
        };
        let mut other_issuer = another.clone();
        other_issuer.origin = AuthenticationOrigin::GoogleIdToken {
            issuer: "https://accounts.google.com".into(),
            subject: "one".into(),
            audience: "server".into(),
        };
        assert_ne!(
            another.connector_subject(),
            other_issuer.connector_subject()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn google_only_admission_uses_the_same_policy_for_api_and_metrics() {
        use crate::config::{Config, MetricsConfig};
        let scratch = branchyard_testkit::Scratch::new("google-server");
        let root = scratch.join("repo");
        std::fs::create_dir(&root).unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@localhost",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "initial",
            ],
        ] {
            assert!(std::process::Command::new("git")
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .current_dir(&root)
                .status()
                .unwrap()
                .success());
        }
        let jwks: serde_json::Value =
            serde_json::from_str(include_str!("testdata/google-jwks.json")).unwrap();
        let key_server = branchyard_testkit::MockHttp::start(move |_| {
            branchyard_testkit::Response::json(200, &jwks)
        });
        let mut config = Config::new(scratch.join("data"));
        config.listen = "127.0.0.1:0".parse().unwrap();
        config.repos = vec![("app".into(), root)];
        config.inventory = false;
        let mut identities = identity_config();
        let mut admin = identities.bindings[0].clone();
        admin.subject = "fake-admin".into();
        admin.principal.scopes = ["admin".into()].into_iter().collect();
        identities.bindings.push(admin);
        config.google_identity = Some(identities);
        config.google_jwks_url = Some(key_server.url());
        config.metrics = Some(MetricsConfig {
            listen: None,
            token_sha256: None,
        });
        let server = crate::start(config).await.unwrap();
        let client = reqwest::Client::new();
        let token = signed(claims());
        for (path, status) in [("/v1/repos", 200), ("/metrics", 403), ("/v1/app/me", 403)] {
            let response = client
                .get(format!("{}{path}", server.url()))
                .bearer_auth(&token)
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status().as_u16(),
                status,
                "{path}: {}",
                response.text().await.unwrap()
            );
        }
        assert_eq!(
            client
                .get(format!("{}/v1/repos", server.url()))
                .bearer_auth("not-a-google-token")
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            401
        );
        let mut admin_claims = claims();
        admin_claims["sub"] = serde_json::json!("fake-admin");
        assert_eq!(
            client
                .get(format!("{}/metrics", server.url()))
                .bearer_auth(signed(admin_claims))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            200
        );
        server.shutdown();
        server.wait().await;
    }
}
