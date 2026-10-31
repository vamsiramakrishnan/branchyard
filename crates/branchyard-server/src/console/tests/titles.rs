//! Source titles over HTTP: RFC 8187 uploads kept byte for byte, each
//! refusal, the upload fingerprint, and the names downloads carry.

use super::super::titles::TITLES;
use super::*;
use sha2::{Digest as _, Sha256};

async fn upload(
    fixture: &Fixture,
    wid: &str,
    key: &str,
    titles: &[(&str, &str)],
    data: &[u8],
) -> (StatusCode, Value) {
    let mut headers = vec![
        ("content-type", "text/plain".to_owned()),
        (
            "x-workspace-revision",
            fixture.detail(wid).await["revision"].to_string(),
        ),
    ];
    headers.extend(
        titles
            .iter()
            .map(|(name, value)| (*name, (*value).to_owned())),
    );
    let (status, body) = fixture
        .bytes(
            Method::POST,
            &format!("/v1/workspaces/{wid}/sources"),
            OWNER,
            Some(key),
            &headers,
            data.to_vec(),
        )
        .await;
    (status, serde_json::from_slice(&body).unwrap())
}

async fn download(fixture: &Fixture, path: &str) -> (String, Vec<u8>) {
    let request = Request::builder()
        .uri(path)
        .header("authorization", format!("Bearer {OWNER}"))
        .body(Body::empty())
        .unwrap();
    let response = crate::api::router(fixture.app.clone())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{path}");
    let disposition = response.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .to_owned();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (disposition, bytes.to_vec())
}

async fn titles(fixture: &Fixture, wid: &str) -> Vec<String> {
    let (status, listed) = fixture
        .json(
            Method::GET,
            &format!("/v1/workspaces/{wid}/sources"),
            OWNER,
            None,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let mut titles: Vec<String> = listed["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|source| source["title"].as_str().unwrap().to_owned())
        .collect();
    titles.sort();
    titles
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unicode_titles_upload_list_and_download_byte_for_byte() {
    let fixture = Fixture::new(true);
    let wid = fixture.workspace().await;
    let dispositions = [
        "attachment; filename=\"_tude de march_ _ T3.xlsx\"; filename*=UTF-8''%C3%89tude%20de%20march%C3%A9%20%E2%80%94%20T3.xlsx",
        "attachment; filename=\"____.docx\"; filename*=UTF-8''%E5%AD%A3%E5%BA%A6%E6%8A%A5%E5%91%8A.docx",
        "attachment; filename=\"_____ _____.pdf\"; filename*=UTF-8''%D8%AA%D9%82%D8%B1%D9%8A%D8%B1%20%D8%A7%D9%84%D8%B1%D8%A8%D8%B9.pdf",
        "attachment; filename=\"Backlog _.csv\"; filename*=UTF-8''Backlog%20%F0%9F%93%8A.csv",
        "attachment; filename=\"Board _draft_ notes.txt\"; filename*=UTF-8''Board%20%22draft%22%20notes.txt",
    ];
    for (i, ((title, escaped), disposition)) in TITLES.into_iter().zip(dispositions).enumerate() {
        let data = format!("original {i}: {title}\n").into_bytes();
        let encoded = format!("UTF-8''{escaped}");
        let (status, source) = upload(
            &fixture,
            &wid,
            &format!("unicode-{i}"),
            &[
                ("x-source-title-encoded", encoded.as_str()),
                ("x-source-title", "ascii fallback.txt"),
            ],
            &data,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{source}");
        assert_eq!(
            source["title"].as_str().unwrap().as_bytes(),
            title.as_bytes()
        );
        let path = format!(
            "/v1/workspaces/{wid}/sources/{}/content",
            source["id"].as_str().unwrap()
        );
        let (header, bytes) = download(&fixture, &path).await;
        assert_eq!(bytes, data, "{title}");
        assert_eq!(header, disposition, "{title}");
    }
    let mut expected: Vec<String> = TITLES
        .iter()
        .map(|(title, _)| (*title).to_owned())
        .collect();
    expected.sort();
    assert_eq!(titles(&fixture, &wid).await, expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_encoded_title_is_refused_by_rule_and_replaces_the_ascii_header() {
    let fixture = Fixture::new(true);
    let wid = fixture.workspace().await;
    let too_long = format!("UTF-8''{}", "a".repeat(513));
    let refusals = [
        ("UTF-8''100%", "malformed escape"),
        ("UTF-8''%z1.txt", "malformed escape"),
        ("UTF-8''report%4", "malformed escape"),
        ("UTF-8''%C3%28.txt", "invalid UTF-8"),
        ("ISO-8859-1''%E9tude.txt", "unsupported charset"),
        (too_long.as_str(), "title length"),
        ("UTF-8''", "title length"),
        ("UTF-8''beep%07.txt", "control character U+0007"),
        ("UTF-8''next%C2%85line.txt", "control character U+0085"),
        (
            "UTF-8''invoice%E2%80%AEfdp.exe",
            "bidirectional control U+202E",
        ),
    ];
    for (i, (encoded, rule)) in refusals.into_iter().enumerate() {
        let (status, body) = upload(
            &fixture,
            &wid,
            &format!("refused-{i}"),
            &[
                ("x-source-title-encoded", encoded),
                ("x-source-title", "Valid fallback.txt"),
            ],
            b"refused",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{encoded}: {body}");
        assert_eq!(body["error"]["code"], "invalid_source_title", "{body}");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(
            message.contains(rule) && message.contains("send UTF-8''<percent-encoded title>"),
            "{encoded}: {message}"
        );
    }
    assert!(
        titles(&fixture, &wid).await.is_empty(),
        "a refused title stores nothing, and never the ASCII fallback"
    );

    for (i, ascii) in ["café.txt", ""].into_iter().enumerate() {
        let (status, source) = upload(
            &fixture,
            &wid,
            &format!("ignored-{i}"),
            &[
                ("x-source-title-encoded", "UTF-8'fr'%C3%89tude.txt"),
                ("x-source-title", ascii),
            ],
            b"kept",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{source}");
        assert_eq!(source["title"], "Étude.txt");
    }
    let (status, body) = upload(
        &fixture,
        &wid,
        "ascii-only-invalid",
        &[("x-source-title", "café.txt")],
        b"refused",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "invalid_request", "{body}");
    let (status, source) = upload(
        &fixture,
        &wid,
        "ascii-only",
        &[("x-source-title", "Plain title.txt")],
        b"kept",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{source}");
    assert_eq!(source["title"], "Plain title.txt");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_upload_fingerprint_is_the_decoded_title() {
    let fixture = Fixture::new(true);
    let wid = fixture.workspace().await;
    let named = |encoded: &'static str, ascii: &'static str| {
        [
            ("x-source-title-encoded", encoded),
            ("x-source-title", ascii),
        ]
    };
    let (status, original) = upload(
        &fixture,
        &wid,
        "same",
        &named("UTF-8''%C3%89tude.txt", "Etude.txt"),
        b"same bytes",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{original}");
    let (status, replayed) = upload(
        &fixture,
        &wid,
        "same",
        &named("utf-8'fr'%c3%89tude.txt", "another fallback.txt"),
        b"same bytes",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replayed}");
    assert_eq!(replayed, original, "the same decoded title replays");
    let (status, conflict) = upload(
        &fixture,
        &wid,
        "same",
        &named("UTF-8''%C3%89tudes.txt", "Etude.txt"),
        b"same bytes",
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["error"]["code"], "idempotency_conflict");
    assert_eq!(titles(&fixture, &wid).await, ["Étude.txt"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_artifact_downloads_under_its_basename() {
    let fixture = Fixture::new(true);
    let wid = fixture.workspace().await;
    let data = "checked evidence";
    let name = "季度/Q3 \"v2\" 📊.txt";
    let manifest = json!({"schema_version":1,"files":[{"path":"evidence.txt","name":name,
        "media_type":"text/plain","sha256":hex::encode(Sha256::digest(data))}]});
    let prompt = format!(
        "SH mkdir -p .cache/outputs && printf %s '{data}' > .cache/outputs/evidence.txt && printf %s '{manifest}' > .cache/outputs/artifacts.json && printf '\\n```json\\n{{\"answer\":\"checked\"}}\\n```\\n'"
    );
    let mut contract = branchyard::ResultContract::deliverable(json!({"type":"object",
        "required":["answer"],"properties":{"answer":{"type":"string"}},"additionalProperties":false}))
    .unwrap();
    contract.capture = Some(branchyard::ResultCapture::new(
        ".cache/outputs/artifacts.json",
    ));
    let (status, started) = fixture
        .json(
            Method::POST,
            &format!("/v1/workspaces/{wid}/sessions"),
            OWNER,
            Some("named-output"),
            json!({"name":"Named output","prompt":prompt,"harness":"gemini-cli",
                "access":"writer","result_contract":contract}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let finished = fixture.finished(started["id"].as_str().unwrap());
    assert_eq!(
        finished.state,
        branchyard_client::api::OperationState::Succeeded,
        "{finished:?}"
    );
    let (status, artifacts) = fixture
        .json(
            Method::GET,
            &format!("/v1/workspaces/{wid}/artifacts"),
            OWNER,
            None,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{artifacts}");
    let artifact = &artifacts["artifacts"][0];
    assert_eq!(artifact["name"], name);
    let path = format!(
        "/v1/workspaces/{wid}/artifacts/{}/content",
        artifact["id"].as_str().unwrap()
    );
    let (header, bytes) = download(&fixture, &path).await;
    assert_eq!(bytes, data.as_bytes());
    assert_eq!(
        header,
        "attachment; filename=\"Q3 _v2_ _.txt\"; filename*=UTF-8''Q3%20%22v2%22%20%F0%9F%93%8A.txt"
    );
}
