use super::*;
use crate::companion::store::{CompanionStore, Pairing};
use crate::config::{sha256_hex, Credential, GoogleIdTokenConfig, GoogleIdentityBinding, Token};
use std::sync::Arc;

fn policy(name: &str) -> Principal {
    let mut principal = Principal::default_for(name);
    principal.scopes = ["read".into(), "run".into()].into_iter().collect();
    principal.repos = Some(["app".into(), "other".into()].into_iter().collect());
    principal
}

fn configured() -> (Config, Principal) {
    let principal = policy("issuer");
    let mut config = Config::new(Default::default());
    config.tokens.push(Token {
        name: "issuer-token".into(),
        secret: "hermetic-issuer-token".into(),
    });
    config
        .principals
        .insert("issuer-token".into(), principal.clone());
    (config, principal)
}

fn google() -> (Config, Principal) {
    let mut principal = policy("operator-display");
    let mut config = Config::new(Default::default());
    config.google_identity = Some(GoogleIdTokenConfig {
        audience: "https://branchyard.test".into(),
        bindings: vec![GoogleIdentityBinding {
            issuer: "https://accounts.google.com".into(),
            subject: "hermetic-external-subject".into(),
            principal: principal.clone(),
        }],
    });
    principal.origin = AuthenticationOrigin::GoogleIdToken {
        issuer: "https://accounts.google.com".into(),
        subject: "hermetic-external-subject".into(),
        audience: "https://branchyard.test".into(),
    };
    (config, principal)
}

fn nested(issuer: &Principal) -> Principal {
    let mut phone = issuer.delegated_to("phone".into());
    phone.scopes = ["read".into()].into_iter().collect();
    phone.repos = Some(["app".into()].into_iter().collect());
    phone.delegated_to("tablet".into())
}

fn token(principal: Principal) -> PairedToken {
    PairedToken {
        token_sha256: sha256_hex(b"hermetic-leaf-token"),
        principal,
        device: Some("Hermetic browser".into()),
        created_at_ms: 0,
        expires_at_ms: 100,
        revoked_at_ms: None,
    }
}

#[test]
fn paired_google_policy_tracks_root_without_replacing_leaf_grants() {
    let (mut config, issuer) = google();
    let leaf = nested(&issuer);
    let before = leaf.clone();
    assert_eq!(leaf.connector_subject(), issuer.connector_subject());
    assert!(origin_allowed(&config, &leaf));
    let binding = &mut config.google_identity.as_mut().unwrap().bindings[0];
    binding.principal.name = "renamed-operator-display".into();
    binding.principal.scopes = leaf.scopes.clone();
    binding.principal.repos = leaf.repos.clone();
    assert!(
        origin_allowed(&config, &leaf),
        "device names are not issuer names"
    );
    assert_eq!(leaf, before, "rechecking must not widen the device policy");

    let allowed = config.clone();
    config.google_identity.as_mut().unwrap().bindings[0]
        .principal
        .scopes
        .clear();
    assert!(!origin_allowed(&config, &leaf));
    config = allowed.clone();
    config.google_identity.as_mut().unwrap().bindings[0]
        .principal
        .repos = Some(["other".into()].into_iter().collect());
    assert!(!origin_allowed(&config, &leaf));
    config = allowed.clone();
    config.google_identity.as_mut().unwrap().bindings[0]
        .principal
        .tenant = "elsewhere".into();
    assert!(!origin_allowed(&config, &leaf));
    config = allowed.clone();
    config.google_identity.as_mut().unwrap().audience = "https://other.test".into();
    assert!(!origin_allowed(&config, &leaf));
    config = allowed.clone();
    config.google_identity.as_mut().unwrap().bindings.clear();
    assert!(!origin_allowed(&config, &leaf));
    config = allowed;
    config.google_identity = None;
    assert!(!origin_allowed(&config, &leaf));
}

#[test]
fn configured_and_nested_pairing_obey_the_current_root_ceiling() {
    let (mut config, issuer) = configured();
    let leaf = nested(&issuer);
    assert!(origin_allowed(&config, &leaf));
    config
        .principals
        .get_mut("issuer-token")
        .unwrap()
        .scopes
        .remove("read");
    assert!(!origin_allowed(&config, &leaf));
    config
        .principals
        .insert("issuer-token".into(), issuer.clone());
    config.principals.get_mut("issuer-token").unwrap().repos =
        Some(["other".into()].into_iter().collect());
    assert!(!origin_allowed(&config, &leaf));
    config
        .principals
        .insert("issuer-token".into(), issuer.clone());
    let mut broad = leaf.clone();
    broad.repos = None;
    assert!(!origin_allowed(&config, &broad));
    config.tokens.clear();
    assert!(
        !origin_allowed(&config, &leaf),
        "orphan principal entries are not credentials"
    );
    config.credentials.push(Credential {
        token_sha256: sha256_hex(b"hermetic-configured"),
        principal: issuer,
    });
    assert!(
        origin_allowed(&config, &leaf),
        "hashed configured credentials are issuers too"
    );
    config.credentials.clear();
    assert!(!origin_allowed(&config, &leaf));
}

#[test]
fn delegation_rejects_changed_subjects_and_invalid_leaf_policy() {
    let (config, issuer) = google();
    let mut leaf = nested(&issuer);
    if let AuthenticationOrigin::Paired { subject, .. } = &mut leaf.origin {
        *subject = "google:forged".into();
    }
    assert!(leaf.root_origin().is_none());
    assert!(!origin_allowed(&config, &leaf));
    let mut leaf = issuer.delegated_to("phone".into());
    if let AuthenticationOrigin::Paired { subject, .. } = &mut leaf.origin {
        *subject = "google:forged".into();
    }
    assert!(
        leaf.root_origin().is_none(),
        "one hop must match the real Google root too"
    );
    assert!(!config.google_identity.as_ref().unwrap().still_allows(&leaf));
    let mut leaf = nested(&issuer);
    leaf.scopes.insert("unknown-scope".into());
    assert!(!origin_allowed(&config, &leaf));
}

#[test]
fn legacy_operator_pairings_keep_their_existing_lifetime_contract() {
    let config = Config::new(Default::default());
    let mut legacy = token(Principal::default_for("local-operator-link"));
    assert!(paired_allowed(&config, &legacy, 99));
    assert!(!paired_allowed(&config, &legacy, 100));
    legacy.revoked_at_ms = Some(1);
    assert!(!paired_allowed(&config, &legacy, 2));
    legacy.revoked_at_ms = None;
    legacy.principal.scopes.insert("unknown-scope".into());
    assert!(!paired_allowed(&config, &legacy, 2));
}

fn app(mut config: Config) -> (tempfile::TempDir, Arc<App>) {
    let directory = tempfile::tempdir().unwrap();
    config.data_dir = directory.path().into();
    config.app.enabled = true;
    config.app.push = false;
    let app = Arc::new(App {
        preview: Arc::new(
            crate::preview::PreviewService::new(directory.path().join("previews")).unwrap(),
        ),
        repos: Default::default(),
        registry: crate::ops::Registry::open(
            Box::new(crate::store::MemoryStore::default()),
            crate::ops::Options::new(Vec::new()),
        )
        .unwrap(),
        credentials: crate::auth::Credentials::new(config.all_credentials()),
        shutdown: tokio::sync::watch::channel(false).1,
        storage_idem: Default::default(),
        triggers: Arc::new(crate::triggers::dispatch::Hub::new(
            Arc::new(crate::triggers::store::SqliteTriggers::memory()),
            config.triggers.clone(),
            "http://127.0.0.1".into(),
        )),
        companion: Some(Arc::new(
            crate::companion::Companion::open(&config).unwrap(),
        )),
        sync: None,
        config,
    });
    (directory, app)
}

fn issue(
    store: &dyn CompanionStore,
    principal: &Principal,
    suffix: &str,
    now: u64,
) -> (String, Verified) {
    let code = sha256_hex(format!("hermetic-code-{suffix}").as_bytes());
    let bearer = format!("hermetic-bearer-{suffix}");
    let digest = sha256_hex(bearer.as_bytes());
    assert!(store
        .create_pairing(
            &Pairing {
                code_sha256: code.clone(),
                principal: principal.clone(),
                token_ttl_ms: 60_000,
                created_at_ms: now,
                expires_at_ms: now + 60_000
            },
            now
        )
        .unwrap());
    let row = store.redeem(&code, &digest, None, now).unwrap().unwrap();
    (
        bearer,
        Verified {
            token_sha256: digest,
            expires_at_ms: Some(row.expires_at_ms),
        },
    )
}

#[tokio::test]
async fn exact_captured_leaf_revocation_cannot_borrow_another_tokens_identity() {
    let (config, issuer) = configured();
    let (_directory, app) = app(config);
    let leaf = nested(&issuer);
    let caller = Caller::new(leaf.clone());
    let store = &app.companion.as_ref().unwrap().store;
    let now = branchyard_support::time::now_ms();
    let (old_bearer, old) = issue(store.as_ref(), &leaf, "old", now);
    assert!(crate::auth::recheck_caller(&app, &caller, Some(&old)).is_ok());
    assert!(
        crate::companion::verify_paired(&app, Some(&format!("Bearer {old_bearer}")))
            .await
            .is_some()
    );
    assert_eq!(
        store.revoke(&leaf.name, Some(&leaf.tenant), now).unwrap(),
        1
    );
    let (new_bearer, new) = issue(store.as_ref(), &leaf, "new", now);
    assert!(crate::auth::recheck_caller(&app, &caller, Some(&new)).is_ok());
    assert!(crate::auth::recheck_caller(&app, &caller, Some(&old)).is_err());
    assert!(
        crate::companion::verify_paired(&app, Some(&format!("Bearer {old_bearer}")))
            .await
            .is_none()
    );
    assert!(
        crate::companion::verify_paired(&app, Some(&format!("Bearer {new_bearer}")))
            .await
            .is_some()
    );
    assert!(crate::companion::push::owner(&app, &old.token_sha256)
        .await
        .is_none());
    assert_eq!(
        crate::companion::push::owner(&app, &new.token_sha256).await,
        Some(leaf)
    );
}

#[tokio::test]
async fn paired_requests_callbacks_and_push_recheck_current_google_binding() {
    let (config, issuer) = google();
    let (_directory, mut app) = app(config);
    let leaf = nested(&issuer);
    let now = branchyard_support::time::now_ms();
    let (bearer, verified) = issue(
        app.companion.as_ref().unwrap().store.as_ref(),
        &leaf,
        "google",
        now,
    );
    let caller = Caller::new(leaf.clone());
    assert!(crate::auth::recheck_caller(&app, &caller, Some(&verified)).is_ok());
    assert!(
        crate::companion::verify_paired(&app, Some(&format!("Bearer {bearer}")))
            .await
            .is_some()
    );
    Arc::get_mut(&mut app)
        .unwrap()
        .config
        .google_identity
        .as_mut()
        .unwrap()
        .bindings[0]
        .principal
        .repos = Some(["other".into()].into_iter().collect());
    assert!(crate::auth::recheck_caller(&app, &caller, Some(&verified)).is_err());
    assert!(
        crate::companion::verify_paired(&app, Some(&format!("Bearer {bearer}")))
            .await
            .is_none()
    );
    assert!(crate::companion::push::owner(&app, &verified.token_sha256)
        .await
        .is_none());
    Arc::get_mut(&mut app).unwrap().config.google_identity = None;
    assert!(crate::auth::recheck_caller(&app, &caller, Some(&verified)).is_err());
    assert!(
        crate::companion::verify_paired(&app, Some(&format!("Bearer {bearer}")))
            .await
            .is_none()
    );
}

#[test]
fn captured_credentials_cannot_change_expiry_identity_or_grants() {
    let (config, issuer) = configured();
    let row = token(nested(&issuer));
    let verified = Verified {
        token_sha256: row.token_sha256.clone(),
        expires_at_ms: Some(row.expires_at_ms),
    };
    assert!(captured_allowed(
        &config,
        &row.principal,
        Some(&verified),
        Some(&row),
        None,
        1
    ));
    let mut narrow = row.principal.clone();
    narrow.scopes.clear();
    assert!(captured_allowed(
        &config,
        &narrow,
        Some(&verified),
        Some(&row),
        None,
        1
    ));
    narrow.scopes.insert("run".into());
    assert!(!captured_allowed(
        &config,
        &narrow,
        Some(&verified),
        Some(&row),
        None,
        1
    ));
    let mut forged = row.principal.clone();
    forged.name = "different-device".into();
    assert!(!captured_allowed(
        &config,
        &forged,
        Some(&verified),
        Some(&row),
        None,
        1
    ));
    let wrong = Verified {
        token_sha256: "another-hash".into(),
        expires_at_ms: verified.expires_at_ms,
    };
    assert!(!captured_allowed(
        &config,
        &row.principal,
        Some(&wrong),
        Some(&row),
        None,
        1
    ));
    let wrong = Verified {
        token_sha256: verified.token_sha256.clone(),
        expires_at_ms: Some(row.expires_at_ms + 1),
    };
    assert!(!captured_allowed(
        &config,
        &row.principal,
        Some(&wrong),
        Some(&row),
        None,
        1
    ));
    assert!(!captured_allowed(
        &config,
        &row.principal,
        Some(&verified),
        Some(&row),
        None,
        row.expires_at_ms
    ));
    assert!(!captured_allowed(
        &config,
        &row.principal,
        Some(&verified),
        None,
        None,
        1
    ));
}

#[test]
fn captured_configured_credentials_and_internal_calls_have_distinct_proof() {
    let (config, issuer) = configured();
    let (_directory, mut app) = app(config);
    let caller = Caller::new(issuer.clone());
    let verified = Verified {
        token_sha256: sha256_hex(b"hermetic-issuer-token"),
        expires_at_ms: None,
    };
    assert!(crate::auth::recheck_caller(&app, &caller, Some(&verified)).is_ok());
    let missing = Verified {
        token_sha256: sha256_hex(b"removed-token"),
        expires_at_ms: None,
    };
    assert!(crate::auth::recheck_caller(&app, &caller, Some(&missing)).is_err());
    assert!(crate::auth::recheck_caller(&app, &caller, None).is_ok());
    let leaf = Caller::new(nested(&issuer));
    assert!(
        crate::auth::recheck_caller(&app, &leaf, None).is_ok(),
        "trusted internal callers require policy, not fabricated leaf proof"
    );
    let paired = Verified {
        token_sha256: sha256_hex(b"missing-leaf"),
        expires_at_ms: Some(100),
    };
    assert!(crate::auth::recheck_caller(&app, &leaf, Some(&paired)).is_err());
    Arc::get_mut(&mut app).unwrap().config.tokens.clear();
    assert!(crate::auth::recheck_caller(&app, &caller, Some(&verified)).is_err());
    assert!(crate::auth::recheck_caller(&app, &leaf, None).is_err());
}

#[test]
fn legacy_captured_leaf_needs_its_exact_valid_token() {
    let config = Config::new(Default::default());
    let row = token(Principal::default_for("operator-link"));
    let verified = Verified {
        token_sha256: row.token_sha256.clone(),
        expires_at_ms: Some(row.expires_at_ms),
    };
    assert!(captured_allowed(
        &config,
        &row.principal,
        Some(&verified),
        Some(&row),
        None,
        1
    ));
    assert!(!captured_allowed(
        &config,
        &row.principal,
        None,
        Some(&row),
        None,
        1
    ));
}
