//! Repository-free discovery through the ordinary authenticated router.

use super::*;

/// Restrict the existing fixture before any weak service references are created.
fn knowledge_reader() -> Fixture {
    let mut fixture = Fixture::new(false);
    let app = Arc::get_mut(&mut fixture.app).unwrap();
    let mut principal = Principal::default_for("owner");
    principal.scopes = ["read".into()].into_iter().collect();
    principal.repos = Some([super::super::MANAGED_REPO.into()].into_iter().collect());
    app.config.principals.insert("owner".into(), principal);
    app.credentials = Credentials::new(app.config.all_credentials());
    fixture
}

#[tokio::test]
async fn harness_discovery_serves_knowledge_only_readers_without_a_yard() {
    let fixture = knowledge_reader();
    let (status, body) = fixture
        .json(Method::GET, "/v1/harnesses", OWNER, None, Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let profiles = body["harnesses"].as_array().unwrap();
    assert!(profiles.iter().any(|item| item["profile"] == "codex-acp"));
    assert!(fixture.app.repos.is_empty());
    assert!(!fixture.app.config.data_dir.join("knowledge").exists());
    assert!(fixture
        .app
        .registry
        .unfinished("default")
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn harness_discovery_still_intersects_read_principal_and_tenant_grants() {
    for (case, expected) in [
        ("disabled", "knowledge_disabled"),
        ("principal", "repo_not_allowed"),
        ("tenant", "repo_not_allowed"),
        ("scope", "scope_required"),
    ] {
        let mut fixture = knowledge_reader();
        let app = Arc::get_mut(&mut fixture.app).unwrap();
        match case {
            "disabled" => app.config.managed_knowledge.enabled = false,
            "principal" => {
                app.config.principals.get_mut("owner").unwrap().repos = Some(Default::default())
            }
            "tenant" => {
                app.config.tenants.insert(
                    "default".into(),
                    crate::config::TenantPolicy {
                        repos: Some(Default::default()),
                        ..Default::default()
                    },
                );
            }
            "scope" => app
                .config
                .principals
                .get_mut("owner")
                .unwrap()
                .scopes
                .clear(),
            _ => unreachable!(),
        }
        app.credentials = Credentials::new(app.config.all_credentials());
        let (status, body) = fixture
            .json(Method::GET, "/v1/harnesses", OWNER, None, Value::Null)
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{case}: {body}");
        assert_eq!(body["error"]["code"], expected, "{case}: {body}");
        assert!(!fixture.app.config.data_dir.join("knowledge").exists());
    }
}

#[tokio::test]
async fn harness_discovery_uses_the_request_identity_for_operator_commands() {
    let mut fixture = knowledge_reader();
    let app = Arc::get_mut(&mut fixture.app).unwrap();
    let command = vec![branchyard_testkit::fake_agent_here().display().to_string()];
    app.config
        .harness_commands
        .insert("gemini-cli".into(), command.clone());
    app.config
        .harness_commands
        .insert("codex-acp".into(), command);
    app.config
        .harness_commands
        .insert("codex".into(), vec!["/missing-operator-command".into()]);
    let (status, body) = fixture
        .json(Method::GET, "/v1/harnesses", OWNER, None, Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let profiles = body["harnesses"].as_array().unwrap();
    let gemini = profiles
        .iter()
        .find(|p| p["harness"] == "gemini-cli" && p["default"] == true)
        .unwrap();
    assert_eq!(gemini["available"], true, "{gemini}");
    let acp = profiles
        .iter()
        .find(|p| p["profile"] == "codex-acp")
        .unwrap();
    assert_eq!(acp["available"], true, "{acp}");
    let default = profiles
        .iter()
        .find(|p| p["harness"] == "codex" && p["default"] == true)
        .unwrap();
    assert_eq!(default["available"], false, "{default}");
    assert!(!fixture.app.config.allow_client_commands);
    assert!(!fixture.app.config.data_dir.join("knowledge").exists());
}
