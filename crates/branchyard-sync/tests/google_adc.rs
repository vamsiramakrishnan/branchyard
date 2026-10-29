//! Official Google providers against fake ADC, STS and metadata endpoints.
#![allow(clippy::unwrap_used, clippy::expect_used)] // tests: a panic is the failure report

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use branchyard_sync::auth::google::GoogleAuth;
use branchyard_testkit::{MockHttp, Response, Scratch};
use serde_json::json;

// One test owns the process environment. Every credential and endpoint is fake.
#[test]
fn adc_federates_refreshes_impersonates_and_stays_inside_its_worker_runtime() {
    let scratch = Scratch::new("google-adc");
    std::env::remove_var("GOOGLE_OAUTH_ACCESS_TOKEN");
    std::env::remove_var("GCE_METADATA_HOST");
    let subject = scratch.join("subject.jwt");
    std::fs::write(&subject, "fake-external-subject").unwrap();
    let rotated_subject = subject.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let server = MockHttp::start(move |request| {
        let form = request.body_text();
        if request.path == "/refresh" {
            assert_eq!(request.json()["grant_type"], "refresh_token");
            return Response::json(
                200,
                &json!({"access_token":"user-access", "expires_in":3600, "token_type":"Bearer"}),
            );
        }
        if request.path == "/denied" {
            return Response::json(
                400,
                &json!({"error":"invalid_grant", "error_description":"secret-refresh-value"}),
            );
        }
        if request.path == "/sts" {
            assert!(form.contains("token-exchange"));
            let number = count.fetch_add(1, Ordering::SeqCst);
            if number == 0 {
                assert!(form.contains("subject_token=fake-external-subject"));
                std::fs::write(&rotated_subject, "fake-rotated-subject").unwrap();
            } else {
                assert!(form.contains("subject_token=fake-rotated-subject"));
            }
            return Response::json(
                200,
                &json!({"access_token":format!("federated-{number}"), "expires_in":if number == 0 {0} else {3600}, "token_type":"Bearer", "issued_token_type":"urn:ietf:params:oauth:token-type:access_token"}),
            );
        }
        if request.path.ends_with(":generateAccessToken") {
            assert_eq!(request.header("authorization"), Some("Bearer user-access"));
            assert!(request.json()["scope"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s == branchyard_sync::auth::google::SCOPE));
            return Response::json(
                200,
                &json!({"accessToken":"impersonated-access", "expireTime": branchyard_support::time::rfc3339(branchyard_support::time::now_ms()+3_600_000)}),
            );
        }
        if request
            .path
            .starts_with("/computeMetadata/v1/instance/service-accounts/default/token")
        {
            assert_eq!(request.header("metadata-flavor"), Some("Google"));
            return Response::json(
                200,
                &json!({"access_token":"metadata-access", "expires_in":3600, "token_type":"Bearer"}),
            );
        }
        Response::json(400, &json!({"error":"unexpected_path"}))
    });
    let user = json!({"type":"authorized_user", "client_id":"fake-client", "client_secret":"fake-secret", "refresh_token":"fake-refresh", "token_uri":format!("{}/refresh",server.url())});
    let adc_file = scratch.join("adc.json");
    std::fs::write(&adc_file, serde_json::to_vec(&user).unwrap()).unwrap();
    std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", &adc_file);
    let auth = GoogleAuth::from_env().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        assert_eq!(
            auth.header(0).unwrap().as_deref(),
            Some("Bearer user-access")
        );
    });
    assert_eq!(
        auth.header(u64::MAX).unwrap().as_deref(),
        Some("Bearer user-access")
    );
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|r| r.path == "/refresh")
            .count(),
        1
    );

    let external = json!({"type":"external_account", "audience":"//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/test/providers/test", "subject_token_type":"urn:ietf:params:oauth:token-type:jwt", "token_url":format!("{}/sts",server.url()), "credential_source":{"file":subject}});
    std::fs::write(&adc_file, serde_json::to_vec(&external).unwrap()).unwrap();
    let federated = GoogleAuth::from_env().unwrap();
    drop(federated.header(0).unwrap());
    branchyard_testkit::wait::until("official provider refresh and subject rotation", || {
        calls.load(Ordering::SeqCst) >= 2
    });
    assert_eq!(
        federated.header(0).unwrap().as_deref(),
        Some("Bearer federated-1")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let impersonated = GoogleAuth::from_json(
        json!({"type":"impersonated_service_account", "service_account_impersonation_url":format!("{}/v1/projects/-/serviceAccounts/fake@example.com:generateAccessToken",server.url()), "source_credentials":user}),
    );
    assert_eq!(
        impersonated.header(0).unwrap().as_deref(),
        Some("Bearer impersonated-access")
    );
    let metadata = GoogleAuth::from_metadata(server.url().trim_start_matches("http://"));
    assert_eq!(
        metadata.header(0).unwrap().as_deref(),
        Some("Bearer metadata-access")
    );

    let mut revoked = user.clone();
    revoked["token_uri"] = json!(format!("{}/denied", server.url()));
    let error = GoogleAuth::from_json(revoked).header(0).unwrap_err();
    assert_eq!(error.kind, branchyard_sync::Kind::Refused);
    assert!(!error.to_string().contains("secret-refresh-value"));

    let service = GoogleAuth::from_json(json!({
        "type":"service_account", "client_email":"fake@example.iam.gserviceaccount.com", "project_id":"fake", "private_key_id":"fake-key",
        "private_key":include_str!("../src/auth/testdata/rsa-test-key.pem")
    }));
    let assertion = service.header(0).unwrap().unwrap();
    assert_eq!(
        assertion
            .strip_prefix("Bearer ")
            .unwrap()
            .split('.')
            .count(),
        3
    );

    let bad = GoogleAuth::from_json(
        json!({"type":"external_account", "private_key":"must-never-appear"}),
    );
    let error = bad.header(0).unwrap_err();
    assert!(!error.to_string().contains("must-never-appear"));
    assert!(!format!("{bad:?}").contains("must-never-appear"));
    assert!(GoogleAuth::anonymous().header(0).unwrap().is_none());
    assert_eq!(
        GoogleAuth::fixed("fake-fixed")
            .header(0)
            .unwrap()
            .as_deref(),
        Some("Bearer fake-fixed")
    );
}
