//! An external manager over real MCP stdio and a checked mock HTTP server.

#![allow(clippy::unwrap_used)] // tests: a panic reports the failed check
mod common;

use branchyard_testkit::{MockHttp, Response, Scratch};
use common::Client;
use serde_json::json;

#[test]
fn manager_discovery_and_calls_use_only_the_separate_http_principal() {
    let scratch = Scratch::new("manager-protocol");
    let token = scratch.join("token");
    std::fs::write(&token, "manager-only-fixture\n").unwrap();
    let server = MockHttp::start(|request| {
        assert_eq!(
            request.header("authorization"),
            Some("Bearer manager-only-fixture")
        );
        match request.path.as_str() {
            "/v1/projects" => Response::json(201, &json!({"id":"project"})),
            "/v1/workspaces/hidden" => Response::json(
                403,
                &json!({"error":{"code":"denied","message":"workspace membership revoked"}}),
            ),
            path if path.starts_with("/v1/workspaces/large/artifacts/table/preview?") => {
                assert!(path.contains("limit=25"), "{path}");
                Response::json(
                    200,
                    &json!({"rows":vec![vec!["cell";100];100],"next_cursor":"next-page","artifact_id":"table"}),
                )
            }
            _ => Response::json(
                404,
                &json!({"error":{"code":"unknown","message":"unexpected route"}}),
            ),
        }
    });
    let mut client = Client::manager(&server.url(), &token);
    let initialized = client.initialize();
    assert_eq!(
        initialized["result"]["serverInfo"]["name"],
        "branchyard-workspace-manager"
    );
    let listed = client.request("tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), branchyard_mcp::manager::tools().len());
    assert!(tools
        .iter()
        .all(|tool| tool["inputSchema"]["additionalProperties"] == false));
    assert!(!tools.iter().any(|tool| tool["name"] == "delegate"));
    let (error,text)=client.call("create_project",json!({"name":"Evidence review","source":{"kind":"knowledge"},"result_mode":"artifacts","idempotency_key":"create-once"}));
    assert!(!error, "{text}");
    assert!(text.contains("project"));
    let (error, text) = client.call("inspect_workspace", json!({"workspace_id":"hidden"}));
    assert!(error);
    assert!(text.contains("workspace membership revoked"));
    let (error, text) = client.call(
        "preview_artifact",
        json!({"workspace_id":"large","artifact_id":"table"}),
    );
    assert!(error);
    assert!(text.len() <= 32 * 1024);
    let bounded: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(bounded["code"], "manager_output_too_large");
    assert_eq!(bounded["metadata"]["next_cursor"], "next-page");
    assert!(!text.contains("cell"));
    let invalid = client.request(
        "tools/call",
        json!({"name":"list_projects","arguments":{"url":"http://another-host"}}),
    );
    assert!(invalid.get("error").is_some());
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].header("idempotency-key"), Some("create-once"));
    assert_eq!(requests[0].json()["source"], json!({"kind":"knowledge"}));
    assert!(client.finish().success());
}

#[test]
fn a_managed_child_cannot_start_the_external_manager_or_load_its_credential() {
    let mut command = std::process::Command::new(common::SERVER);
    branchyard_testkit::hermetic(&mut command)
        .args([
            "manager",
            "--url",
            "http://127.0.0.1:1",
            "--token-file",
            "/no-such-operator-token",
        ])
        .env(branchyard::ENV_BRANCH, "child")
        .stdin(std::process::Stdio::null());
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(
        error.contains("cannot run inside a managed harness turn"),
        "{error}"
    );
    assert!(!error.contains("token file"));
}
