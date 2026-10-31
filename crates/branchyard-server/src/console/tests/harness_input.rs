//! What the console adds to a person's text reaches the harness on every
//! turn, and the messages response gives back only what the person typed.

use super::*;
use branchyard::{Activity, Event};
use branchyard_client::api::OperationState;
use sha2::{Digest as _, Sha256};

const HARNESS: &str = "claude-code-acp";
const REQUESTED: &str = "## Requested result";
const MANIFEST: &str = "write artifact manifest `outputs/artifacts.json`";

fn outputs_contract() -> Value {
    json!({"schema":{"type":"object","required":["summary"],"properties":{"summary":{"type":"string"}}},
        "required":true,"purpose":"deliverable","capture":{"manifest":"outputs/artifacts.json"}})
}

/// Has the fake agent write `outputs/summary.txt`, the manifest that lists
/// it, and the fenced summary the contract asks for.
fn collects(summary: &str) -> String {
    let digest = hex::encode(Sha256::digest(summary.as_bytes()));
    format!(
        "SH mkdir -p outputs && printf %s '{summary}' > outputs/summary.txt && \
         printf %s '{{\"schema_version\":1,\"files\":[{{\"path\":\"summary.txt\",\"media_type\":\"text/plain\",\"sha256\":\"{digest}\"}}]}}' > outputs/artifacts.json && \
         printf '\\n```json\\n{{\"summary\":\"{summary}\"}}\\n```\\n'"
    )
}

async fn start(fixture: &Fixture, wid: &str, key: &str, body: Value) -> String {
    let (status, started) = fixture
        .json(
            Method::POST,
            &format!("/v1/workspaces/{wid}/sessions"),
            OWNER,
            Some(key),
            body,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let done = fixture.finished(started["id"].as_str().unwrap());
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    started["session_id"].as_str().unwrap().into()
}

async fn follow_up(
    fixture: &Fixture,
    wid: &str,
    sid: &str,
    key: &str,
    body: Value,
) -> branchyard_client::api::Operation {
    let (status, admitted) = fixture
        .json(
            Method::POST,
            &format!("/v1/workspaces/{wid}/sessions/{sid}/messages"),
            OWNER,
            Some(key),
            body,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{admitted}");
    fixture.finished(admitted["id"].as_str().unwrap())
}

fn branch(fixture: &Fixture, wid: &str, sid: &str) -> branchyard::Branch {
    let workspace = super::super::get(&fixture.app, Collection::Workspaces, wid).unwrap();
    let repo = super::super::runtime::resolve(&fixture.app, &workspace).unwrap();
    repo.yard.branch(sid).unwrap()
}

/// Each native turn's recorded prompt and the harness's reply to it.
fn turns(fixture: &Fixture, wid: &str, sid: &str) -> Vec<(String, String)> {
    let mut turns: Vec<(String, String)> = Vec::new();
    for event in branch(fixture, wid, sid).events().unwrap() {
        match event.activity {
            Activity::Prompt(text) => turns.push((text, String::new())),
            Activity::Harness(Event::MessageDelta { text, .. }) => {
                if let Some((_, reply)) = turns.last_mut() {
                    reply.push_str(&text);
                }
            }
            _ => {}
        }
    }
    turns
}

async fn typed(fixture: &Fixture, wid: &str, sid: &str) -> Vec<Value> {
    let (status, messages) = fixture
        .json(
            Method::GET,
            &format!("/v1/workspaces/{wid}/sessions/{sid}/messages"),
            OWNER,
            None,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{messages}");
    messages["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .map(|message| message["text"].clone())
        .collect()
}

async fn view(fixture: &Fixture, wid: &str, sid: &str) -> Value {
    fixture.detail(wid).await["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == sid)
        .unwrap()
        .clone()
}

async fn outputs(fixture: &Fixture, wid: &str, sid: &str) -> Vec<(String, u64)> {
    let (status, listed) = fixture
        .json(
            Method::GET,
            &format!("/v1/workspaces/{wid}/artifacts"),
            OWNER,
            None,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let mut outputs: Vec<(String, u64)> = listed["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|artifact| artifact["publisher_branch"] == sid)
        .map(|artifact| {
            (
                artifact["name"].as_str().unwrap().into(),
                artifact["turn"].as_u64().unwrap(),
            )
        })
        .collect();
    outputs.sort();
    outputs
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follow_ups_and_resumes_of_a_collect_outputs_session_repeat_the_requested_result() {
    let fixture = Fixture::new(true);
    let wid = fixture.workspace().await;
    let sid = start(
        &fixture,
        &wid,
        "collector",
        json!({"name":"Collector","prompt":collects("first draft"),"harness":HARNESS,
            "access":"writer","result_contract":outputs_contract()}),
    )
    .await;

    let second = follow_up(
        &fixture,
        &wid,
        &sid,
        "collector-second",
        json!({"prompt":collects("second draft")}),
    )
    .await;
    assert_eq!(second.state, OperationState::Succeeded, "{second:?}");
    let session = view(&fixture, &wid, &sid).await;
    assert_eq!(session["turns"], 2, "{session}");
    assert_eq!(
        session["result"],
        json!({"summary":"second draft"}),
        "{session}"
    );
    assert_eq!(
        outputs(&fixture, &wid, &sid).await,
        [("summary.txt".to_owned(), 1), ("summary.txt".to_owned(), 2)]
    );

    let echoed = follow_up(
        &fixture,
        &wid,
        &sid,
        "collector-echo",
        json!({"prompt":"Tighten the summary"}),
    )
    .await;
    assert!(echoed.state.is_terminal(), "{echoed:?}");

    let resumed = follow_up(
        &fixture,
        &wid,
        &sid,
        "collector-resume",
        json!({"prompt":collects("resumed draft"),"action":"resume"}),
    )
    .await;
    assert_eq!(resumed.state, OperationState::Succeeded, "{resumed:?}");
    let session = view(&fixture, &wid, &sid).await;
    assert_eq!(session["turns"], 4, "{session}");
    assert_eq!(
        session["result"],
        json!({"summary":"resumed draft"}),
        "{session}"
    );
    assert!(outputs(&fixture, &wid, &sid)
        .await
        .contains(&("summary.txt".to_owned(), 4)));

    let turns = turns(&fixture, &wid, &sid);
    assert_eq!(turns.len(), 4, "{turns:?}");
    for (number, (prompt, _)) in turns.iter().enumerate() {
        assert!(
            prompt.contains(REQUESTED) && prompt.contains(MANIFEST),
            "turn {} reached the harness without the requested result: {prompt}",
            number + 1
        );
    }
    let (_, echo) = &turns[2];
    assert!(
        echo.contains("Tighten the summary") && echo.contains(MANIFEST),
        "the harness was not told to rewrite the manifest: {echo}"
    );
    assert_eq!(
        typed(&fixture, &wid, &sid).await,
        [
            json!(collects("first draft")),
            json!(collects("second draft")),
            json!("Tighten the summary"),
            json!(collects("resumed draft")),
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_follow_up_after_a_turn_without_outputs_is_told_to_write_the_manifest() {
    let fixture = Fixture::new(true);
    let wid = fixture.workspace().await;
    let (status, started) = fixture
        .json(
            Method::POST,
            &format!("/v1/workspaces/{wid}/sessions"),
            OWNER,
            Some("answerer"),
            json!({"name":"Answerer","prompt":"What do the figures say?","harness":HARNESS,
                "access":"writer","result_contract":outputs_contract()}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let first = fixture.finished(started["id"].as_str().unwrap());
    assert_eq!(first.state, OperationState::Failed, "{first:?}");
    let sid = started["session_id"].as_str().unwrap();
    let missing = "open capture directory outputs";
    let session = view(&fixture, &wid, sid).await;
    assert!(
        session["status_detail"].to_string().contains(missing),
        "{session}"
    );

    let retold = follow_up(
        &fixture,
        &wid,
        sid,
        "answerer-retold",
        json!({"prompt":"Put the answer in a file."}),
    )
    .await;
    assert_eq!(retold.state, OperationState::Failed, "{retold:?}");
    let session = view(&fixture, &wid, sid).await;
    assert!(
        session["status_detail"].to_string().contains(missing),
        "{session}"
    );
    let (prompt, echo) = &turns(&fixture, &wid, sid)[1];
    assert!(
        prompt.contains(MANIFEST) && echo.contains(MANIFEST),
        "the follow-up's harness was not told to write the manifest: {prompt}"
    );

    let written = follow_up(
        &fixture,
        &wid,
        sid,
        "answerer-written",
        json!({"prompt":collects("in a file")}),
    )
    .await;
    assert_eq!(written.state, OperationState::Succeeded, "{written:?}");
    let session = view(&fixture, &wid, sid).await;
    assert_eq!(session["status"], "ready", "{session}");
    assert_eq!(
        session["result"],
        json!({"summary":"in a file"}),
        "{session}"
    );
    assert_eq!(
        outputs(&fixture, &wid, sid).await,
        [("summary.txt".to_owned(), 3)]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_messages_response_shows_what_the_person_typed_and_native_events_keep_the_rest() {
    let fixture = Fixture::new(true);
    let wid = fixture.workspace().await;
    let source = super::inputs::upload(
        &fixture,
        &wid,
        "typed-source",
        "figures.csv",
        b"option,annual_net\nA,15000\n",
    )
    .await;
    let first = format!("Summarise the figures.\n{}", collects("from figures"));
    let writer = start(
        &fixture,
        &wid,
        "typed-writer",
        json!({"name":"Writer","prompt":first,"harness":HARNESS,"access":"writer",
            "sources":[source["id"]],"result_contract":outputs_contract()}),
    )
    .await;
    let again = "Shorter, please.";
    follow_up(
        &fixture,
        &wid,
        &writer,
        "typed-writer-again",
        json!({"prompt":again}),
    )
    .await;
    assert_eq!(
        typed(&fixture, &wid, &writer).await,
        [json!(first), json!(again)]
    );
    let writer_turns = turns(&fixture, &wid, &writer);
    let index = format!(".console-inputs/{wid}/{writer}/INDEX.md");
    assert!(
        writer_turns[0].0.starts_with(&first)
            && writer_turns[0].0.contains(&index)
            && writer_turns[0].0.contains(MANIFEST),
        "{writer_turns:?}"
    );
    assert!(
        writer_turns[1].0.starts_with(again) && writer_turns[1].0.contains(MANIFEST),
        "{writer_turns:?}"
    );
    assert!(!writer_turns[1].0.contains(&index), "{writer_turns:?}");

    let (status, listed) = fixture
        .json(
            Method::GET,
            &format!("/v1/workspaces/{wid}/artifacts"),
            OWNER,
            None,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let artifact = listed["artifacts"][0]["id"].clone();
    let review = "Check the summary for unsupported claims.";
    let reviewer = start(
        &fixture,
        &wid,
        "typed-reviewer",
        json!({"name":"Reviewer","prompt":review,"harness":HARNESS,"access":"reviewer",
            "artifacts":[artifact]}),
    )
    .await;
    let question = "Which claim is weakest?";
    follow_up(
        &fixture,
        &wid,
        &reviewer,
        "typed-reviewer-again",
        json!({"prompt":question}),
    )
    .await;
    assert_eq!(
        typed(&fixture, &wid, &reviewer).await,
        [json!(review), json!(question)]
    );
    let reviewer_turns = turns(&fixture, &wid, &reviewer);
    let review_index = format!(".console-inputs/{wid}/{reviewer}/INDEX.md");
    assert!(
        reviewer_turns[0]
            .0
            .starts_with("Review the frozen candidate and input artifacts.")
            && reviewer_turns[0].0.contains(review)
            && reviewer_turns[0].0.contains(&review_index),
        "{reviewer_turns:?}"
    );
    assert_eq!(reviewer_turns[1].0, question);
}
